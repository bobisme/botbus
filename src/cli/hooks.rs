//! Channel hooks — trigger commands when messages are sent to channels.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use colored::Colorize;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{info, instrument, warn};

use super::OutputFormat;
use crate::core::claim::FileClaim;
use crate::core::flags::HookFlags;
use crate::core::hook::{
    ClaimRelease, Hook, HookCondition, HookFiring, QueuedTrigger, SpawnLease, shell_display,
};
use crate::core::message::{Message, MessageMeta, SystemEvent};
use crate::core::presence::{self, PRESENCE_TTL_SECS};
use crate::core::project::{
    channel_path, claims_path, hook_queue_path, hooks_audit_path, hooks_path,
};
use crate::storage::jsonl::{append_if, append_record, read_records};

/// Parse a cooldown duration string (e.g., "30s", "5m", "1h").
/// Returns seconds. Defaults to seconds if no unit.
fn parse_cooldown(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("Empty cooldown value");
    }

    let last = s.chars().last().unwrap();
    if last.is_ascii_digit() {
        // No unit — assume seconds
        return s.parse::<u64>().context("Invalid cooldown number");
    }

    let number_part = &s[..s.len() - 1];
    let value: u64 = number_part.parse().context("Invalid cooldown number")?;

    match last {
        's' => Ok(value),
        'm' => Ok(value * 60),
        'h' => Ok(value * 3600),
        _ => bail!("Unknown cooldown unit '{}'. Use s, m, or h.", last),
    }
}

/// Add a new hook.
#[allow(clippy::too_many_arguments)]
pub fn add(
    channel: Option<String>,
    claim: Option<String>,
    mention: Option<String>,
    cwd: PathBuf,
    cooldown: Option<String>,
    command: Vec<String>,
    ttl: Option<u64>,
    release_on_exit: bool,
    claim_owner: Option<String>,
    priority: i32,
    require_flag: Option<String>,
    description: Option<String>,
    lease: bool,
    lease_ttl: Option<u64>,
    max_batch: Option<usize>,
    name: Option<String>,
    owner: Option<String>,
    agent: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    if command.is_empty() {
        bail!(
            "Command is required. Use -- before the command, e.g.:\n  rite hooks add --channel ch --claim pattern --cwd /tmp --release-on-exit -- echo hello"
        );
    }

    // Determine which condition type to use
    let condition = match (claim.as_ref(), mention.as_ref()) {
        (Some(pattern), None) => {
            // Claim-only hooks require explicit channel
            if channel.is_none() {
                bail!("Claim-based hooks require --channel to be specified");
            }
            HookCondition::ClaimAvailable {
                pattern: pattern.clone(),
            }
        }
        (Some(_), Some(agent_name)) => {
            // Mention + claim: fires on @mention, acquires claim atomically
            if channel.is_none() {
                bail!("Hooks with --claim require --channel to be specified");
            }
            // Condition is MentionReceived; claim pattern stored in hook.claim_pattern
            HookCondition::MentionReceived {
                agent: agent_name
                    .strip_prefix('@')
                    .unwrap_or(agent_name)
                    .to_string(),
            }
        }
        (None, Some(agent_name)) => HookCondition::MentionReceived {
            agent: agent_name
                .strip_prefix('@')
                .unwrap_or(agent_name)
                .to_string(),
        },
        (None, None) => bail!("Must specify either --claim or --mention"),
    };

    // Default channel to "*" (all non-DM channels) if not specified
    let hook_channel = channel.unwrap_or_else(|| "*".to_string());

    // Validate claim release strategy
    // Required for ClaimAvailable hooks; required when --claim is used with --mention
    let has_claim = claim.is_some();
    if has_claim && ttl.is_none() && !release_on_exit {
        bail!("Must specify either --ttl <seconds> or --release-on-exit for claim acquisition");
    }

    // Validate cwd exists and is a directory
    if !cwd.exists() {
        bail!("Working directory does not exist: {}", cwd.display());
    }
    if !cwd.is_dir() {
        bail!("Working directory is not a directory: {}", cwd.display());
    }

    let cooldown_secs = match cooldown {
        Some(ref s) => parse_cooldown(s)?,
        None => 30,
    };

    // Load existing hooks to check for ID collisions
    let existing_hooks: Vec<Hook> = read_records(&hooks_path()).unwrap_or_default();
    let active_hooks = build_active_hooks(&existing_hooks);
    let existing_ids: Vec<String> = active_hooks.values().map(|h| h.id.clone()).collect();

    // A named hook that already exists on this channel is converged, not
    // duplicated. Preserving the record — and therefore the ID — is the whole
    // point: the ID is the spawn-lease key, so remove-and-add leaves a
    // running spawn holding a lease nobody checks any more.
    if let Some(ref hook_name) = name
        && let Some(existing) = active_hooks
            .values()
            .find(|h| h.name.as_deref() == Some(hook_name.as_str()) && h.channel == hook_channel)
    {
        let mut updated = (*existing).clone();
        let edit = HookEdit {
            channel: None,
            claim: claim.clone(),
            mention: mention.clone(),
            cwd: Some(cwd),
            cooldown,
            command,
            ttl,
            release_on_exit,
            claim_owner,
            // `--priority` carries a clap default, so it cannot say "leave it
            // alone". Treat the default as unspecified on the converge path.
            priority: (priority != 0).then_some(priority),
            require_flag,
            description,
            // Absent `--lease` means "this caller said nothing about
            // leasing", never "turn leasing off". A converge that silently
            // dropped the lease is exactly the bn-20eh failure. Use
            // `hooks set --no-lease` to turn one off deliberately.
            lease,
            no_lease: false,
            lease_ttl,
            max_batch,
            name: None,
            owner,
        };
        let changed = apply_edit(&mut updated, &edit)?;
        append_record(&hooks_path(), &updated).context("Failed to update hook")?;

        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&updated)?),
            OutputFormat::Pretty | OutputFormat::Text => {
                println!(
                    "{} Hook {} updated ({})",
                    "Converged:".green(),
                    updated.id.cyan(),
                    hook_name
                );
                for line in &changed {
                    println!("  {line}");
                }
            }
        }
        return Ok(());
    }

    // Set claim_release when --claim is used (required for claim hooks, optional otherwise)
    let claim_release = if has_claim {
        if let Some(secs) = ttl {
            Some(ClaimRelease::Ttl { secs })
        } else {
            Some(ClaimRelease::OnExit)
        }
    } else {
        None
    };

    // For mention+claim hooks, store the explicit claim pattern
    let claim_pattern = if matches!(condition, HookCondition::MentionReceived { .. }) {
        claim
    } else {
        None
    };

    let lease = lease.then_some(SpawnLease {
        ttl_secs: lease_ttl,
        max_batch,
        extra: Default::default(),
    });

    let hook = Hook {
        id: Hook::generate_id(&existing_ids),
        channel: hook_channel.clone(),
        condition,
        command,
        cwd,
        cooldown_secs,
        last_fired: None,
        created_at: Utc::now(),
        created_by: agent.map(|s| s.to_string()),
        claim_release,
        claim_pattern,
        claim_owner,
        priority,
        require_flag: require_flag.map(|f| f.to_lowercase()),
        lease,
        active: true,
        description,
        name,
        owner,
        extra: Default::default(),
    };

    append_record(&hooks_path(), &hook).context("Failed to save hook")?;

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&hook)?);
        }
        OutputFormat::Pretty | OutputFormat::Text => {
            println!("{} Hook {} created", "Added:".green(), hook.id.cyan());
            println!("  channel: #{}", hook.channel);
            println!("  condition: {:?}", hook.condition);
            println!("  command: {:?}", hook.command);
            if hook.uses_lease() {
                println!(
                    "  lease: {} (ttl: {}s, max-batch: {})",
                    hook.lease_pattern(&hook.channel),
                    hook.lease_ttl_secs(),
                    hook.lease_max_batch()
                );
                if hook.claim_owner.is_none() {
                    // Presence is what tells a wedged lease from a working
                    // one, and presence is per agent — so a lease owned by
                    // whoever happened to send the message tracks the sender,
                    // not the spawn. Say so rather than let it surprise.
                    println!(
                        "  {}",
                        "note: without --claim-owner the lease is owned by the triggering sender; \
                         a stuck lease then only clears at its TTL"
                            .yellow()
                    );
                }
            } else {
                println!(
                    "  cooldown: {}s (deprecated — prefer --lease)",
                    hook.cooldown_secs
                );
            }
        }
    }

    Ok(())
}

