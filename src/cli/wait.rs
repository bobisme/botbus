//! Wait command - block until a relevant message arrives.
//!
//! # Acknowledgment
//!
//! `--reply-to <id>` turns this from "some message arrived" into "*that*
//! message was answered". An agent posts a request, captures the id from
//! `rite send --format json`, and blocks on it. It either gets the answer or
//! learns definitively that nobody answered. That is the whole point: without
//! it, the requester cannot tell "heard, working on it" from "shouted into the
//! void", so it posts the request again, and again.
//!
//! Detection has two halves, and the seam between them is where a naive
//! implementation loses messages:
//!
//! 1. **Startup.** [`replies_already_present`] asks the index for the direct
//!    replies to the awaited message. This catches a reply that landed in the
//!    window between `rite send` returning and `rite wait` starting, which is
//!    small but is exactly when a fast reviewer answers.
//! 2. **Steady state.** The existing file-tail loop, with
//!    `msg.parent_id() == Some(target)` added to the predicate.
//!
//! The halves are ordered so they overlap rather than abut. Channel offsets are
//! snapshotted and the watcher is armed *first*, at instant A; the index query
//! runs after that, at instant B >= A. The tail loop therefore covers
//! everything appended after A, the index query covers everything on disk at B,
//! and A <= B leaves no interval uncovered. Anything written in [A, B] is seen
//! by both halves, so ids reported by the startup half are recorded in a
//! `seen` set and skipped by the tail loop: caught once, never twice.

use anyhow::{Context, Result};
use chrono::DateTime;
use colored::Colorize;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};
use ulid::Ulid;

use crate::cli::OutputFormat;
use crate::core::identity::resolve_agent;
use crate::core::message::{Message, MessageMeta, read_messages_from_offset};
use crate::core::project::channels_dir;
use crate::index::IndexSyncer;
use crate::storage::jsonl::read_records;
use crate::storage::watch::{debounce_events, filter_channel_events, watch_directory};

/// Exit status when the wait timed out with no matching message.
pub const EXIT_TIMEOUT: i32 = 1;

/// Exit status when the `--reply-to` id cannot be waited on: it is not a ULID,
/// or this store has never seen it.
///
/// Distinct from [`EXIT_TIMEOUT`] on purpose. "Nobody answered you" and "you
/// asked about an id that does not exist here" demand opposite responses from a
/// script, and a mistyped id that merely timed out is how a requester concludes
/// it was ignored and re-posts.
pub const EXIT_BAD_PARENT: i32 = 2;

pub struct WaitOptions {
    /// Wait for @mentions of current agent from any channel
    pub mentions: bool,
    /// Wait for messages in specific channel(s)
    pub channels: Vec<String>,
    /// Wait for messages with specific labels (any of them)
    pub labels: Vec<String>,
    /// Wait only for messages from this agent
    pub from: Option<String>,
    /// Wait for a reply to this specific message id (acknowledgment)
    pub reply_to: Option<String>,
    /// Wait on a `--reply-to` id this store has not seen, instead of refusing
    pub allow_missing_parent: bool,
    /// Timeout in seconds (0 = no timeout)
    pub timeout: u64,
    /// Output format
    pub format: OutputFormat,
}

#[derive(Debug, Serialize)]
pub struct WaitOutput {
    /// Whether a message was received (vs timeout)
    pub received: bool,
    /// The triggering message (if received)
    pub message: Option<Message>,
    /// Channel the message was in
    pub channel: Option<String>,
    /// Reason for returning
    pub reason: String,
    /// The message that was awaited, when `--reply-to` was given. Echoed on
    /// every outcome so a caller can correlate this result with the request it
    /// sent, without holding the id itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<String>,
}

