use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use ulid::Ulid;

/// Fields a build did not recognise, kept verbatim so a rewrite re-emits them.
///
/// `hooks.jsonl` and `hook-queue.jsonl` are append-only with
/// latest-record-per-id wins, and several paths rewrite a *whole* record:
/// the `last_fired` bump on every firing, `rite hooks remove`, the channel
/// rename, and the queue entry's delivery mark. A build that predates a field
/// deserializes the record without it and then appends a copy that omits it —
/// so the newer configuration is deleted with no error, no warning, and no
/// diagnostic. Forward-compatible *reading* does not help: the record parses
/// fine, it just comes back short.
///
/// A `serde(flatten)` catch-all closes that. Anything this build cannot
/// interpret lands here and is written back out unchanged, so a rewrite can
/// only ever clobber fields the writer actually owns.
///
/// Declared **last** in every struct that carries one: flattened entries
/// serialize at the position of the field, so keeping it last leaves the key
/// order of a record with no unknown fields byte-identical to before. An
/// empty map emits nothing at all.
pub type UnknownFields = BTreeMap<String, serde_json::Value>;

/// Reserved claim scheme for hook spawn leases.
///
/// A lease is an ordinary claim in `claims.jsonl` — same file, same atomic
/// check-and-stake, visible in `rite claims list`. What makes it a lease is
/// only the scheme: rite derives the pattern
/// (`spawn://<hook-id>/<channel>`), so no human and no agent ever stakes one
/// by hand.
///
/// That reservation is load-bearing. `crate::cli::hooks` lets a *provably
/// abandoned* lease be stepped over (see the supersession rule there), and
/// confining that rule to a scheme rite owns guarantees it can never apply
/// to a claim someone else staked — `src/**`, `agent://foo`, `bone://…` keep
/// the ordinary, never-bypassed claim semantics.
pub const LEASE_SCHEME: &str = "spawn://";

/// Fallback lease TTL when neither the lease nor the hook's claim says.
///
/// The TTL is a backstop, not the primary release path: it bounds how long a
/// lease whose holder never reported presence at all can wedge a channel.
/// One hour is long enough not to cut a working agent off mid-task, and
/// short enough that a wedged channel recovers on its own.
pub const DEFAULT_LEASE_TTL_SECS: u64 = 3600;

/// Default cap on how many triggers one spawn is handed.
pub const DEFAULT_MAX_BATCH: usize = 50;

/// Build the lease pattern for a (hook, channel) pair.
///
/// The channel is the one that actually fired, not `hook.channel`, so a
/// wildcard (`*`) hook gets one lease per channel rather than one globally.
pub fn spawn_lease_pattern(hook_id: &str, channel: &str) -> String {
    format!("{}{}/{}", LEASE_SCHEME, hook_id, channel)
}

/// Per-(hook, channel) spawn lease: at most one live spawn at a time, with
/// triggers that arrive while it is held batched into the next spawn.
///
/// Opt-in per hook (`rite hooks add --lease`). Hooks without it keep the
/// wall-clock [`Hook::cooldown_secs`] behaviour untouched.
///
/// Plain struct on purpose — no tagged enum, so it cannot interact with the
/// `crate::core::wire` corruption rules — and every field is optional, so an
/// older rite ignores the whole thing and a newer one can add fields without
/// making this build reject the record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpawnLease {
    /// Seconds the lease may be held before it expires.
    /// Defaults to the hook's claim TTL, then [`DEFAULT_LEASE_TTL_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,

    /// Maximum triggers handed to a single spawn (default
    /// [`DEFAULT_MAX_BATCH`]). Anything over the cap stays queued for the
    /// spawn after that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_batch: Option<usize>,

    /// Lease settings this build does not know. See [`UnknownFields`].
    /// Keep last.
    #[serde(flatten)]
    pub extra: UnknownFields,
}