/// Output struct for hook listing.
#[derive(Debug, Serialize)]
struct HookInfo {
    id: String,
    channel: String,
    condition: HookCondition,
    command: Vec<String>,
    cwd: String,
    cooldown_secs: u64,
    priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    require_flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    last_fired: Option<String>,
    /// Spawn lease config, when the hook uses one instead of `cooldown_secs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    lease: Option<SpawnLease>,
    /// Triggers queued behind this hook's lease, waiting for the next spawn.
    pending: usize,
    active: bool,
}

#[derive(Debug, Serialize)]
struct HooksOutput {
    hooks: Vec<HookInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    advice: Vec<String>,
}

/// List all active hooks.
pub fn list(owner: Option<&str>, format: OutputFormat) -> Result<()> {
    let all_hooks: Vec<Hook> = read_records(&hooks_path()).unwrap_or_default();
    let active = build_active_hooks(&all_hooks);

    let mut hooks: Vec<&Hook> = active
        .values()
        .filter(|h| owner.is_none_or(|want| h.owner.as_deref() == Some(want)))
        .collect();
    hooks.sort_by_key(|h| &h.created_at);

    // Pending counts make a wedged lease visible: a hook whose queue only
    // grows is a channel that has stopped spawning.
    let mut pending_counts: HashMap<String, usize> = HashMap::new();
    for entry in pending_triggers() {
        *pending_counts.entry(entry.hook_id).or_default() += 1;
    }

    let infos: Vec<HookInfo> = hooks
        .iter()
        .map(|h| HookInfo {
            id: h.id.clone(),
            channel: h.channel.clone(),
            condition: h.condition.clone(),
            command: h.command.clone(),
            cwd: h.cwd.to_string_lossy().to_string(),
            cooldown_secs: h.cooldown_secs,
            priority: h.priority,
            require_flag: h.require_flag.clone(),
            description: h.description.clone(),
            name: h.name.clone(),
            owner: h.owner.clone(),
            last_fired: h.last_fired.map(|t| t.to_rfc3339()),
            lease: h.lease.clone(),
            pending: *pending_counts.get(&h.id).unwrap_or(&0),
            active: h.active,
        })
        .collect();

    match format {
        OutputFormat::Json => {
            let output = HooksOutput {
                hooks: infos,
                advice: vec![], // Informational command, no specific next action
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Pretty => {
            if hooks.is_empty() {
                println!("No active hooks.");
            } else {
                println!("{}", "Hooks:".bold());
                for h in &hooks {
                    let last = h
                        .last_fired
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "never".to_string());
                    let throttle = if h.uses_lease() {
                        format!("lease: {}s", h.lease_ttl_secs())
                    } else {
                        format!("cooldown: {}s", h.cooldown_secs)
                    };
                    println!(
                        "  {} #{} → {:?} (priority: {}, {}, last: {})",
                        h.id.cyan(),
                        h.channel,
                        h.command,
                        h.priority,
                        throttle,
                        last.dimmed()
                    );
                    if h.uses_lease() {
                        let pending = *pending_counts.get(&h.id).unwrap_or(&0);
                        println!(
                            "    lease: {} (max-batch: {}, queued: {})",
                            h.lease_pattern(&h.channel),
                            h.lease_max_batch(),
                            pending
                        );
                    }
                    match &h.condition {
                        HookCondition::ClaimAvailable { pattern } => {
                            println!("    if-claim-available: {}", pattern);
                        }
                        HookCondition::MentionReceived { agent } => {
                            println!("    if-mention-received: @{}", agent);
                        }
                    }
                    if let Some(ref owner) = h.claim_owner {
                        println!("    claim-owner: {}", owner);
                    }
                    if let Some(ref flag) = h.require_flag {
                        println!("    require-flag: !{}", flag);
                    }
                    if let Some(ref desc) = h.description {
                        println!("    description: {}", desc);
                    }
                }
            }
        }
        OutputFormat::Text => {
            for h in &hooks {
                let event = match &h.condition {
                    HookCondition::ClaimAvailable { .. } => "claim-available",
                    HookCondition::MentionReceived { .. } => "mention",
                };
                let command_str = shell_display(&h.command);
                let desc_str = h.description.as_deref().unwrap_or("");
                if desc_str.is_empty() {
                    println!("{}  {}  {}  {}", h.id, h.channel, event, command_str);
                } else {
                    println!(
                        "{}  {}  {}  {}  {}",
                        h.id, h.channel, event, command_str, desc_str
                    );
                }
            }
        }
    }

    Ok(())
}

/// What a single `hooks set` invocation changes. Every field is optional:
/// `None` means "leave whatever the hook already has".
///
/// Bundled into a struct rather than passed as fifteen positional arguments
/// so a caller cannot silently transpose two of them.
#[derive(Debug, Default)]
pub struct HookEdit {
    pub channel: Option<String>,
    pub claim: Option<String>,
    pub mention: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cooldown: Option<String>,
    pub command: Vec<String>,
    pub ttl: Option<u64>,
    pub release_on_exit: bool,
    pub claim_owner: Option<String>,
    pub priority: Option<i32>,
    pub require_flag: Option<String>,
    pub description: Option<String>,
    pub lease: bool,
    pub no_lease: bool,
    pub lease_ttl: Option<u64>,
    pub max_batch: Option<usize>,
    pub name: Option<String>,
    pub owner: Option<String>,
}

impl HookEdit {
    /// Whether this edit asks for anything at all.
    ///
    /// An empty edit is rejected rather than appended: a no-op record would
    /// still be a new latest-wins entry, which is noise in an append-only
    /// log and makes `hooks set` look like it did something.
    fn is_empty(&self) -> bool {
        self.channel.is_none()
            && self.claim.is_none()
            && self.mention.is_none()
            && self.cwd.is_none()
            && self.cooldown.is_none()
            && self.command.is_empty()
            && self.ttl.is_none()
            && !self.release_on_exit
            && self.claim_owner.is_none()
            && self.priority.is_none()
            && self.require_flag.is_none()
            && self.description.is_none()
            && !self.lease
            && !self.no_lease
            && self.lease_ttl.is_none()
            && self.max_batch.is_none()
            && self.name.is_none()
            && self.owner.is_none()
    }
}

/// Apply `edit` to `hook` in place, returning a human-readable list of what
/// changed.
///
/// Split out from [`set`] so the merge rules are testable without touching
/// the filesystem. Anything the edit does not mention is left exactly as it
/// was — including `id`, `created_at`, `last_fired`, and `extra`, which is
/// what makes this an update rather than a replacement.
fn apply_edit(hook: &mut Hook, edit: &HookEdit) -> Result<Vec<String>> {
    let mut changed = Vec::new();

    if let Some(channel) = &edit.channel {
        hook.channel = channel.clone();
        changed.push(format!("channel -> #{channel}"));
    }

    // Condition. --mention wins the condition slot; a --claim alongside it
    // becomes the explicit claim pattern, matching `hooks add`.
    match (&edit.claim, &edit.mention) {
        (_, Some(agent)) => {
            let agent = agent.strip_prefix('@').unwrap_or(agent).to_string();
            hook.condition = HookCondition::MentionReceived {
                agent: agent.clone(),
            };
            hook.claim_pattern = edit.claim.clone();
            changed.push(format!("condition -> mention @{agent}"));
            if let Some(pattern) = &edit.claim {
                changed.push(format!("claim pattern -> {pattern}"));
            }
        }
        (Some(pattern), None) => {
            hook.condition = HookCondition::ClaimAvailable {
                pattern: pattern.clone(),
            };
            hook.claim_pattern = None;
            changed.push(format!("condition -> claim available {pattern}"));
        }
        (None, None) => {}
    }

    if let Some(cwd) = &edit.cwd {
        // The whole reason this command exists is repointing a hook whose cwd
        // vanished, so a typo here must not be accepted quietly.
        if !cwd.exists() {
            bail!("Working directory does not exist: {}", cwd.display());
        }
        if !cwd.is_dir() {
            bail!("Working directory is not a directory: {}", cwd.display());
        }
        hook.cwd = cwd.clone();
        changed.push(format!("cwd -> {}", cwd.display()));
    }

    if !edit.command.is_empty() {
        hook.command = edit.command.clone();
        changed.push(format!("command -> {:?}", hook.command));
    }

    if let Some(cooldown) = &edit.cooldown {
        hook.cooldown_secs = parse_cooldown(cooldown)?;
        changed.push(format!("cooldown -> {}s", hook.cooldown_secs));
    }

    if edit.release_on_exit {
        hook.claim_release = Some(ClaimRelease::OnExit);
        changed.push("claim release -> on exit".to_string());
    } else if let Some(secs) = edit.ttl {
        hook.claim_release = Some(ClaimRelease::Ttl { secs });
        changed.push(format!("claim release -> ttl {secs}s"));
    }

    if let Some(owner) = &edit.claim_owner {
        hook.claim_owner = Some(owner.clone());
        changed.push(format!("claim owner -> {owner}"));
    }

    if let Some(priority) = edit.priority {
        hook.priority = priority;
        changed.push(format!("priority -> {priority}"));
    }

    if let Some(flag) = &edit.require_flag {
        hook.require_flag = Some(flag.to_lowercase());
        changed.push(format!("require flag -> !{}", flag.to_lowercase()));
    }

    if let Some(description) = &edit.description {
        hook.description = Some(description.clone());
        changed.push(format!("description -> {description}"));
    }

    if let Some(name) = &edit.name {
        hook.name = Some(name.clone());
        changed.push(format!("name -> {name}"));
    }

    if let Some(owner) = &edit.owner {
        hook.owner = Some(owner.clone());
        changed.push(format!("owner -> {owner}"));
    }

    // Lease. --no-lease clears it; --lease turns it on; --lease-ttl and
    // --max-batch tune an existing one without having to re-state --lease.
    if edit.no_lease {
        if hook.lease.take().is_some() {
            changed.push(format!(
                "lease -> off (cooldown {}s applies again)",
                hook.cooldown_secs
            ));
        }
    } else if edit.lease || edit.lease_ttl.is_some() || edit.max_batch.is_some() {
        let existing = hook.lease.clone().unwrap_or_default();
        let was_leased = hook.lease.is_some();
        hook.lease = Some(SpawnLease {
            // An unspecified knob keeps the value the lease already had, so
            // `--lease-ttl` alone does not silently reset `--max-batch`.
            ttl_secs: edit.lease_ttl.or(existing.ttl_secs),
            max_batch: edit.max_batch.or(existing.max_batch),
            extra: existing.extra,
        });
        if was_leased {
            changed.push(format!(
                "lease -> ttl {}s, max-batch {}",
                hook.lease_ttl_secs(),
                hook.lease_max_batch()
            ));
        } else {
            changed.push(format!(
                "lease -> on (ttl {}s, max-batch {}; cooldown now ignored)",
                hook.lease_ttl_secs(),
                hook.lease_max_batch()
            ));
        }
    }

    Ok(changed)
}

/// Update an existing hook in place, preserving its ID.
///
/// hooks.jsonl is append-only with latest-record-wins, so this appends an
/// amended copy rather than mutating anything. Keeping the ID is the point:
/// a hook's ID is its spawn-lease key (`spawn://<id>/<channel>`), so
/// remove-then-add hands a running spawn's lease to nobody and lets the
/// replacement spawn a second agent alongside it.
pub fn set(hook_id: String, edit: &HookEdit, format: OutputFormat) -> Result<()> {
    if edit.is_empty() {
        bail!(
            "Nothing to change. Pass at least one field, e.g.:\n  rite hooks set {hook_id} --cwd /path/to/project"
        );
    }

    let all_hooks: Vec<Hook> = read_records(&hooks_path()).unwrap_or_default();
    let active = build_active_hooks(&all_hooks);

    let hook = active
        .get(&hook_id)
        .ok_or_else(|| anyhow::anyhow!("Hook not found: {}", hook_id))?;

    // Clone the stored record so every field this build does not know about
    // rides along untouched. See `UnknownFields` / bn-14o5.
    let mut updated = hook.clone();
    let changed = apply_edit(&mut updated, edit)?;

    append_record(&hooks_path(), &updated).context("Failed to update hook")?;

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&updated)?);
        }
        OutputFormat::Pretty | OutputFormat::Text => {
            println!("{} Hook {} updated", "Updated:".green(), hook_id.cyan());
            for line in &changed {
                println!("  {line}");
            }
            if updated.uses_lease() {
                println!(
                    "  lease pattern: {}",
                    updated.lease_pattern(&updated.channel)
                );
            }
        }
    }

    Ok(())
}