/// Wait for a relevant message to arrive.
pub fn run(mut options: WaitOptions, explicit_agent: Option<&str>) -> Result<()> {
    let agent = resolve_agent(explicit_agent);

    // For --mentions, we need an agent identity
    if options.mentions && agent.is_none() {
        anyhow::bail!("--mentions requires agent identity. Set RITE_AGENT or use --agent flag.");
    }

    // A malformed anchor never becomes a wait. It cannot match anything, so
    // waiting on it would burn the whole timeout and then report the one thing
    // that is not true: that nobody answered.
    let target: Option<Ulid> = match options.reply_to.as_deref() {
        Some(raw) => match raw.trim().parse::<Ulid>() {
            Ok(parent) => Some(parent),
            Err(_) => bad_parent(
                raw.trim(),
                "invalid_parent",
                &format!(
                    "Invalid --reply-to message ID: '{}'. Expected a ULID, as printed by \
                     `rite send --format json` or carried in $RITE_MESSAGE_ID inside a hook.",
                    raw.trim()
                ),
                options.format,
            ),
        },
        None => None,
    };

    // Strip # prefix from channels if present (common user pattern)
    options.channels = options
        .channels
        .iter()
        .map(|ch| ch.strip_prefix('#').unwrap_or(ch).to_string())
        .collect();

    // When --mentions is set, watch ALL channels (mentions can come from anywhere).
    // The --channels list is used for OR matching, not for restricting what we watch.
    let watch_channels: Option<Vec<&str>> = if options.mentions || options.channels.is_empty() {
        None
    } else {
        Some(options.channels.iter().map(|s| s.as_str()).collect())
    };

    let channels_path = channels_dir();
    if !channels_path.exists() {
        std::fs::create_dir_all(&channels_path)?;
    }

    // Track current file offsets for all channels we're watching
    let mut channel_offsets = collect_channel_offsets(&channels_path, watch_channels.as_deref())?;

    // Set up file watcher
    let (_watcher, rx) =
        watch_directory(&channels_path).with_context(|| "Failed to watch channels directory")?;

    let timeout_duration = if options.timeout > 0 {
        Some(Duration::from_secs(options.timeout))
    } else {
        None
    };

    let start = Instant::now();

    if options.format != OutputFormat::Json {
        if let Some(parent) = target {
            // `--reply-to` narrows: it names one question, and the other flags
            // only ever remove candidate answers. Say so in one line.
            let mut what = format!("Waiting for a reply to {}", parent);
            if let Some(ref from) = options.from {
                what.push_str(&format!(" from @{}", from));
            }
            if !options.channels.is_empty() {
                let ch_display: Vec<String> =
                    options.channels.iter().map(|c| format!("#{}", c)).collect();
                what.push_str(&format!(" in {}", ch_display.join(", ")));
            }
            if !options.labels.is_empty() {
                what.push_str(&format!(" with labels {:?}", options.labels));
            }
            eprint!("{}...", what.cyan());
        } else if options.mentions && !options.channels.is_empty() {
            let ch_display: Vec<String> =
                options.channels.iter().map(|c| format!("#{}", c)).collect();
            eprint!(
                "Waiting for @{} or messages in {}...",
                agent.as_ref().unwrap().cyan(),
                ch_display.join(", ").cyan()
            );
        } else if !options.channels.is_empty() {
            let ch_display: Vec<String> =
                options.channels.iter().map(|c| format!("#{}", c)).collect();
            eprint!(
                "Waiting for messages in {}...",
                ch_display.join(", ").cyan()
            );
        } else if options.mentions {
            eprint!("Waiting for @{}...", agent.as_ref().unwrap().cyan());
        } else if !options.labels.is_empty() {
            eprint!("Waiting for messages with labels {:?}...", options.labels);
        } else {
            eprint!("Waiting for any message...");
        }
        if let Some(t) = timeout_duration {
            eprintln!(" (timeout: {}s)", t.as_secs());
        } else {
            eprintln!();
        }
    }

    let filters = WaitFilters {
        agent: agent.as_deref(),
        mentions: options.mentions,
        channels: &options.channels,
        labels: &options.labels,
        from: options.from.as_deref(),
        reply_to: target,
    };

    // --- startup half -------------------------------------------------------
    //
    // Runs strictly after the offset snapshot and the watcher above, so the two
    // halves overlap instead of leaving a hole. See the module docs.
    let mut seen: HashSet<Ulid> = HashSet::new();

    if let Some(parent) = target {
        for (channel_name, msg) in replies_already_present(parent, watch_channels.as_deref()) {
            seen.insert(msg.id);

            if agent.as_ref().is_some_and(|a| a == &msg.agent) {
                continue;
            }

            if filters.matches(&msg, &channel_name) {
                return report_match(
                    &msg,
                    &channel_name,
                    agent.as_deref(),
                    target,
                    options.format,
                );
            }
        }

        // No answer yet. Before blocking, prove the question exists here. An id
        // this store has never seen can only produce a full-length timeout, and
        // "timed out" would then mean "you typed the id wrong" — the exact
        // misreading that makes an agent re-post its request.
        //
        // The index was just synced by the startup query, so the cheap lookup
        // is usually enough and the JSONL scan is the rare fallback.
        if !options.allow_missing_parent && !parent_is_known(parent) {
            bad_parent(
                &parent.to_string(),
                "unknown_parent",
                &format!(
                    "No message {} in this store, so no reply to it can arrive. \
                     Check the id from `rite send --format json`, or pass \
                     --allow-missing-parent to wait for a parent that is still syncing in.",
                    parent
                ),
                options.format,
            );
        }
    }

    loop {
        // Check timeout
        if let Some(timeout) = timeout_duration
            && start.elapsed() >= timeout
        {
            let advice = match target {
                Some(parent) => vec![format!(
                    "No reply to {} arrived. Do not send the request again. \
                     Read `rite history --thread {}` or escalate.",
                    parent, parent
                )],
                None => vec![],
            };

            let output = WaitOutput {
                received: false,
                message: None,
                channel: None,
                reason: "timeout".to_string(),
                reply_to: target.map(|p| p.to_string()),
                advice,
            };

            match options.format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&output)?);
                }
                OutputFormat::Pretty => {
                    println!("{} Timeout after {}s", "✗".red(), timeout.as_secs());
                    for note in &output.advice {
                        println!("{}", note.dimmed());
                    }
                }
                OutputFormat::Text => {
                    println!("timeout");
                }
            }

            std::process::exit(EXIT_TIMEOUT);
        }

        // Wait for file changes (with short poll interval for timeout checking)
        let poll_duration = Duration::from_millis(500);
        let changed = debounce_events(&rx, poll_duration);
        let changed_channels = filter_channel_events(changed);

        // Check each changed channel for new messages
        for channel_name in changed_channels {
            // Skip if we're filtering to specific channels
            if let Some(ref filter) = watch_channels
                && !filter.contains(&channel_name.as_str())
            {
                continue;
            }

            let channel_path = channels_path.join(format!("{}.jsonl", channel_name));
            let offset = channel_offsets.get(&channel_name).copied().unwrap_or(0);

            // Read new messages
            let (new_messages, new_offset): (Vec<Message>, u64) =
                read_messages_from_offset(&channel_path, offset)?;

            // Update offset
            channel_offsets.insert(channel_name.clone(), new_offset);

            // Check each message
            for msg in new_messages {
                // Skip our own messages
                if agent.as_ref().is_some_and(|a| a == &msg.agent) {
                    continue;
                }

                // Already offered by the startup half. Only reachable for a
                // reply written in the overlap window between the offset
                // snapshot and the index query, and only when that reply failed
                // the filters — but the guard is unconditional so no message can
                // ever be reported twice.
                if seen.contains(&msg.id) {
                    continue;
                }

                if filters.matches(&msg, &channel_name) {
                    return report_match(
                        &msg,
                        &channel_name,
                        agent.as_deref(),
                        target,
                        options.format,
                    );
                }
            }
        }
    }
}