/// A hook trigger that arrived while the (hook, channel) spawn lease was
/// held, kept so the next spawn gets it instead of it being dropped.
///
/// Append-only with latest-record-per-`id` wins, exactly like claims: an
/// entry is written once with `delivered: false`, then re-written with
/// `delivered: true` once a spawn has actually been handed it. Deliberately
/// a flat struct with no variant tag, so it never reaches the
/// `crate::core::wire` known-tag/unknown-tag policy, and every added field
/// must stay `#[serde(default)]` so an older build can read a newer file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedTrigger {
    /// When the trigger was queued.
    pub ts: DateTime<Utc>,

    /// Queue entry ID (not the message ID — one message can queue for
    /// several hooks).
    pub id: Ulid,

    /// Hook the trigger is queued for.
    pub hook_id: String,

    /// Channel the trigger arrived on.
    pub channel: String,

    /// Message that triggered the hook.
    pub message_id: String,

    /// Agent that sent the triggering message.
    pub agent: String,

    /// Identity used to collapse duplicate triggers within one batch —
    /// same sender, same message body.
    pub dedup_key: String,

    /// False while queued; true on the record written when a spawn has been
    /// handed this trigger.
    #[serde(default)]
    pub delivered: bool,

    /// Queue-entry fields this build does not know, preserved across the
    /// delivery rewrite. See [`UnknownFields`]. Keep last.
    #[serde(flatten)]
    pub extra: UnknownFields,
}

impl QueuedTrigger {
    pub fn new(
        hook_id: impl Into<String>,
        channel: impl Into<String>,
        message_id: impl Into<String>,
        agent: impl Into<String>,
        dedup_key: impl Into<String>,
    ) -> Self {
        Self {
            ts: Utc::now(),
            id: Ulid::new(),
            hook_id: hook_id.into(),
            channel: channel.into(),
            message_id: message_id.into(),
            agent: agent.into(),
            dedup_key: dedup_key.into(),
            delivered: false,
            extra: UnknownFields::new(),
        }
    }

    /// The record that marks this entry as handed to a spawn.
    pub fn delivered(&self) -> Self {
        Self {
            delivered: true,
            ts: Utc::now(),
            ..self.clone()
        }
    }
}

/// A channel hook that triggers a command when a message is sent to a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hook {
    /// Terse ID (e.g., "hk-a3x")
    pub id: String,

    /// Channel that triggers this hook
    pub channel: String,

    /// Condition that must be met for the hook to fire
    pub condition: HookCondition,

    /// Command to execute (no shell — executed via execvp)
    pub command: Vec<String>,

    /// Working directory for the command
    pub cwd: PathBuf,

    /// Minimum seconds between firings.
    ///
    /// **Deprecated** in favour of [`Hook::lease`]: a wall clock cannot tell
    /// "a spawn is still running" from "a spawn finished early", so it
    /// double-spawns when set too short and silently drops everything that
    /// arrives inside the window when set too long. Still honoured exactly as
    /// before for every hook without a lease — which is every hook written by
    /// a rite that predates leases.
    pub cooldown_secs: u64,

    /// Last time this hook was fired
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fired: Option<DateTime<Utc>>,

    /// When this hook was created
    pub created_at: DateTime<Utc>,

    /// Agent that created the hook
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,

    /// How to release the claim after hook fires.
    /// None for hooks created before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_release: Option<ClaimRelease>,

    /// Explicit claim pattern for mention hooks (from --claim).
    /// ClaimAvailable hooks use their condition pattern instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_pattern: Option<String>,

    /// Agent that should own the claim (default: message sender).
    /// Useful when spawning an agent that needs to refresh its own claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_owner: Option<String>,

    /// Priority for hook execution (lower runs first, Unix convention).
    /// Default: 0
    #[serde(default)]
    pub priority: i32,

    /// Only fire this hook if the message contains the specified !flag.
    /// E.g., require_flag = "dev" means the hook only fires on messages containing "!dev".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_flag: Option<String>,

    /// Per-(hook, channel) spawn lease. `None` — the case for every hook
    /// written before this field existed — keeps the [`Hook::cooldown_secs`]
    /// behaviour verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<SpawnLease>,

    /// Whether this hook is active
    pub active: bool,

    /// Optional description for identification/deduplication
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Hook fields this build does not know, preserved across every rewrite
    /// of the record. See [`UnknownFields`].
    ///
    /// Keep this field last, and add new named fields *above* it, so a hook
    /// with nothing unknown still serializes exactly as it did before.
    #[serde(flatten)]
    pub extra: UnknownFields,
}