/// Remove (deactivate) a hook by ID.
pub fn remove(hook_id: String, format: OutputFormat) -> Result<()> {
    let all_hooks: Vec<Hook> = read_records(&hooks_path()).unwrap_or_default();
    let active = build_active_hooks(&all_hooks);

    let hook = active
        .get(&hook_id)
        .ok_or_else(|| anyhow::anyhow!("Hook not found: {}", hook_id))?;

    // Append a deactivated copy
    let mut deactivated = hook.clone();
    deactivated.active = false;

    append_record(&hooks_path(), &deactivated).context("Failed to deactivate hook")?;

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "removed": hook_id
                }))?
            );
        }
        OutputFormat::Pretty | OutputFormat::Text => {
            println!("{} Hook {} removed", "Removed:".green(), hook_id.cyan());
        }
    }

    Ok(())
}

/// Rename a channel in all hooks that reference it.
/// Returns the count of hooks that were updated.
pub fn rename_channel_in_hooks(old_name: &str, new_name: &str) -> Result<usize> {
    let all_hooks: Vec<Hook> = read_records(&hooks_path()).unwrap_or_default();

    // Find hooks that need updating (only active hooks with matching channel)
    let active = build_active_hooks(&all_hooks);
    let hooks_to_update: Vec<Hook> = active
        .values()
        .filter(|h| h.channel == old_name)
        .cloned()
        .collect();

    let update_count = hooks_to_update.len();

    // If no hooks need updating, return early
    if update_count == 0 {
        return Ok(0);
    }

    // Append updated versions with new channel name
    for mut hook in hooks_to_update {
        hook.channel = new_name.to_string();
        append_record(&hooks_path(), &hook).context("Failed to update hook")?;
    }

    Ok(update_count)
}