/// Print a matching message and return, in whichever format was asked for.
///
/// Shared by both halves so the startup path and the tail path cannot drift
/// into reporting the same event differently.
fn report_match(
    msg: &Message,
    channel_name: &str,
    agent: Option<&str>,
    target: Option<Ulid>,
    format: OutputFormat,
) -> Result<()> {
    let reason = if target.is_some() {
        "reply".to_string()
    } else if is_mention_for_agent(msg, agent) {
        "mention".to_string()
    } else {
        "message".to_string()
    };

    let output = WaitOutput {
        received: true,
        message: Some(msg.clone()),
        channel: Some(channel_name.to_string()),
        reason,
        reply_to: target.map(|p| p.to_string()),
        advice: vec![],
    };

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Pretty => {
            println!();
            if target.is_some() {
                println!("{} Reply received in #{}", "✓".green(), channel_name.cyan());
            } else {
                println!(
                    "{} Message received in #{}",
                    "✓".green(),
                    channel_name.cyan()
                );
            }
            print_message(msg);
        }
        OutputFormat::Text => {
            println!("{}  {}  {}", channel_name, msg.agent, msg.body);
        }
    }

    Ok(())
}

/// Report a `--reply-to` id that cannot be waited on, and exit
/// [`EXIT_BAD_PARENT`].
///
/// This is not an anyhow error because the exit status carries meaning: a
/// caller must be able to tell "your id is unusable" from "nobody answered"
/// (`1`) and from an ordinary failure (`1`, via `main`).
fn bad_parent(id: &str, reason: &str, human: &str, format: OutputFormat) -> ! {
    match format {
        OutputFormat::Json => {
            let output = WaitOutput {
                received: false,
                message: None,
                channel: None,
                reason: reason.to_string(),
                reply_to: Some(id.to_string()),
                advice: vec![human.to_string()],
            };
            match serde_json::to_string_pretty(&output) {
                Ok(text) => println!("{}", text),
                Err(_) => eprintln!("{}", human),
            }
        }
        OutputFormat::Pretty => {
            eprintln!("{} {}", "✗".red(), human);
        }
        OutputFormat::Text => {
            println!("{}", reason);
            eprintln!("{}", human);
        }
    }

    std::process::exit(EXIT_BAD_PARENT);
}