/// How to release the claim acquired when a hook fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaimRelease {
    /// Hold claim for a fixed duration (seconds).
    Ttl { secs: u64 },
    /// Release claim when the spawned command exits.
    OnExit,
}

/// Condition that must be met for a hook to fire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookCondition {
    /// Fire when no active claim holds the given pattern.
    ClaimAvailable { pattern: String },
    /// Fire when a message contains a specific @mention.
    MentionReceived { agent: String },
}

/// Audit record for a hook evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookFiring {
    /// When the evaluation happened
    pub ts: DateTime<Utc>,

    /// Hook that was evaluated
    pub hook_id: String,

    /// Channel that triggered the evaluation
    pub channel: String,

    /// Message ID that triggered the evaluation
    pub message_id: String,

    /// Whether the condition passed
    pub condition_result: bool,

    /// Whether the command was actually executed
    pub executed: bool,

    /// Reason the hook was skipped (cooldown, condition failed, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Format a command as a shell would display it, quoting args that need it.
pub fn shell_display(cmd: &[String]) -> String {
    cmd.iter()
        .map(|arg| {
            if arg.is_empty() {
                "''".to_string()
            } else if arg
                .chars()
                .any(|c| " \t\n\"'\\$`!#&|;(){}[]<>?*~".contains(c))
            {
                // Wrap in single quotes, escaping embedded single quotes
                format!("'{}'", arg.replace('\'', "'\\''"))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Hook {
    /// Generate a terse hook ID using ULID.
    ///
    /// Takes last 3 chars of ULID (Crockford base32), lowercased, prefixed with "hk-".
    /// Checks for collisions against existing IDs; extends length if needed.
    pub fn generate_id(existing_ids: &[String]) -> String {
        let ulid_str = ulid::Ulid::new().to_string().to_lowercase();
        let chars: Vec<char> = ulid_str.chars().collect();

        // Try 3 chars, then 4, then 5, etc.
        for len in 3..=ulid_str.len() {
            let suffix: String = chars[chars.len() - len..].iter().collect();
            let id = format!("hk-{}", suffix);
            if !existing_ids.contains(&id) {
                return id;
            }
        }

        // Fallback: full ULID (should never happen with < 100 hooks)
        format!("hk-{}", ulid_str)
    }

    /// Whether this hook uses a spawn lease instead of the wall-clock cooldown.
    pub fn uses_lease(&self) -> bool {
        self.lease.is_some()
    }

    /// Lease pattern for a firing on `channel`.
    pub fn lease_pattern(&self, channel: &str) -> String {
        spawn_lease_pattern(&self.id, channel)
    }

    /// Seconds a lease for this hook is held before it expires.
    ///
    /// Falls back to the hook's own claim TTL so a lease-enabled hook does
    /// not hold the channel longer than the claim it stakes alongside it.
    pub fn lease_ttl_secs(&self) -> u64 {
        self.lease
            .as_ref()
            .and_then(|l| l.ttl_secs)
            .or(match self.claim_release {
                Some(ClaimRelease::Ttl { secs }) => Some(secs),
                _ => None,
            })
            .unwrap_or(DEFAULT_LEASE_TTL_SECS)
            .max(1)
    }

    /// Maximum triggers handed to one spawn of this hook.
    pub fn lease_max_batch(&self) -> usize {
        self.lease
            .as_ref()
            .and_then(|l| l.max_batch)
            .unwrap_or(DEFAULT_MAX_BATCH)
            .max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_id_format() {
        let id = Hook::generate_id(&[]);
        assert!(id.starts_with("hk-"));
        // "hk-" + 3 chars = 6 total
        assert_eq!(id.len(), 6);
    }

    #[test]
    fn test_generate_id_avoids_collision() {
        // Generate first ID
        let id1 = Hook::generate_id(&[]);
        // Generate second ID with first as existing — should be different
        let id2 = Hook::generate_id(&[id1.clone()]);
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_hook_roundtrip() {
        let hook = Hook {
            id: "hk-abc".to_string(),
            channel: "deploy".to_string(),
            condition: HookCondition::ClaimAvailable {
                pattern: "agent://test-dev".to_string(),
            },
            command: vec!["echo".to_string(), "fired".to_string()],
            cwd: PathBuf::from("/tmp"),
            cooldown_secs: 30,
            last_fired: None,
            created_at: Utc::now(),
            created_by: Some("test-agent".to_string()),
            claim_release: Some(ClaimRelease::OnExit),
            claim_pattern: None,
            claim_owner: None,
            priority: 0,
            require_flag: None,
            lease: None,
            active: true,
            description: None,
            extra: Default::default(),
        };

        let json = serde_json::to_string(&hook).unwrap();
        let parsed: Hook = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "hk-abc");
        assert_eq!(parsed.channel, "deploy");
        assert!(parsed.active);
        assert!(parsed.claim_release.is_some());
    }

    #[test]
    fn test_hook_firing_roundtrip() {
        let firing = HookFiring {
            ts: Utc::now(),
            hook_id: "hk-abc".to_string(),
            channel: "deploy".to_string(),
            message_id: "01ABCDEF".to_string(),
            condition_result: true,
            executed: true,
            reason: None,
        };

        let json = serde_json::to_string(&firing).unwrap();
        let parsed: HookFiring = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.hook_id, "hk-abc");
        assert!(parsed.executed);
    }

    #[test]
    fn test_shell_display() {
        // Simple command
        assert_eq!(
            shell_display(&["echo".into(), "hello".into()]),
            "echo hello"
        );
        // Arg with spaces gets quoted
        assert_eq!(
            shell_display(&["echo".into(), "hello world".into()]),
            "echo 'hello world'"
        );
        // Arg with single quote gets escaped
        assert_eq!(
            shell_display(&["echo".into(), "it's".into()]),
            "echo 'it'\\''s'"
        );
        // Empty arg
        assert_eq!(shell_display(&["echo".into(), "".into()]), "echo ''");
        // No args
        assert_eq!(shell_display(&[]), "");
    }

    #[test]
    fn test_claim_release_serde() {
        let ttl = ClaimRelease::Ttl { secs: 300 };
        let json = serde_json::to_string(&ttl).unwrap();
        assert!(json.contains("\"type\":\"ttl\""));
        let parsed: ClaimRelease = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, ClaimRelease::Ttl { secs: 300 }));

        let on_exit = ClaimRelease::OnExit;
        let json = serde_json::to_string(&on_exit).unwrap();
        assert!(json.contains("\"type\":\"on_exit\""));
        let parsed: ClaimRelease = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, ClaimRelease::OnExit));
    }

    fn lease_hook(lease: Option<SpawnLease>) -> Hook {
        Hook {
            id: "hk-lse".to_string(),
            channel: "deploy".to_string(),
            condition: HookCondition::MentionReceived {
                agent: "worker".to_string(),
            },
            command: vec!["echo".to_string()],
            cwd: PathBuf::from("/tmp"),
            cooldown_secs: 30,
            last_fired: None,
            created_at: Utc::now(),
            created_by: None,
            claim_release: Some(ClaimRelease::Ttl { secs: 600 }),
            claim_pattern: None,
            claim_owner: None,
            priority: 0,
            require_flag: None,
            lease,
            active: true,
            description: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn test_lease_pattern_is_per_hook_and_channel() {
        let hook = lease_hook(Some(SpawnLease::default()));
        assert_eq!(hook.lease_pattern("rite"), "spawn://hk-lse/rite");
        // A wildcard hook leases per firing channel, never once globally.
        assert_ne!(hook.lease_pattern("rite"), hook.lease_pattern("manifold"));
        assert!(hook.lease_pattern("rite").starts_with(LEASE_SCHEME));
    }

    #[test]
    fn test_lease_ttl_falls_back_to_claim_ttl_then_default() {
        // Explicit lease TTL wins.
        let explicit = lease_hook(Some(SpawnLease {
            ttl_secs: Some(120),
            max_batch: None,
            extra: Default::default(),
        }));
        assert_eq!(explicit.lease_ttl_secs(), 120);

        // Otherwise the hook's own claim TTL, so the lease never outlives it.
        let from_claim = lease_hook(Some(SpawnLease::default()));
        assert_eq!(from_claim.lease_ttl_secs(), 600);

        // Otherwise the documented default backstop.
        let mut no_claim_ttl = lease_hook(Some(SpawnLease::default()));
        no_claim_ttl.claim_release = Some(ClaimRelease::OnExit);
        assert_eq!(no_claim_ttl.lease_ttl_secs(), DEFAULT_LEASE_TTL_SECS);
    }

    #[test]
    fn test_lease_max_batch_defaults_and_floor() {
        assert_eq!(
            lease_hook(Some(SpawnLease::default())).lease_max_batch(),
            DEFAULT_MAX_BATCH
        );
        // A zero cap would mean "deliver nothing", which is a silent drop.
        assert_eq!(
            lease_hook(Some(SpawnLease {
                ttl_secs: None,
                max_batch: Some(0),
                extra: Default::default(),
            }))
            .lease_max_batch(),
            1
        );
    }

    #[test]
    fn test_lease_omitted_when_absent() {
        let hook = lease_hook(None);
        assert!(!hook.uses_lease());
        let json = serde_json::to_string(&hook).unwrap();
        assert!(
            !json.contains("\"lease\""),
            "a hook without a lease must serialize byte-identically to before: {json}"
        );
    }

    #[test]
    fn test_lease_roundtrip() {
        let hook = lease_hook(Some(SpawnLease {
            ttl_secs: Some(900),
            max_batch: Some(10),
            extra: Default::default(),
        }));
        let json = serde_json::to_string(&hook).unwrap();
        let parsed: Hook = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.lease,
            Some(SpawnLease {
                ttl_secs: Some(900),
                max_batch: Some(10),
                extra: Default::default(),
            })
        );
    }

    /// A lease record written by a newer rite that has grown fields must not
    /// make this build reject the hook — the whole point of keeping the lease
    /// a flat, all-optional struct rather than a tagged variant.
    #[test]
    fn test_lease_tolerates_unknown_fields() {
        let json = r#"{"id":"hk-fut","channel":"test","condition":{"type":"claim_available","pattern":"x"},"command":["echo"],"cwd":"/tmp","cooldown_secs":30,"created_at":"2025-01-01T00:00:00Z","lease":{"ttl_secs":42,"steer_mode":"inject"},"active":true}"#;
        let hook: Hook = serde_json::from_str(json).unwrap();
        assert!(hook.uses_lease());
        assert_eq!(hook.lease_ttl_secs(), 42);
        assert_eq!(hook.lease_max_batch(), DEFAULT_MAX_BATCH);

        // Tolerating it is not enough — writing it back out is what stops a
        // rewrite from deleting it.
        let out: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&hook).unwrap()).unwrap();
        assert_eq!(out["lease"]["steer_mode"], "inject");
    }

    /// The bn-14o5 defect: a firing rewrites the whole hook record, so a
    /// field this build cannot interpret must come back out unchanged.
    #[test]
    fn test_unknown_hook_fields_survive_a_rewrite() {
        let json = r#"{"id":"hk-fut","channel":"test","condition":{"type":"claim_available","pattern":"x"},"command":["echo"],"cwd":"/tmp","cooldown_secs":30,"created_at":"2025-01-01T00:00:00Z","active":true,"steer_mode":"inject","future_limits":{"max_depth":3}}"#;
        let hook: Hook = serde_json::from_str(json).unwrap();
        assert_eq!(hook.extra.len(), 2, "unknown fields must be captured");

        // Exactly what a firing does: clone, bump last_fired, append.
        let mut rewritten = hook.clone();
        rewritten.last_fired = Some(Utc::now());
        let out: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rewritten).unwrap()).unwrap();

        assert_eq!(out["steer_mode"], "inject");
        assert_eq!(out["future_limits"]["max_depth"], 3);
        assert!(out["last_fired"].is_string());
        assert!(
            out.get("extra").is_none(),
            "the catch-all must flatten, never appear as a key: {out}"
        );
    }

    /// The catch-all takes leftover *fields*; it must not turn a record with
    /// a missing or wrong-typed required field into a readable hook, or
    /// `rite doctor` would stop reporting corruption.
    #[test]
    fn test_catch_all_does_not_accept_a_broken_record() {
        // No `id`.
        let missing = r#"{"channel":"test","condition":{"type":"claim_available","pattern":"x"},"command":["echo"],"cwd":"/tmp","cooldown_secs":30,"created_at":"2025-01-01T00:00:00Z","active":true}"#;
        assert!(serde_json::from_str::<Hook>(missing).is_err());

        // `command` is not a list of strings.
        let wrong_type = r#"{"id":"hk-bad","channel":"test","condition":{"type":"claim_available","pattern":"x"},"command":"echo","cwd":"/tmp","cooldown_secs":30,"created_at":"2025-01-01T00:00:00Z","active":true}"#;
        assert!(serde_json::from_str::<Hook>(wrong_type).is_err());
    }

    /// A hook with nothing unknown must serialize exactly as it did before
    /// the catch-all existed — the guarantee every hook already on disk needs.
    #[test]
    fn test_catch_all_adds_nothing_when_empty() {
        let json = serde_json::to_string(&lease_hook(None)).unwrap();
        assert!(!json.contains("extra"), "{json}");
        assert!(!json.contains("{}"), "{json}");
    }

    /// The queue entry is rewritten too, when a spawn is handed the trigger.
    #[test]
    fn test_unknown_queue_fields_survive_the_delivery_mark() {
        let json = r#"{"ts":"2025-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hook_id":"hk-abc","channel":"rite","message_id":"01ABC","agent":"sender","dedup_key":"deadbeef","priority":7}"#;
        let entry: QueuedTrigger = serde_json::from_str(json).unwrap();
        let out: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&entry.delivered()).unwrap()).unwrap();
        assert_eq!(out["priority"], 7);
        assert_eq!(out["delivered"], true);
    }

    #[test]
    fn test_queued_trigger_roundtrip_and_delivery() {
        let entry = QueuedTrigger::new("hk-abc", "rite", "01ABC", "sender", "deadbeef");
        assert!(!entry.delivered);

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: QueuedTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, entry.id);
        assert_eq!(parsed.hook_id, "hk-abc");
        assert!(!parsed.delivered);

        // The delivery record keeps the same entry ID so latest-wins
        // collapses the pair into one delivered entry.
        let delivered = entry.delivered();
        assert_eq!(delivered.id, entry.id);
        assert!(delivered.delivered);
    }

    /// A queue entry written by an older rite has no `delivered` field at
    /// all; it must read as still-pending rather than failing the parse.
    #[test]
    fn test_queued_trigger_missing_delivered_defaults_to_pending() {
        let json = r#"{"ts":"2025-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hook_id":"hk-abc","channel":"rite","message_id":"01ABC","agent":"sender","dedup_key":"deadbeef"}"#;
        let parsed: QueuedTrigger = serde_json::from_str(json).unwrap();
        assert!(!parsed.delivered);
    }

    #[test]
    fn test_hook_backward_compat_no_claim_release() {
        // Simulate old hook JSON without claim_release, created_by, priority, or description fields
        let json = r#"{"id":"hk-old","channel":"test","condition":{"type":"claim_available","pattern":"x"},"command":["echo"],"cwd":"/tmp","cooldown_secs":30,"created_at":"2025-01-01T00:00:00Z","active":true}"#;
        let hook: Hook = serde_json::from_str(json).unwrap();
        assert!(hook.claim_release.is_none());
        assert!(
            !hook.uses_lease(),
            "a hook that predates leases must keep its cooldown behaviour"
        );
        assert!(hook.created_by.is_none());
        assert_eq!(hook.priority, 0);
        assert!(hook.require_flag.is_none());
        assert!(hook.description.is_none());
    }

    #[test]
    fn test_description_roundtrip() {
        let hook = Hook {
            id: "hk-desc".to_string(),
            channel: "test".to_string(),
            condition: HookCondition::MentionReceived {
                agent: "test-agent".to_string(),
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
            description: Some("botbox:respond:general".to_string()),
            extra: Default::default(),
        };

        let json = serde_json::to_string(&hook).unwrap();
        assert!(json.contains("description"));
        assert!(json.contains("botbox:respond:general"));
        let parsed: Hook = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.description,
            Some("botbox:respond:general".to_string())
        );
    }

    #[test]
    fn test_description_omitted_when_none() {
        let hook = Hook {
            id: "hk-nodesc".to_string(),
            channel: "test".to_string(),
            condition: HookCondition::MentionReceived {
                agent: "test-agent".to_string(),
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
            extra: Default::default(),
        };

        let json = serde_json::to_string(&hook).unwrap();
        assert!(!json.contains("description"));
    }

    #[test]
    fn test_condition_serde() {
        let cond = HookCondition::ClaimAvailable {
            pattern: "agent://test".to_string(),
        };
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("claim_available"));
        let parsed: HookCondition = serde_json::from_str(&json).unwrap();
        match parsed {
            HookCondition::ClaimAvailable { pattern } => {
                assert_eq!(pattern, "agent://test");
            }
            HookCondition::MentionReceived { .. } => {
                panic!("Expected ClaimAvailable, got MentionReceived");
            }
        }
    }

    #[test]
    fn test_mention_condition_serde() {
        let cond = HookCondition::MentionReceived {
            agent: "security-reviewer".to_string(),
        };
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("mention_received"));
        assert!(json.contains("security-reviewer"));
        let parsed: HookCondition = serde_json::from_str(&json).unwrap();
        match parsed {
            HookCondition::MentionReceived { agent } => {
                assert_eq!(agent, "security-reviewer");
            }
            HookCondition::ClaimAvailable { .. } => {
                panic!("Expected MentionReceived, got ClaimAvailable");
            }
        }
    }

    #[test]
    fn test_require_flag_roundtrip() {
        let hook = Hook {
            id: "hk-flg".to_string(),
            channel: "deploy".to_string(),
            condition: HookCondition::ClaimAvailable {
                pattern: "agent://test-dev".to_string(),
            },
            command: vec!["echo".to_string(), "fired".to_string()],
            cwd: PathBuf::from("/tmp"),
            cooldown_secs: 30,
            last_fired: None,
            created_at: Utc::now(),
            created_by: None,
            claim_release: Some(ClaimRelease::OnExit),
            claim_pattern: None,
            claim_owner: None,
            priority: 0,
            require_flag: Some("dev".to_string()),
            lease: None,
            active: true,
            description: None,
            extra: Default::default(),
        };

        let json = serde_json::to_string(&hook).unwrap();
        assert!(json.contains("require_flag"));
        assert!(json.contains("dev"));
        let parsed: Hook = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.require_flag, Some("dev".to_string()));
    }

    #[test]
    fn test_require_flag_omitted_when_none() {
        let hook = Hook {
            id: "hk-nfl".to_string(),
            channel: "deploy".to_string(),
            condition: HookCondition::ClaimAvailable {
                pattern: "agent://test-dev".to_string(),
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
            extra: Default::default(),
        };

        let json = serde_json::to_string(&hook).unwrap();
        assert!(!json.contains("require_flag"));
    }
}