/// Dry-run test of a hook — evaluate condition without executing.
pub fn test(hook_id: String, format: OutputFormat) -> Result<()> {
    let all_hooks: Vec<Hook> = read_records(&hooks_path()).unwrap_or_default();
    let active = build_active_hooks(&all_hooks);

    let hook = active
        .get(&hook_id)
        .ok_or_else(|| anyhow::anyhow!("Hook not found: {}", hook_id))?;

    let now = Utc::now();

    // Cooldown only gates hooks without a lease.
    let cooldown_ok = if hook.uses_lease() {
        true
    } else {
        match hook.last_fired {
            Some(last) => (now - last).num_seconds() >= hook.cooldown_secs as i64,
            None => true,
        }
    };

    // Lease state for the hook's own channel. A wildcard hook leases per
    // firing channel, so this is the template rather than the only lease.
    let (lease_pattern, lease_free, pending) = if hook.uses_lease() {
        let pattern = hook.lease_pattern(&hook.channel);
        let claims: Vec<FileClaim> = read_records(&claims_path()).unwrap_or_default();
        let free = lease_available(&pattern, &claims, now);
        let pending = pending_for(&hook.id, &hook.channel).len();
        (Some(pattern), Some(free), pending)
    } else {
        (None, None, 0)
    };

    // Evaluate condition (MentionReceived hooks will always return false in test mode)
    let condition_result = evaluate_condition(&hook.condition, &[])?;

    let would_execute = cooldown_ok && condition_result && lease_free.unwrap_or(true);

    let reason = if !cooldown_ok {
        Some("cooldown active".to_string())
    } else if lease_free == Some(false) {
        Some("spawn lease held".to_string())
    } else if !condition_result {
        Some("condition not met".to_string())
    } else {
        None
    };

    #[derive(Serialize)]
    struct TestResult {
        hook_id: String,
        cooldown_ok: bool,
        condition_result: bool,
        would_execute: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        lease_pattern: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        lease_free: Option<bool>,
        pending: usize,
        command: Vec<String>,
        cwd: String,
    }

    let result = TestResult {
        hook_id: hook.id.clone(),
        cooldown_ok,
        condition_result,
        would_execute,
        reason,
        lease_pattern,
        lease_free,
        pending,
        command: hook.command.clone(),
        cwd: hook.cwd.to_string_lossy().to_string(),
    };

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutputFormat::Pretty | OutputFormat::Text => {
            println!("{} Hook {} dry-run:", "Test:".green(), hook.id.cyan());
            if let Some(pattern) = &result.lease_pattern {
                println!(
                    "  lease {}: {} (queued: {})",
                    pattern,
                    if result.lease_free == Some(true) {
                        "free".green().to_string()
                    } else {
                        "held (skipped, trigger would be queued)".red().to_string()
                    },
                    result.pending
                );
            } else {
                println!(
                    "  cooldown: {}",
                    if cooldown_ok {
                        "ok".green().to_string()
                    } else {
                        "active (skipped)".red().to_string()
                    }
                );
            }
            println!(
                "  condition: {}",
                if condition_result {
                    "passed".green().to_string()
                } else {
                    "failed".red().to_string()
                }
            );
            println!(
                "  would execute: {}",
                if would_execute {
                    "yes".green().to_string()
                } else {
                    "no".red().to_string()
                }
            );
            println!("  command: {:?}", hook.command);
            println!("  cwd: {}", hook.cwd.display());
        }
    }

    Ok(())
}

/// Build a map of active hooks (latest state per ID wins).
pub(crate) fn build_active_hooks(all_hooks: &[Hook]) -> HashMap<String, Hook> {
    let mut map: HashMap<String, Hook> = HashMap::new();
    for hook in all_hooks {
        map.insert(hook.id.clone(), hook.clone());
    }
    // Remove inactive hooks
    map.retain(|_, h| h.active);
    map
}

/// Check if a pattern is held by any active claim in the given claims list.
/// This properly deduplicates claims by ID (latest state wins) before checking.
fn is_pattern_held(pattern: &str, existing_claims: &[FileClaim], now: DateTime<Utc>) -> bool {
    // Build active claims map (latest state per ID wins)
    let mut active: HashMap<ulid::Ulid, &FileClaim> = HashMap::new();
    for claim in existing_claims {
        active.insert(claim.id, claim);
    }

    // Check if ANY active, non-expired claim holds this exact pattern
    active.values().any(|claim| {
        claim.active && claim.expires_at > now && claim.patterns.iter().any(|p| p == pattern)
    })
}

/// Check if a claim pattern has NO active holder.
/// Returns true if the pattern is available (no one holds it).
fn is_claim_available(pattern: &str) -> Result<bool> {
    let all_claims: Vec<FileClaim> = read_records(&claims_path()).unwrap_or_default();
    let now = Utc::now();
    Ok(!is_pattern_held(pattern, &all_claims, now))
}

/// Hard cap on queued-but-undelivered triggers per (hook, channel).
///
/// A queue that grows without bound is its own failure mode: it would hand a
/// returning agent a batch it can never work through, and it would grow
/// `hook_queue.jsonl` forever while the channel is wedged. Past the cap new
/// triggers are refused and audited, so the drop is visible instead of silent.
const MAX_PENDING_PER_KEY: usize = 500;

// ---------------------------------------------------------------------------
// Spawn lease (bn-fsx0)
//
// `cooldown_secs` asks a wall clock a question it cannot answer: "is a spawn
// from this hook still running?" The lease answers it directly. It is an
// ordinary claim on a rite-owned pattern (`spawn://<hook>/<channel>`), staked
// through the same atomic check-and-stake that hook claims already use, and
// held by the agent the hook spawns.
//
// The failure mode that matters is a lease that is never released — the agent
// is killed, the machine reboots — because a hook that stops firing forever is
// far worse than one that fires twice. Two independent guards close that:
//
//   1. Presence (bn-12i6). A lease whose holder's heartbeat has lapsed is
//      *superseded*: the next trigger stakes its own lease on the same
//      pattern and proceeds. Nothing releases, expires, or rewrites the dead
//      holder's claim — it stays in `claims.jsonl` exactly as written, still
//      `active`, still reported (and now reported `stale`) by `claims list`.
//      Staleness stays a report; what changes is only that *this* code
//      declines to be blocked by a report of a dead agent, within a scheme it
//      owns. Superseding requires the lease to be at least one presence TTL
//      old, so an agent that simply has not checked in yet cannot have its
//      lease taken the instant it is granted.
//
//   2. TTL. A holder that never recorded a heartbeat at all is `Unknown`, not
//      `Lapsed`, and presence deliberately refuses to call that stale — so the
//      lease's own expiry is the backstop that bounds the wedge.
// ---------------------------------------------------------------------------