/// True when this store holds the awaited message.
///
/// The index answers first because it is one indexed lookup and it was just
/// synced by [`replies_already_present`]. A negative from the index is never
/// trusted on its own — it is also what an unwritable, absent, or partly synced
/// index says — so the JSONL, which is the source of truth, decides.
fn parent_is_known(parent: Ulid) -> bool {
    let id = parent.to_string();

    if let Ok(syncer) = IndexSyncer::new()
        && syncer.index().has_message(&id).unwrap_or(false)
    {
        return true;
    }

    for path in channel_files(None) {
        let messages: Vec<Message> = read_records(&path).unwrap_or_default();
        if messages.iter().any(|m| m.id == parent) {
            return true;
        }
    }

    false
}

/// Direct replies to `parent` that were already on disk when the wait began.
///
/// Ordered oldest first, so the earliest acknowledgment wins when several
/// arrived before the wait started.
fn replies_already_present(parent: Ulid, watch: Option<&[&str]>) -> Vec<(String, Message)> {
    match indexed_replies(parent, watch) {
        Ok(found) => found,
        // The index is derived and optional. When it cannot answer, read the
        // channels directly rather than pretend there are no replies — a false
        // "no answer yet" here is the retry storm this command exists to stop.
        Err(_) => scanned_replies(parent, watch),
    }
}

/// Startup half via the index: one query instead of reading every channel.
fn indexed_replies(parent: Ulid, watch: Option<&[&str]>) -> Result<Vec<(String, Message)>> {
    let mut syncer = IndexSyncer::new()?;
    let stats = syncer.sync_all()?;
    if !stats.errors.is_empty() {
        // A channel that failed to sync is a channel whose replies the index
        // cannot see. Fall back rather than answer from a known-incomplete set.
        anyhow::bail!("index sync incomplete: {}", stats.errors.join("; "));
    }

    let mut wanted: HashMap<String, HashSet<Ulid>> = HashMap::new();
    for edge in syncer.index().replies_to(&parent.to_string())? {
        if let Some(filter) = watch
            && !filter.contains(&edge.channel.as_str())
        {
            continue;
        }
        if let Ok(id) = edge.id.parse::<Ulid>() {
            wanted.entry(edge.channel).or_default().insert(id);
        }
    }

    let mut found = Vec::new();
    for (channel, ids) in wanted {
        let path = channels_dir().join(format!("{}.jsonl", channel));
        let messages: Vec<Message> = read_records(&path).unwrap_or_default();
        for msg in messages {
            // The edge says which channel to open; the record itself decides
            // whether it really answers `parent`.
            if ids.contains(&msg.id) && msg.parent_id() == Some(parent) {
                found.push((channel.clone(), msg));
            }
        }
    }

    found.sort_by_key(|(_, msg)| msg.id);
    Ok(found)
}

/// Startup half without the index: read the channel files.
fn scanned_replies(parent: Ulid, watch: Option<&[&str]>) -> Vec<(String, Message)> {
    let mut found = Vec::new();

    for path in channel_files(watch) {
        let Some(channel) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let messages: Vec<Message> = read_records(&path).unwrap_or_default();

        // A deleted reply is not an acknowledgment. The index drops the edge on
        // deletion, so excluding tombstoned messages keeps both halves of the
        // startup query answering the same question.
        let deleted: HashSet<Ulid> = messages
            .iter()
            .filter_map(|m| match &m.meta {
                Some(MessageMeta::Deleted { target_id, .. }) => Some(*target_id),
                _ => None,
            })
            .collect();

        for msg in messages {
            if msg.parent_id() == Some(parent) && !deleted.contains(&msg.id) {
                found.push((channel.to_string(), msg));
            }
        }
    }

    found.sort_by_key(|(_, msg)| msg.id);
    found
}

/// Channel JSONL paths, optionally restricted to named channels.
fn channel_files(filter: Option<&[&str]>) -> Vec<std::path::PathBuf> {
    let channels_path = channels_dir();
    let Ok(entries) = std::fs::read_dir(&channels_path) else {
        return Vec::new();
    };

    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter(|path| match filter {
            Some(names) => path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|name| names.contains(&name)),
            None => true,
        })
        .collect()
}

fn collect_channel_offsets(
    channels_path: &Path,
    filter_channels: Option<&[&str]>,
) -> Result<std::collections::HashMap<String, u64>> {
    let mut offsets = std::collections::HashMap::new();

    if let Ok(entries) = std::fs::read_dir(channels_path) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str())
            {
                // Skip if filtering to specific channels
                if let Some(filters) = filter_channels
                    && !filters.contains(&name)
                {
                    continue;
                }

                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                offsets.insert(name.to_string(), size);
            }
        }
    }

    // If filtering to channels that don't exist yet, add them with offset 0
    if let Some(filters) = filter_channels {
        for filter in filters {
            offsets.entry(filter.to_string()).or_insert(0);
        }
    }

    Ok(offsets)
}

fn is_mention_for_agent(msg: &Message, agent: Option<&str>) -> bool {
    agent.is_some_and(|agent| {
        let mention = format!("@{}", agent);
        msg.body.contains(&mention)
    })
}

/// Decide whether `msg` satisfies the wait.
///
/// # Composition
///
/// `--mentions` and `-c` are places a wanted message might turn up, so they
/// combine with OR: "@me anywhere, or anything in #rite". `--from` and
/// `--reply-to` are not places. They are identity, and they narrow: a match
/// must satisfy every one of them **and** the OR group.
///
/// `--reply-to` must narrow. Putting it in the OR group would let any message
/// in a watched channel end the wait, so the requester would report "answered"
/// when nobody answered. A false acknowledgment is worse than a timeout: a
/// timeout makes an agent ask again, a false ack makes it proceed on an answer
/// that does not exist.
struct WaitFilters<'a> {
    agent: Option<&'a str>,
    mentions: bool,
    channels: &'a [String],
    labels: &'a [String],
    from: Option<&'a str>,
    reply_to: Option<Ulid>,
}