/// Whether a claim on a lease pattern still blocks a new spawn.
///
/// `false` when the holder is provably gone: its heartbeat has lapsed *and*
/// the lease has been held long enough that a live holder would have checked
/// in at least once.
fn lease_blocks(claim: &FileClaim, now: DateTime<Utc>) -> bool {
    let age_secs = now.signed_duration_since(claim.ts).num_seconds();
    if age_secs < PRESENCE_TTL_SECS {
        // Too young to judge — a freshly spawned agent may not have run a
        // single rite command yet, and its last heartbeat could be from a
        // previous life.
        return true;
    }
    !presence::agent_presence(&claim.agent).is_stale()
}

/// Whether `pattern` is free for a new spawn lease.
fn lease_available(pattern: &str, existing_claims: &[FileClaim], now: DateTime<Utc>) -> bool {
    // Latest record per claim ID wins, same as `is_pattern_held`.
    let mut active: HashMap<ulid::Ulid, &FileClaim> = HashMap::new();
    for claim in existing_claims {
        active.insert(claim.id, claim);
    }

    !active.values().any(|claim| {
        claim.active
            && claim.expires_at > now
            && claim.patterns.iter().any(|p| p == pattern)
            && lease_blocks(claim, now)
    })
}

/// Atomically take the spawn lease for a (hook, channel), or report it held.
fn acquire_lease(pattern: &str, owner: &str, ttl_secs: u64) -> Option<FileClaim> {
    let claim = FileClaim::with_message(
        owner,
        vec![pattern.to_string()],
        ttl_secs,
        Some("hook spawn lease".to_string()),
    );
    let pattern = pattern.to_string();
    let acquired = append_if(&claims_path(), &claim, |existing| {
        lease_available(&pattern, existing, Utc::now())
    })
    .unwrap_or(false);

    acquired.then_some(claim)
}

/// Release a claim this evaluation staked itself.
fn release_own_claim(claim: Option<&FileClaim>) {
    if let Some(claim) = claim {
        let _ = append_record(&claims_path(), &claim.release());
    }
}

/// Stake a hook's own claim, atomically, exactly as before leases existed.
fn stake_hook_claim(pattern: &str, agent: &str, ttl_secs: u64) -> Option<FileClaim> {
    let claim = FileClaim::new(agent, vec![pattern.to_string()], ttl_secs);
    let pattern = pattern.to_string();
    let acquired = append_if(&claims_path(), &claim, |existing| {
        !is_pattern_held(&pattern, existing, Utc::now())
    })
    .unwrap_or(false);

    acquired.then_some(claim)
}