impl WaitFilters<'_> {
    fn matches(&self, msg: &Message, channel_name: &str) -> bool {
        if self.from.is_some_and(|from| msg.agent != from) {
            return false;
        }

        // `parent_id` and not `reply_to`: a message that anchors to itself has
        // no parent, so it can never acknowledge itself.
        if let Some(parent) = self.reply_to
            && msg.parent_id() != Some(parent)
        {
            return false;
        }

        // When --mentions + --channels: OR logic (mention from any channel OR
        // any message from specified channels). --from narrows that result.
        let is_mention = self.mentions && is_mention_for_agent(msg, self.agent);
        let is_in_channel =
            !self.channels.is_empty() && self.channels.iter().any(|c| c == channel_name);

        if self.mentions && !self.channels.is_empty() {
            is_mention || is_in_channel
        } else if self.mentions {
            is_mention
        } else if !self.labels.is_empty() {
            msg.has_any_label(self.labels)
        } else {
            true
        }
    }
}

fn print_message(msg: &Message) {
    use chrono::Local;

    let local_time: DateTime<Local> = msg.ts.with_timezone(&Local);
    let time_str = local_time.format("%H:%M").to_string();

    let agent_colored = colorize_agent(&msg.agent);

    println!("[{}] {}: {}", time_str.dimmed(), agent_colored, msg.body);
}