/// Identity two triggers must share to be collapsed into one: same sender,
/// same message body. Hashed so the queue file stays a fixed width whatever
/// the message length.
fn dedup_key(agent: &str, body: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(agent.as_bytes());
    hasher.update([0u8]);
    hasher.update(body.trim().as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// Read every queued-but-undelivered trigger, latest record per entry ID.
fn pending_triggers() -> Vec<QueuedTrigger> {
    let all: Vec<QueuedTrigger> = read_records(&hook_queue_path()).unwrap_or_default();
    let mut latest: HashMap<ulid::Ulid, QueuedTrigger> = HashMap::new();
    for entry in all {
        latest.insert(entry.id, entry);
    }
    let mut pending: Vec<QueuedTrigger> = latest.into_values().filter(|e| !e.delivered).collect();
    pending.sort_by_key(|e| (e.ts, e.id));
    pending
}

/// Queued-but-undelivered triggers for one (hook, channel), oldest first.
fn pending_for(hook_id: &str, channel: &str) -> Vec<QueuedTrigger> {
    pending_triggers()
        .into_iter()
        .filter(|e| e.hook_id == hook_id && e.channel == channel)
        .collect()
}

/// Remember a trigger that arrived while the lease was held.
///
/// Returns the reason it was *not* queued, if it was not.
fn enqueue_trigger(
    hook: &Hook,
    channel: &str,
    message_id: &str,
    sender: &str,
    body: &str,
    command_agent: &str,
) -> Option<&'static str> {
    // Never queue the spawned agent's own chatter behind its own lease:
    // that turns "reply in the channel you were spawned for" into a
    // self-sustaining spawn loop.
    if sender == command_agent {
        return Some("own message");
    }

    let pending = pending_for(&hook.id, channel);
    if pending.len() >= MAX_PENDING_PER_KEY {
        return Some("queue full");
    }

    let key = dedup_key(sender, body);
    if pending.iter().any(|e| e.dedup_key == key) {
        // Identical trigger already waiting — dedup at the door as well as at
        // delivery, so a flapping sender cannot fill the queue.
        return Some("duplicate");
    }

    let entry = QueuedTrigger::new(&hook.id, channel, message_id, sender, key);
    match append_record(&hook_queue_path(), &entry) {
        Ok(()) => None,
        Err(error) => {
            warn!(%error, hook_id = %hook.id, "failed to queue hook trigger");
            Some("queue write failed")
        }
    }
}

/// Pick the queued triggers a spawn should be handed alongside `message_id`.
///
/// Deduplicates against the triggering message and against each other, and
/// caps the batch — anything over the cap stays queued for the spawn after.
fn drain_batch(hook: &Hook, channel: &str, trigger_key: &str) -> Vec<QueuedTrigger> {
    let cap = hook.lease_max_batch().saturating_sub(1);
    if cap == 0 {
        return Vec::new();
    }

    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::from([trigger_key.to_string()]);
    let mut batch = Vec::new();

    for entry in pending_for(&hook.id, channel) {
        if !seen.insert(entry.dedup_key.clone()) {
            continue;
        }
        batch.push(entry);
        if batch.len() >= cap {
            break;
        }
    }

    batch
}

/// Mark queued triggers as handed to a spawn. Only ever called after the
/// spawn actually started, so a failed spawn leaves them queued.
fn mark_delivered(batch: &[QueuedTrigger]) {
    for entry in batch {
        let _ = append_record(&hook_queue_path(), &entry.delivered());
    }
}

/// Outcome of a hook's condition and claim acquisition.
enum HookGate {
    /// Fire, holding these claims.
    Ready {
        claim: Option<FileClaim>,
        claim_ttl: Option<u64>,
        claim_pattern: Option<String>,
    },
    /// Condition genuinely does not apply to this message (no mention).
    /// There is nothing to batch — the message was never this hook's work.
    ConditionFailed,
    /// The hook's own claim is held: an instance is already working. This is
    /// the case a lease-enabled hook batches instead of dropping.
    Busy {
        reason: &'static str,
        condition_result: bool,
    },
}

/// Evaluate a hook's condition and take its claim, unchanged from the
/// pre-lease behaviour — the same atomic check-and-stake, the same audit
/// reasons. Only the *classification* of the outcome is new.
fn evaluate_gate(hook: &Hook, agent: &str, mentions: &[String]) -> HookGate {
    match &hook.condition {
        HookCondition::ClaimAvailable { pattern } => {
            // Use claim_owner if specified, otherwise use message sender
            let claim_agent = hook.claim_owner.as_deref().unwrap_or(agent);

            match &hook.claim_release {
                Some(ClaimRelease::Ttl { secs }) => {
                    match stake_hook_claim(pattern, claim_agent, *secs) {
                        Some(claim) => HookGate::Ready {
                            claim: Some(claim),
                            claim_ttl: Some(*secs),
                            claim_pattern: Some(pattern.clone()),
                        },
                        None => HookGate::Busy {
                            reason: "claim unavailable (atomic check)",
                            condition_result: false,
                        },
                    }
                }
                Some(ClaimRelease::OnExit) => {
                    // Large sentinel TTL; released explicitly after the command exits
                    match stake_hook_claim(pattern, claim_agent, 86400) {
                        Some(claim) => HookGate::Ready {
                            claim: Some(claim),
                            claim_ttl: None,
                            claim_pattern: Some(pattern.clone()),
                        },
                        None => HookGate::Busy {
                            reason: "claim unavailable (atomic check)",
                            condition_result: false,
                        },
                    }
                }
                None => {
                    // No claim release strategy - just check availability without claiming
                    if is_claim_available(pattern).unwrap_or(false) {
                        HookGate::Ready {
                            claim: None,
                            claim_ttl: None,
                            claim_pattern: Some(pattern.clone()),
                        }
                    } else {
                        HookGate::Busy {
                            reason: "condition not met",
                            condition_result: false,
                        }
                    }
                }
            }
        }
        HookCondition::MentionReceived {
            agent: mention_agent,
        } => {
            if !mentions.iter().any(|m| m == mention_agent) {
                return HookGate::ConditionFailed;
            }

            // If hook has an explicit --claim pattern, acquire it atomically
            let (Some(pattern), Some(release)) = (&hook.claim_pattern, &hook.claim_release) else {
                // No claim — just fire on mention
                return HookGate::Ready {
                    claim: None,
                    claim_ttl: None,
                    claim_pattern: None,
                };
            };

            let claim_agent = hook.claim_owner.as_deref().unwrap_or(agent);
            let (ttl, reported_ttl) = match release {
                ClaimRelease::Ttl { secs } => (*secs, Some(*secs)),
                ClaimRelease::OnExit => (86400, None),
            };

            match stake_hook_claim(pattern, claim_agent, ttl) {
                Some(claim) => HookGate::Ready {
                    claim: Some(claim),
                    claim_ttl: reported_ttl,
                    claim_pattern: Some(pattern.clone()),
                },
                None => HookGate::Busy {
                    reason: "claim unavailable",
                    condition_result: true,
                },
            }
        }
    }
}

/// Evaluate a hook condition.
fn evaluate_condition(condition: &HookCondition, mentions: &[String]) -> Result<bool> {
    match condition {
        HookCondition::ClaimAvailable { pattern } => is_claim_available(pattern),
        HookCondition::MentionReceived { agent } => Ok(mentions.iter().any(|m| m == agent)),
    }
}

/// Result of a hook that fired during evaluation.
pub struct HookFireResult {
    pub hook_id: String,
    pub command_display: String,
    pub claim_pattern: Option<String>,
    pub claim_ttl: Option<u64>,
    /// Triggers handed to this spawn, including the message that fired it.
    /// Always 1 for hooks without a spawn lease.
    pub batch_count: usize,
}

/// Evaluate all hooks for a channel after a message is sent.
/// Returns info about hooks that fired (for caller to display).
#[instrument(skip(meta, mentions, body), fields(channel = channel, message_id = message_id, agent = agent))]
pub fn evaluate_hooks(
    channel: &str,
    message_id: &str,
    body: &str,
    meta: Option<&MessageMeta>,
    agent: &str,
    mentions: &[String],
) -> Vec<HookFireResult> {
    evaluate_hooks_with_flags(
        channel,
        message_id,
        body,
        meta,
        agent,
        mentions,
        &HookFlags::default(),
    )
}

/// Evaluate hooks with explicit flag control.
/// Flags can suppress channel hooks, mention hooks, or both.
///
/// `body` is the message text; it is only used to recognise duplicate
/// triggers when batching behind a spawn lease.
#[instrument(skip(meta, mentions, flags, body), fields(channel = channel, message_id = message_id, agent = agent))]
pub fn evaluate_hooks_with_flags(
    channel: &str,
    message_id: &str,
    body: &str,
    meta: Option<&MessageMeta>,
    agent: &str,
    mentions: &[String],
    flags: &HookFlags,
) -> Vec<HookFireResult> {
    match evaluate_hooks_inner(channel, message_id, body, meta, agent, mentions, flags) {
        Ok(results) => results,
        Err(error) => {
            warn!(%error, "hook evaluation failed");
            vec![]
        }
    }
}

fn evaluate_hooks_inner(
    channel: &str,
    message_id: &str,
    body: &str,
    meta: Option<&MessageMeta>,
    agent: &str,
    mentions: &[String],
    flags: &HookFlags,
) -> Result<Vec<HookFireResult>> {
    // Skip hook evaluation for system messages to prevent recursive loops
    if matches!(meta, Some(MessageMeta::System { .. })) {
        return Ok(vec![]);
    }

    let all_hooks: Vec<Hook> = read_records(&hooks_path()).unwrap_or_default();
    if all_hooks.is_empty() {
        return Ok(vec![]);
    }

    let active = build_active_hooks(&all_hooks);
    let now = Utc::now();
    let mut results = Vec::new();

    // Collect hooks into a vector and sort by priority (lower priority runs first)
    let mut hooks_to_process: Vec<&Hook> = active.values().collect();
    hooks_to_process.sort_by_key(|h| h.priority);

    for hook in hooks_to_process {
        // Check if hook is suppressed by flags
        let is_channel_hook = matches!(hook.condition, HookCondition::ClaimAvailable { .. });
        let is_mention_hook = matches!(hook.condition, HookCondition::MentionReceived { .. });

        if is_channel_hook && flags.suppress_channel_hooks() {
            continue;
        }
        if is_mention_hook && flags.suppress_mention_hooks() {
            continue;
        }

        // Check require_flag: if set, the message must contain the specified !flag
        if let Some(ref required) = hook.require_flag
            && !flags.has_custom_flag(required)
        {
            continue;
        }

        // Match hook channel: exact match OR wildcard "*" (except DMs)
        let channel_matches = if hook.channel == "*" {
            !crate::core::channel::is_dm_channel(channel)
        } else {
            hook.channel == channel
        };

        if !channel_matches {
            continue;
        }

        let command_agent = hook_command_agent(hook, agent).to_string();

        // Wall-clock cooldown — deprecated, and skipped entirely for hooks
        // that carry a lease, since the lease answers the same question
        // properly. Hooks without a lease (every hook written before leases
        // existed) go through exactly the path they always did.
        if !hook.uses_lease() {
            let cooldown_ok = match hook.last_fired {
                Some(last) => (now - last).num_seconds() >= hook.cooldown_secs as i64,
                None => true,
            };

            if !cooldown_ok {
                let firing = HookFiring {
                    ts: now,
                    hook_id: hook.id.clone(),
                    channel: channel.to_string(),
                    message_id: message_id.to_string(),
                    condition_result: false,
                    executed: false,
                    reason: Some("cooldown active".to_string()),
                };
                let _ = append_record(&hooks_audit_path(), &firing);
                continue;
            }
        }

        // Take the spawn lease before anything with side effects. Held means
        // a spawn for this (hook, channel) is still live: batch the trigger
        // for that spawn's successor rather than dropping or duplicating it.
        let lease = if hook.uses_lease() {
            let pattern = hook.lease_pattern(channel);
            match acquire_lease(&pattern, &command_agent, hook.lease_ttl_secs()) {
                Some(claim) => Some(claim),
                None => {
                    let not_queued =
                        enqueue_trigger(hook, channel, message_id, agent, body, &command_agent);
                    let reason = match not_queued {
                        None => "lease held (queued)".to_string(),
                        Some(why) => format!("lease held ({})", why),
                    };
                    let firing = HookFiring {
                        ts: now,
                        hook_id: hook.id.clone(),
                        channel: channel.to_string(),
                        message_id: message_id.to_string(),
                        condition_result: false,
                        executed: false,
                        reason: Some(reason),
                    };
                    let _ = append_record(&hooks_audit_path(), &firing);
                    continue;
                }
            }
        } else {
            None
        };

        // Condition + the hook's own claim. Unchanged semantics; the lease
        // above is what decides whether a *spawn* may start at all.
        let (claim, claim_ttl, claim_pattern) = match evaluate_gate(hook, agent, mentions) {
            HookGate::Ready {
                claim,
                claim_ttl,
                claim_pattern,
            } => (claim, claim_ttl, claim_pattern),
            HookGate::ConditionFailed => {
                release_own_claim(lease.as_ref());
                let firing = HookFiring {
                    ts: now,
                    hook_id: hook.id.clone(),
                    channel: channel.to_string(),
                    message_id: message_id.to_string(),
                    condition_result: false,
                    executed: false,
                    reason: Some("condition not met".to_string()),
                };
                let _ = append_record(&hooks_audit_path(), &firing);
                continue;
            }
            HookGate::Busy {
                reason,
                condition_result,
            } => {
                // An instance of this hook is already working. Give the lease
                // straight back — it is the *claim* that is busy — and, for a
                // lease-enabled hook, keep the trigger for the next spawn
                // instead of dropping it the way a cooldown would.
                release_own_claim(lease.as_ref());
                let reason = if hook.uses_lease() {
                    match enqueue_trigger(hook, channel, message_id, agent, body, &command_agent) {
                        None => format!("{} (queued)", reason),
                        Some(why) => format!("{} ({})", reason, why),
                    }
                } else {
                    reason.to_string()
                };
                let firing = HookFiring {
                    ts: now,
                    hook_id: hook.id.clone(),
                    channel: channel.to_string(),
                    message_id: message_id.to_string(),
                    condition_result,
                    executed: false,
                    reason: Some(reason),
                };
                let _ = append_record(&hooks_audit_path(), &firing);
                continue;
            }
        };

        let is_on_exit = matches!(hook.claim_release, Some(ClaimRelease::OnExit));
        let cmd_display = shell_display(&hook.command);

        // Everything that queued up behind the previous spawn goes to this
        // one, deduplicated against itself and against the message that
        // triggered it. Marked delivered only if the spawn actually starts.
        let batch = if hook.uses_lease() {
            drain_batch(hook, channel, &dedup_key(agent, body))
        } else {
            Vec::new()
        };

        // Spawn the command
        let executed = if hook.command.is_empty() {
            release_own_claim(claim.as_ref());
            release_own_claim(lease.as_ref());
            false
        } else {
            let mut command = std::process::Command::new(&hook.command[0]);
            command
                .args(&hook.command[1..])
                .current_dir(&hook.cwd)
                .env("RITE_CHANNEL", channel)
                .env("RITE_MESSAGE_ID", message_id)
                .env("RITE_AGENT", &command_agent)
                .env("RITE_HOOK_ID", &hook.id)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());

            if hook.uses_lease() {
                // Chronological, triggering message last. Additive: hooks
                // without a lease see exactly the environment they always did.
                let mut ids: Vec<&str> = batch.iter().map(|e| e.message_id.as_str()).collect();
                ids.push(message_id);
                command
                    .env("RITE_BATCH_COUNT", ids.len().to_string())
                    .env("RITE_BATCH_MESSAGE_IDS", ids.join(","))
                    .env("RITE_LEASE_PATTERN", hook.lease_pattern(channel));
            }

            if let Some(traceparent) = crate::telemetry::current_traceparent() {
                command.env("TRACEPARENT", traceparent);
            }

            match command.spawn() {
                Ok(mut child) => {
                    mark_delivered(&batch);
                    if is_on_exit {
                        // Block until command exits, then release claim and
                        // lease — the next message drains whatever queued up
                        // while this spawn was running.
                        let _ = child.wait();
                        release_own_claim(claim.as_ref());
                        release_own_claim(lease.as_ref());
                    } else {
                        // Reap child in background to prevent zombie processes
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                    }
                    true
                }
                Err(_) => {
                    release_own_claim(claim.as_ref());
                    release_own_claim(lease.as_ref());
                    false
                }
            }
        };

        // Post system message to channel
        if executed {
            info!(hook_id = %hook.id, channel, message_id, "hook fired");
            let sys_msg = Message::new(
                "system",
                channel,
                format!("Hook {} fired: {}", hook.id, cmd_display),
            )
            .with_meta(MessageMeta::System {
                event: SystemEvent::HookFired {
                    hook_id: hook.id.clone(),
                    command: hook.command.clone(),
                },
            });
            let _ = append_record(&channel_path(channel), &sys_msg);

            results.push(HookFireResult {
                hook_id: hook.id.clone(),
                command_display: cmd_display,
                claim_pattern,
                claim_ttl,
                batch_count: batch.len() + 1,
            });
        }

        // Update last_fired
        let mut updated = hook.clone();
        updated.last_fired = Some(now);
        let _ = append_record(&hooks_path(), &updated);

        // Audit log
        let firing = HookFiring {
            ts: now,
            hook_id: hook.id.clone(),
            channel: channel.to_string(),
            message_id: message_id.to_string(),
            condition_result: true,
            executed,
            reason: if executed {
                None
            } else {
                Some("spawn failed".to_string())
            },
        };
        let _ = append_record(&hooks_audit_path(), &firing);
    }

    Ok(results)
}