fn colorize_agent(name: &str) -> colored::ColoredString {
    let hash: usize = name.bytes().map(|b| b as usize).sum();
    let colors = [
        colored::Color::Cyan,
        colored::Color::Green,
        colored::Color::Yellow,
        colored::Color::Blue,
        colored::Color::Magenta,
    ];
    let color = colors[hash % colors.len()];
    name.color(color).bold()
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn test_collect_channel_offsets_filtered() {
        // This test only tests the offset collection logic, not the full wait
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path()).unwrap();
        std::fs::write(temp.path().join("general.jsonl"), "{}\n").unwrap();
        std::fs::write(temp.path().join("backend.jsonl"), "{}\n").unwrap();

        let offsets = collect_channel_offsets(temp.path(), Some(&["backend"])).unwrap();

        // Should only have backend
        assert_eq!(offsets.len(), 1);
        assert!(offsets.contains_key("backend"));
    }

    #[test]
    fn test_collect_channel_offsets_multiple() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path()).unwrap();
        std::fs::write(temp.path().join("general.jsonl"), "{}\n").unwrap();
        std::fs::write(temp.path().join("backend.jsonl"), "{}\n").unwrap();
        std::fs::write(temp.path().join("frontend.jsonl"), "{}\n").unwrap();

        let offsets = collect_channel_offsets(temp.path(), Some(&["backend", "frontend"])).unwrap();

        assert_eq!(offsets.len(), 2);
        assert!(offsets.contains_key("backend"));
        assert!(offsets.contains_key("frontend"));
    }

    #[test]
    fn test_collect_channel_offsets_all() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path()).unwrap();
        std::fs::write(temp.path().join("general.jsonl"), "{}\n").unwrap();
        std::fs::write(temp.path().join("backend.jsonl"), "{}\n").unwrap();

        let offsets = collect_channel_offsets(temp.path(), None).unwrap();

        assert_eq!(offsets.len(), 2);
        assert!(offsets.contains_key("general"));
        assert!(offsets.contains_key("backend"));
    }

    fn test_message(agent: &str, channel: &str, body: &str) -> Message {
        Message::new(agent, channel, body)
    }

    /// A `WaitFilters` with everything off, ready to be narrowed per test.
    fn filters<'a>() -> WaitFilters<'a> {
        WaitFilters {
            agent: Some("me"),
            mentions: false,
            channels: &[],
            labels: &[],
            from: None,
            reply_to: None,
        }
    }

    #[test]
    fn test_message_matches_from_filter() {
        let msg = test_message("rite-codex", "rite", "reply");
        let channels = vec!["rite".to_string()];

        let mut f = filters();
        f.channels = &channels;
        f.from = Some("rite-codex");
        assert!(f.matches(&msg, "rite"));

        f.from = Some("other-agent");
        assert!(!f.matches(&msg, "rite"));
    }

    #[test]
    fn test_from_filter_narrows_mention_channel_or_logic() {
        let channels = vec!["rite".to_string()];
        let from_agent = test_message("rite-codex", "general", "@me done");
        let other_agent = test_message("other-agent", "rite", "side note");

        let mut f = filters();
        f.mentions = true;
        f.channels = &channels;
        f.from = Some("rite-codex");

        assert!(f.matches(&from_agent, "general"));
        assert!(!f.matches(&other_agent, "rite"));
    }

    #[test]
    fn test_from_filter_narrows_label_matches() {
        let msg = test_message("reviewer", "rite", "ready").with_labels(vec!["review".to_string()]);
        let labels = vec!["review".to_string()];

        let mut f = filters();
        f.labels = &labels;
        f.from = Some("reviewer");
        assert!(f.matches(&msg, "rite"));

        f.from = Some("other-agent");
        assert!(!f.matches(&msg, "rite"));
    }

    // --- --reply-to -------------------------------------------------------

    /// The whole point of the flag: a reply to a *different* question must not
    /// end the wait. Answering the wrong question is a false acknowledgment,
    /// and the requester would act on an answer it never got.
    #[test]
    fn reply_to_matches_only_the_named_parent() {
        let mine = test_message("me", "rite", "review rv-12?");
        let theirs = test_message("me", "rite", "review rv-13?");

        let answer = test_message("reviewer", "rite", "on it").with_reply_to(mine.id);
        let other_answer = test_message("reviewer", "rite", "on it").with_reply_to(theirs.id);
        let top_level = test_message("reviewer", "rite", "unrelated chatter");

        assert!(matches_reply(&answer, Some(mine.id)));
        assert!(!matches_reply(&other_answer, Some(mine.id)));
        assert!(!matches_reply(&top_level, Some(mine.id)));
    }

    /// `--reply-to` narrows rather than widens. With `--from`, both must hold.
    #[test]
    fn reply_to_and_from_are_both_required() {
        let question = test_message("me", "rite", "review rv-12?");
        let wanted = test_message("reviewer", "rite", "on it").with_reply_to(question.id);
        let stranger = test_message("passer-by", "rite", "nice").with_reply_to(question.id);

        let mut f = filters();
        f.from = Some("reviewer");
        f.reply_to = Some(question.id);

        assert!(f.matches(&wanted, "rite"));
        assert!(!f.matches(&stranger, "rite"));
    }

    /// And with `-L`, which is the documented ack convention.
    #[test]
    fn reply_to_and_label_are_both_required() {
        let question = test_message("me", "rite", "review rv-12?");
        let labels = vec!["ack".to_string()];
        let acked = test_message("reviewer", "rite", "heard")
            .with_reply_to(question.id)
            .with_labels(labels.clone());
        let unlabelled = test_message("reviewer", "rite", "heard").with_reply_to(question.id);

        let mut f = filters();
        f.labels = &labels;
        f.reply_to = Some(question.id);

        assert!(f.matches(&acked, "rite"));
        assert!(!f.matches(&unlabelled, "rite"));
    }

    /// A message that anchors to itself has no parent, so it cannot be its own
    /// acknowledgment — even when the waiter names its id.
    #[test]
    fn a_self_anchored_message_never_answers_itself() {
        let mut looped = test_message("reviewer", "rite", "me");
        looped.reply_to = Some(looped.id);

        assert!(!matches_reply(&looped, Some(looped.id)));
    }

    /// Without the flag nothing changes: any message still matches.
    #[test]
    fn without_reply_to_the_predicate_is_unchanged() {
        let msg = test_message("reviewer", "rite", "anything");
        assert!(matches_reply(&msg, None));
    }

    fn matches_reply(msg: &Message, parent: Option<Ulid>) -> bool {
        let mut f = filters();
        f.reply_to = parent;
        f.matches(msg, "rite")
    }
}