fn hook_command_agent<'a>(hook: &'a Hook, triggering_agent: &'a str) -> &'a str {
    hook.claim_owner.as_deref().unwrap_or(triggering_agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cooldown() {
        assert_eq!(parse_cooldown("30s").unwrap(), 30);
        assert_eq!(parse_cooldown("5m").unwrap(), 300);
        assert_eq!(parse_cooldown("1h").unwrap(), 3600);
        assert_eq!(parse_cooldown("60").unwrap(), 60);
        assert!(parse_cooldown("").is_err());
        assert!(parse_cooldown("abc").is_err());
    }

    #[test]
    fn test_build_active_hooks() {
        let hooks = vec![
            Hook {
                id: "hk-abc".to_string(),
                channel: "test".to_string(),
                condition: HookCondition::ClaimAvailable {
                    pattern: "p".to_string(),
                },
                command: vec!["echo".to_string()],
                cwd: PathBuf::from("/tmp"),
                cooldown_secs: 30,
                last_fired: None,
                created_at: Utc::now(),
                created_by: None,
                claim_release: Some(ClaimRelease::OnExit),
                claim_pattern: None,
                claim_owner: None,
                priority: 0,
                require_flag: None,
                lease: None,
                active: true,
                description: None,
                name: None,
                owner: None,
                extra: Default::default(),
            },
            Hook {
                id: "hk-abc".to_string(),
                channel: "test".to_string(),
                condition: HookCondition::ClaimAvailable {
                    pattern: "p".to_string(),
                },
                command: vec!["echo".to_string()],
                cwd: PathBuf::from("/tmp"),
                cooldown_secs: 30,
                last_fired: None,
                created_at: Utc::now(),
                created_by: None,
                claim_release: Some(ClaimRelease::OnExit),
                claim_pattern: None,
                claim_owner: None,
                priority: 0,
                require_flag: None,
                lease: None,
                active: false, // Deactivated
                description: None,
                name: None,
                owner: None,
                extra: Default::default(),
            },
        ];

        let active = build_active_hooks(&hooks);
        assert!(active.is_empty()); // Second record deactivated it
    }

    #[test]
    fn test_is_claim_available_no_claims() {
        // With no claims file, everything is available
        // This test relies on the default data dir not having claims,
        // but since read_records returns empty on missing file, it works.
        // We'll test this more thoroughly in integration tests.
        let result = is_claim_available("agent://nonexistent-test-pattern-12345");
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_hook_command_agent_uses_claim_owner() {
        let hook = Hook {
            id: "hk-owner".to_string(),
            channel: "test".to_string(),
            condition: HookCondition::ClaimAvailable {
                pattern: "p".to_string(),
            },
            command: vec!["echo".to_string()],
            cwd: PathBuf::from("/tmp"),
            cooldown_secs: 30,
            last_fired: None,
            created_at: Utc::now(),
            created_by: None,
            claim_release: Some(ClaimRelease::OnExit),
            claim_pattern: None,
            claim_owner: Some("worker-agent".to_string()),
            priority: 0,
            require_flag: None,
            lease: None,
            active: true,
            description: None,
            name: None,
            owner: None,
            extra: Default::default(),
        };

        assert_eq!(hook_command_agent(&hook, "trigger-agent"), "worker-agent");
    }

    #[test]
    fn test_hook_command_agent_defaults_to_triggering_agent() {
        let hook = Hook {
            id: "hk-default".to_string(),
            channel: "test".to_string(),
            condition: HookCondition::MentionReceived {
                agent: "helper".to_string(),
            },
            command: vec!["echo".to_string()],
            cwd: PathBuf::from("/tmp"),
            cooldown_secs: 30,
            last_fired: None,
            created_at: Utc::now(),
            created_by: None,
            claim_release: None,
            claim_pattern: None,
            claim_owner: None,
            priority: 0,
            require_flag: None,
            lease: None,
            active: true,
            description: None,
            name: None,
            owner: None,
            extra: Default::default(),
        };

        assert_eq!(hook_command_agent(&hook, "trigger-agent"), "trigger-agent");
    }

    /// A young lease blocks whatever presence says. A freshly spawned agent
    /// may not have run a single rite command yet, and its last heartbeat
    /// could be from a previous life — stealing its lease on that basis would
    /// double-spawn every single time.
    #[test]
    fn test_young_lease_blocks_regardless_of_presence() {
        let mut claim = FileClaim::new(
            "agent-that-never-heartbeat",
            vec!["spawn://hk-abc/rite".to_string()],
            600,
        );
        claim.ts = Utc::now() - chrono::Duration::seconds(PRESENCE_TTL_SECS / 2);
        assert!(lease_blocks(&claim, Utc::now()));
    }

    /// Unknown presence is not evidence of a dead agent (bn-12i6), so it must
    /// not supersede a lease; the lease TTL is the backstop for that case.
    #[test]
    fn test_old_lease_with_unknown_presence_still_blocks() {
        let mut claim = FileClaim::new(
            "agent-with-no-heartbeat-history-at-all",
            vec!["spawn://hk-abc/rite".to_string()],
            600,
        );
        claim.ts = Utc::now() - chrono::Duration::seconds(PRESENCE_TTL_SECS * 10);
        assert!(
            lease_blocks(&claim, Utc::now()),
            "unknown presence must never be read as 'holder is gone'"
        );
    }

    #[test]
    fn test_lease_available_ignores_expired_and_released_leases() {
        let pattern = "spawn://hk-abc/rite";
        let now = Utc::now();

        let mut expired = FileClaim::new("holder", vec![pattern.to_string()], 600);
        expired.expires_at = now - chrono::Duration::seconds(1);
        assert!(
            lease_available(pattern, std::slice::from_ref(&expired), now),
            "an expired lease is the TTL backstop doing its job"
        );

        let live = FileClaim::new("holder", vec![pattern.to_string()], 600);
        assert!(!lease_available(pattern, std::slice::from_ref(&live), now));

        // Latest record per ID wins, so a release frees the pattern.
        assert!(lease_available(
            pattern,
            &[live.clone(), live.release()],
            now
        ));

        // A lease on another channel is a different pattern entirely.
        assert!(lease_available(
            "spawn://hk-abc/other",
            std::slice::from_ref(&live),
            now
        ));
    }

    #[test]
    fn test_dedup_key_identity() {
        // Same sender, same body (modulo surrounding whitespace) collapses.
        assert_eq!(
            dedup_key("a", "do the thing"),
            dedup_key("a", "do the thing\n")
        );
        // Different sender or different body does not.
        assert_ne!(
            dedup_key("a", "do the thing"),
            dedup_key("b", "do the thing")
        );
        assert_ne!(dedup_key("a", "do the thing"), dedup_key("a", "do other"));
    }

    #[test]
    fn test_priority_sorting() {
        // Create hooks with different priorities
        let hooks = vec![
            Hook {
                id: "hk-high".to_string(),
                channel: "test".to_string(),
                condition: HookCondition::ClaimAvailable {
                    pattern: "p1".to_string(),
                },
                command: vec!["echo".to_string(), "high".to_string()],
                cwd: PathBuf::from("/tmp"),
                cooldown_secs: 30,
                last_fired: None,
                created_at: Utc::now(),
                created_by: None,
                claim_release: Some(ClaimRelease::OnExit),
                claim_pattern: None,
                claim_owner: None,
                priority: 10,
                require_flag: None,
                lease: None,
                active: true,
                description: None,
                name: None,
                owner: None,
                extra: Default::default(),
            },
            Hook {
                id: "hk-low".to_string(),
                channel: "test".to_string(),
                condition: HookCondition::ClaimAvailable {
                    pattern: "p2".to_string(),
                },
                command: vec!["echo".to_string(), "low".to_string()],
                cwd: PathBuf::from("/tmp"),
                cooldown_secs: 30,
                last_fired: None,
                created_at: Utc::now(),
                created_by: None,
                claim_release: Some(ClaimRelease::OnExit),
                claim_pattern: None,
                claim_owner: None,
                priority: -5,
                require_flag: None,
                lease: None,
                active: true,
                description: None,
                name: None,
                owner: None,
                extra: Default::default(),
            },
            Hook {
                id: "hk-mid".to_string(),
                channel: "test".to_string(),
                condition: HookCondition::ClaimAvailable {
                    pattern: "p3".to_string(),
                },
                command: vec!["echo".to_string(), "mid".to_string()],
                cwd: PathBuf::from("/tmp"),
                cooldown_secs: 30,
                last_fired: None,
                created_at: Utc::now(),
                created_by: None,
                claim_release: Some(ClaimRelease::OnExit),
                claim_pattern: None,
                claim_owner: None,
                priority: 0,
                require_flag: None,
                lease: None,
                active: true,
                description: None,
                name: None,
                owner: None,
                extra: Default::default(),
            },
        ];

        let active = build_active_hooks(&hooks);
        let mut hooks_to_process: Vec<&Hook> = active.values().collect();
        hooks_to_process.sort_by_key(|h| h.priority);

        // Verify order: low (-5), mid (0), high (10)
        assert_eq!(hooks_to_process.len(), 3);
        assert_eq!(hooks_to_process[0].id, "hk-low");
        assert_eq!(hooks_to_process[1].id, "hk-mid");
        assert_eq!(hooks_to_process[2].id, "hk-high");
    }
}
