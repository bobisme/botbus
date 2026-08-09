use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use ulid::Ulid;

use crate::core::wire::{self, ForwardCompatible};

/// The fundamental unit of communication in Rite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Timestamp when the message was created
    pub ts: DateTime<Utc>,

    /// Unique identifier (ULID for sortability without coordination)
    pub id: Ulid,

    /// Name of the sending agent
    pub agent: String,

    /// Channel name, or "_dm_{agent1}_{agent2}" for DMs (names sorted)
    pub channel: String,

    /// Message content (markdown supported)
    pub body: String,

    /// Extracted @mentions for potential notifications
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,

    /// Optional labels for categorization/filtering
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,

    /// The message this one answers, if any.
    ///
    /// Absent means top-level. That is every message written before this field
    /// existed and every message that is not a reply, so the flat transcript
    /// stays the untouched default path.
    ///
    /// # Wire format
    ///
    /// The field is skipped when `None`, so a non-reply is byte-identical on
    /// the wire to a record written by a build that never heard of threading.
    /// An older rite ignores the extra key like any unknown field (`Message`
    /// does not deny unknown fields), so it reads a reply as a plain message.
    ///
    /// A value this build cannot read degrades to `None` rather than failing
    /// the parse. This is deliberate and it is *not* the tagged-enum policy in
    /// [`crate::core::wire`]: an anchor is a pointer, not payload. Dropping an
    /// unreadable pointer costs one message's thread position; rejecting the
    /// record would cost the message itself, in every reader, for good. Losing
    /// the anchor lands the message exactly where it would have been before
    /// threading existed.
    ///
    /// The drop is counted and reported — see [`deserialize_reply_to`]. It is
    /// degradation, not silence.
    #[serde(
        default,
        deserialize_with = "deserialize_reply_to",
        skip_serializing_if = "Option::is_none"
    )]
    pub reply_to: Option<Ulid>,

    /// Optional file attachments
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,

    /// Optional structured metadata
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<MessageMeta>,
}

impl Message {
    /// Create a new message with the current timestamp and a fresh ULID.
    pub fn new(
        agent: impl Into<String>,
        channel: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        let body = body.into();
        let mentions = extract_mentions(&body);

        Self {
            ts: Utc::now(),
            id: Ulid::new(),
            agent: agent.into(),
            channel: channel.into(),
            body,
            mentions,
            labels: Vec::new(),
            reply_to: None,
            attachments: Vec::new(),
            meta: None,
        }
    }

    /// Create a new message with metadata.
    pub fn with_meta(mut self, meta: MessageMeta) -> Self {
        self.meta = Some(meta);
        self
    }

    /// Anchor this message to the message it answers.
    pub fn with_reply_to(mut self, parent: Ulid) -> Self {
        self.reply_to = Some(parent);
        self
    }

    /// True when this message carries a reply anchor.
    pub fn is_reply(&self) -> bool {
        self.parent_id().is_some()
    }

    /// The parent this message answers, with self-reference removed.
    ///
    /// A message that points at itself has no parent. Use this instead of
    /// reading `reply_to` directly wherever the value feeds a walk, an index
    /// row, or a children map — a self-edge there loops forever.
    pub fn parent_id(&self) -> Option<Ulid> {
        match self.reply_to {
            Some(parent) if parent == self.id => None,
            other => other,
        }
    }

    /// Add labels to the message.
    pub fn with_labels(mut self, labels: Vec<String>) -> Self {
        self.labels = labels;
        self
    }

    /// Add attachments to the message.
    pub fn with_attachments(mut self, attachments: Vec<Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Check if message has a specific label.
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }

    /// Check if message has any of the specified labels.
    pub fn has_any_label(&self, labels: &[String]) -> bool {
        labels.iter().any(|l| self.has_label(l))
    }
}

/// Read `reply_to` without ever failing the record.
///
/// A ULID string becomes the anchor. Anything else — a shape a newer rite gave
/// the field, a value mangled by a bad merge — is dropped so the message still
/// reads as top-level instead of vanishing from the channel. See the field docs
/// on [`Message::reply_to`] for why this differs from the tagged-enum policy in
/// [`crate::core::wire`].
///
/// Dropping is never silent. Each loss is handed to
/// [`crate::storage::jsonl::report_damaged_field`], which surfaces it the same
/// way an unreadable line is surfaced: one deduped stderr note per file per
/// process, and a count in `rite doctor`. That matters more than the usual
/// tidiness argument: a dropped anchor demotes a reply to a top-level message,
/// so an acknowledgment stops correlating with the request it answers, a
/// waiter times out, and the requester re-posts. Losing anchors quietly
/// recreates the duplicate-request problem threading exists to remove.
fn deserialize_reply_to<'de, D>(deserializer: D) -> Result<Option<Ulid>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;

    Ok(match raw {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => match text.parse::<Ulid>() {
            Ok(parent) => Some(parent),
            Err(_) => {
                crate::storage::jsonl::report_damaged_field(
                    REPLY_TO_FIELD,
                    format!("{:?} (not a ULID)", text),
                );
                None
            }
        },
        Some(other) => {
            crate::storage::jsonl::report_damaged_field(
                REPLY_TO_FIELD,
                format!("{} (not a ULID string)", other),
            );
            None
        }
    })
}

/// Name reported for a dropped reply anchor. Shared so a test cannot drift
/// from what the reader actually emits.
pub const REPLY_TO_FIELD: &str = "reply_to";

/// File attachment on a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Display name for the attachment
    pub name: String,

    /// Type of attachment
    #[serde(flatten)]
    pub content: AttachmentContent,
}

/// Content of an attachment - either a file reference or inline content.
///
/// Carries an [`AttachmentContent::Unknown`] fallback so an attachment type
/// added by a newer rite does not make the whole message unreadable. A
/// *recognized* type with a broken body is still a hard error — see
/// [`crate::core::wire`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", remote = "Self")]
pub enum AttachmentContent {
    /// Reference to a file path (relative to project root)
    File { path: String },

    /// Inline text content (for small snippets)
    Inline {
        content: String,
        /// Optional language hint for syntax highlighting
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },

    /// URL reference
    Url { url: String },

    /// An attachment type this build does not recognize.
    ///
    /// The raw JSON is kept verbatim so re-serializing does not lose data.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ForwardCompatible for AttachmentContent {
    const WIRE_NAME: &'static str = "attachment";
    const KNOWN_TAGS: &'static [&'static str] = &["file", "inline", "url"];

    fn tag(value: &serde_json::Value) -> Option<&str> {
        wire::internal_tag(value, "type")
    }

    fn parse_known(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        AttachmentContent::deserialize(value)
    }

    fn unknown(value: serde_json::Value) -> Self {
        AttachmentContent::Unknown(value)
    }

    fn is_unknown(&self) -> bool {
        matches!(self, AttachmentContent::Unknown(_))
    }
}

impl Serialize for AttachmentContent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        AttachmentContent::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for AttachmentContent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        wire::deserialize(deserializer)
    }
}

impl Attachment {
    /// Create a file attachment.
    pub fn file(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            content: AttachmentContent::File { path: path.into() },
        }
    }

    /// Create an inline content attachment.
    pub fn inline(
        name: impl Into<String>,
        content: impl Into<String>,
        language: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            content: AttachmentContent::Inline {
                content: content.into(),
                language,
            },
        }
    }

    /// Create a URL attachment.
    pub fn url(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            content: AttachmentContent::Url { url: url.into() },
        }
    }

    /// Check if the attachment content is available locally.
    ///
    /// Returns `true` for inline content and URLs (always available).
    /// For file attachments, checks if the file exists on disk.
    /// This is useful after git sync, where attachments may be missing.
    pub fn is_available(&self) -> bool {
        match &self.content {
            AttachmentContent::File { path } => std::path::Path::new(path).exists(),
            AttachmentContent::Inline { .. } => true,
            AttachmentContent::Url { .. } => true,
            // Nothing local to fetch, and nothing this build can render.
            AttachmentContent::Unknown(_) => false,
        }
    }
}

/// Structured metadata for special message types.
///
/// # Forward compatibility
///
/// Rite instances sync channel files over git, so an older build routinely
/// reads records written by a newer one. Every tagged enum on the wire
/// therefore carries an `Unknown` fallback: an unrecognized `type` deserializes
/// into [`MessageMeta::Unknown`] holding the raw JSON, instead of failing the
/// parse (and, before this, the entire file read). Ignore what you do not
/// understand; never drop it.
///
/// This leniency stops at the tag. A `type` this build *does* know, carrying a
/// body it cannot read, is corruption and still fails the parse loudly — see
/// [`crate::core::wire`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", remote = "Self")]
pub enum MessageMeta {
    /// Agent claimed files for editing
    Claim {
        patterns: Vec<String>,
        ttl_secs: u64,
    },

    /// Agent extended an existing claim
    ClaimExtended {
        patterns: Vec<String>,
        ttl_secs: u64,
    },

    /// Agent released file claims
    Release { patterns: Vec<String> },

    /// System event (agent joined, etc.)
    System { event: SystemEvent },

    /// Tombstone: marks a message as deleted (append-only deletion)
    Deleted {
        target_id: Ulid,
        deleted_by: String,
        deleted_at: DateTime<Utc>,
    },

    /// Metadata written by a newer rite that this build does not understand.
    ///
    /// Holds the raw JSON verbatim, so the record survives a round-trip and
    /// stays inspectable (`rite messages get --format json`).
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ForwardCompatible for MessageMeta {
    const WIRE_NAME: &'static str = "message meta";
    const KNOWN_TAGS: &'static [&'static str] =
        &["claim", "claim_extended", "release", "system", "deleted"];

    fn tag(value: &serde_json::Value) -> Option<&str> {
        wire::internal_tag(value, "type")
    }

    fn parse_known(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        MessageMeta::deserialize(value)
    }

    fn unknown(value: serde_json::Value) -> Self {
        MessageMeta::Unknown(value)
    }

    fn is_unknown(&self) -> bool {
        matches!(self, MessageMeta::Unknown(_))
    }
}

impl Serialize for MessageMeta {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        MessageMeta::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for MessageMeta {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        wire::deserialize(deserializer)
    }
}

/// System events carried by [`MessageMeta::System`].
///
/// Same forward-compatibility contract as [`MessageMeta`]: an unrecognized
/// event kind becomes [`SystemEvent::Unknown`]; a recognized one with a broken
/// body is corruption and fails the parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", remote = "Self")]
pub enum SystemEvent {
    AgentRegistered,
    AgentRenamed {
        old_name: String,
    },
    ClaimExpired {
        patterns: Vec<String>,
    },
    HookFired {
        hook_id: String,
        command: Vec<String>,
    },
    /// A system event this build does not recognize, kept verbatim.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ForwardCompatible for SystemEvent {
    const WIRE_NAME: &'static str = "system event";
    const KNOWN_TAGS: &'static [&'static str] = &[
        "agent_registered",
        "agent_renamed",
        "claim_expired",
        "hook_fired",
    ];

    fn tag(value: &serde_json::Value) -> Option<&str> {
        wire::external_tag(value)
    }

    fn parse_known(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        SystemEvent::deserialize(value)
    }

    fn unknown(value: serde_json::Value) -> Self {
        SystemEvent::Unknown(value)
    }

    fn is_unknown(&self) -> bool {
        matches!(self, SystemEvent::Unknown(_))
    }
}

impl Serialize for SystemEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SystemEvent::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for SystemEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        wire::deserialize(deserializer)
    }
}

impl Message {
    /// Returns true if this message is a deletion tombstone.
    pub fn is_tombstone(&self) -> bool {
        matches!(&self.meta, Some(MessageMeta::Deleted { .. }))
    }

    /// If this message is a tombstone, returns the target message ID.
    pub fn tombstone_target_id(&self) -> Option<Ulid> {
        match &self.meta {
            Some(MessageMeta::Deleted { target_id, .. }) => Some(*target_id),
            _ => None,
        }
    }
}

/// Read messages from a JSONL file, filtering out deleted messages and their tombstones.
///
/// Two-pass approach:
/// 1. Collect all tombstone target_ids into a HashSet
/// 2. Filter out both the tombstone records AND the deleted originals
///
/// Use this everywhere instead of raw `read_records::<Message>` for user-facing reads.
pub fn read_messages(path: &Path) -> anyhow::Result<Vec<Message>> {
    let all: Vec<Message> = crate::storage::jsonl::read_records(path)?;
    Ok(filter_deleted(all))
}

/// Read the last N live messages from a JSONL file (after filtering deletions).
///
/// Reads all records, filters deleted messages, then takes the last N.
pub fn read_last_n_messages(path: &Path, n: usize) -> anyhow::Result<Vec<Message>> {
    let live = read_messages(path)?;
    let start = live.len().saturating_sub(n);
    Ok(live.into_iter().skip(start).collect())
}

/// Read messages from a JSONL file starting at a byte offset, filtering out deleted messages
/// and their tombstones.
///
/// Returns the filtered messages and the new byte offset.
/// Note: This only filters deletions within the newly-read portion. For full correctness
/// when tombstones may reference messages before the offset, callers should use `read_messages()`
/// for full reads.
pub fn read_messages_from_offset(path: &Path, offset: u64) -> anyhow::Result<(Vec<Message>, u64)> {
    let (all, new_offset): (Vec<Message>, u64) =
        crate::storage::jsonl::read_records_from_offset(path, offset)?;
    Ok((filter_deleted(all), new_offset))
}

/// Read up to `limit` messages from a JSONL file starting at a byte offset,
/// filtering out deleted messages and their tombstones.
pub fn read_messages_from_offset_limited(
    path: &Path,
    offset: u64,
    limit: usize,
) -> anyhow::Result<(Vec<Message>, u64)> {
    let (all, new_offset): (Vec<Message>, u64) =
        crate::storage::jsonl::read_records_from_offset_limited(path, offset, Some(limit))?;
    Ok((filter_deleted(all), new_offset))
}

/// Byte offset immediately after the message with the given `id`.
///
/// Uses the same cursor semantics as [`read_messages_from_offset`], so the
/// returned value can be passed straight back as a continuation offset. Scans
/// raw records (including tombstones) so a since-deleted id can still anchor
/// pagination. Returns `None` if no record with that id exists in the file.
pub fn offset_after_message_id(path: &Path, id: &str) -> anyhow::Result<Option<u64>> {
    use anyhow::Context;
    use std::io::{BufRead, BufReader, Seek};

    if !path.exists() {
        return Ok(None);
    }

    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open file: {}", path.display()))?;
    file.lock_shared()
        .with_context(|| format!("Failed to acquire shared lock on: {}", path.display()))?;

    let mut reader = BufReader::new(&file);
    loop {
        let mut line = String::new();
        let bytes_read = reader
            .read_line(&mut line)
            .with_context(|| format!("Failed to read from: {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }

        // Cursor position after this line, matching read_records_from_offset.
        let pos = reader.stream_position()?;

        if line.trim().is_empty() {
            continue;
        }

        // An unreadable line must not abort the scan; skip it and keep looking.
        match serde_json::from_str::<Message>(line.trim()) {
            Ok(msg) if msg.id.to_string() == id => return Ok(Some(pos)),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                path = %path.display(),
                byte_offset = pos,
                error = %error,
                "skipping unreadable JSONL record while seeking message id"
            ),
        }
    }

    Ok(None)
}

/// Filter out deleted messages and their tombstones from a vec of messages.
fn filter_deleted(messages: Vec<Message>) -> Vec<Message> {
    // Pass 1: collect all tombstone target IDs
    let deleted_ids: HashSet<Ulid> = messages
        .iter()
        .filter_map(|m| m.tombstone_target_id())
        .collect();

    if deleted_ids.is_empty() {
        return messages;
    }

    // Pass 2: filter out tombstones and deleted originals
    messages
        .into_iter()
        .filter(|m| {
            // Exclude tombstone records themselves
            if m.is_tombstone() {
                return false;
            }
            // Exclude messages targeted by a tombstone
            !deleted_ids.contains(&m.id)
        })
        .collect()
}

/// Extract @mentions from message body.
fn extract_mentions(body: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let mut chars = body.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '@' {
            let mut name = String::new();
            while let Some(&next) = chars.peek() {
                if next.is_alphanumeric() || next == '_' || next == '-' {
                    name.push(chars.next().unwrap());
                } else {
                    break;
                }
            }
            if !name.is_empty() {
                mentions.push(name);
            }
        }
    }

    mentions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_roundtrip() {
        let msg = Message::new("TestAgent", "general", "Hello, world!");

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(msg.id, parsed.id);
        assert_eq!(msg.body, parsed.body);
        assert_eq!(msg.agent, parsed.agent);
        assert_eq!(msg.channel, parsed.channel);
    }

    #[test]
    fn test_offset_after_message_id() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("channel.jsonl");
        let m1 = Message::new("agent", "channel", "one");
        let m2 = Message::new("agent", "channel", "two");
        crate::storage::jsonl::append_record(&path, &m1).unwrap();
        crate::storage::jsonl::append_record(&path, &m2).unwrap();

        // The offset after m1 is a continuation cursor: reading from it yields m2.
        let offset = offset_after_message_id(&path, &m1.id.to_string())
            .unwrap()
            .expect("m1 should be found");
        let (rest, _) = read_messages_from_offset(&path, offset).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].body, "two");

        // Unknown id → None.
        assert!(
            offset_after_message_id(&path, &Ulid::nil().to_string())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_reply_to_round_trips() {
        let parent = Message::new("asker", "general", "who owns review 42?");
        let reply = Message::new("answerer", "general", "I do").with_reply_to(parent.id);

        let json = serde_json::to_string(&reply).unwrap();
        assert!(json.contains("reply_to"));

        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.reply_to, Some(parent.id));
        assert_eq!(parsed.parent_id(), Some(parent.id));
        assert!(parsed.is_reply());
    }

    /// The flat path must be untouched: a message with no anchor writes no
    /// `reply_to` key, so its bytes match what a pre-threading rite wrote.
    #[test]
    fn test_absent_reply_to_is_not_serialized() {
        let msg = Message::new("agent", "general", "top level");
        let json = serde_json::to_string(&msg).unwrap();

        assert!(!json.contains("reply_to"), "{}", json);
        assert!(!msg.is_reply());
        assert_eq!(msg.parent_id(), None);

        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.reply_to, None);
    }

    /// Every record ever written lacks the field. Reading one must yield a
    /// top-level message, not an error.
    #[test]
    fn test_record_without_reply_to_reads_as_top_level() {
        let legacy = r#"{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"old","channel":"general","body":"before threading"}"#;

        let parsed: Message = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.reply_to, None);
        assert!(!parsed.is_reply());
    }

    /// An anchor this build cannot read costs the message its thread position,
    /// never the message. Contrast `test_corrupt_known_meta_is_an_error`: a
    /// damaged tagged enum still fails loudly.
    #[test]
    fn test_unreadable_reply_to_degrades_to_top_level() {
        for bad in [
            r#""not-a-ulid""#,
            r#"{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","channel":"general"}"#,
            "42",
            "null",
        ] {
            let line = format!(
                r#"{{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"future","channel":"general","body":"hello","reply_to":{}}}"#,
                bad
            );
            let parsed: Message = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("reply_to {bad} must not fail the record: {e}"));
            assert_eq!(parsed.reply_to, None, "reply_to {bad}");
            assert_eq!(parsed.body, "hello");
        }
    }

    /// A reply must also survive the fields a newer rite may add next to it.
    #[test]
    fn test_reply_to_coexists_with_unknown_fields() {
        let line = r#"{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FBW","agent":"future","channel":"general","body":"answer","reply_to":"01ARZ3NDEKTSV4RRFFQ69G5FAV","thread_root":"01ARZ3NDEKTSV4RRFFQ69G5FAV","priority":"high"}"#;

        let parsed: Message = serde_json::from_str(line).unwrap();
        assert_eq!(
            parsed.reply_to,
            Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".parse::<Ulid>().unwrap())
        );
    }

    /// A message pointing at itself has no parent. Anything that walks or
    /// indexes anchors must see `None`, or it loops.
    #[test]
    fn test_self_reference_is_not_a_parent() {
        let mut msg = Message::new("agent", "general", "me");
        msg.reply_to = Some(msg.id);

        assert_eq!(msg.parent_id(), None);
        assert!(!msg.is_reply());
        // The raw value is preserved so a diagnostic can still show it.
        assert_eq!(msg.reply_to, Some(msg.id));
    }

    /// A reply in a channel file must not disturb its neighbours, and a
    /// neighbour with a damaged anchor must not take the channel down.
    #[test]
    fn test_replies_and_damaged_anchors_share_a_channel() {
        use std::io::Write;
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("channel.jsonl");

        let parent = Message::new("asker", "channel", "question");
        let reply = Message::new("answerer", "channel", "answer").with_reply_to(parent.id);
        crate::storage::jsonl::append_record(&path, &parent).unwrap();
        crate::storage::jsonl::append_record(&path, &reply).unwrap();

        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(
                file,
                r#"{{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"mangled","channel":"channel","body":"bad anchor","reply_to":"????"}}"#
            )
            .unwrap();
        }

        let (messages, issues) =
            crate::storage::jsonl::read_records_reporting::<Message>(&path).unwrap();

        assert!(
            issues.skipped.is_empty(),
            "no record may be treated as corrupt"
        );
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].reply_to, Some(parent.id));
        assert_eq!(messages[2].reply_to, None);

        // The dropped anchor is *counted*, not merely absent. A reply that
        // quietly becomes top-level is how an acknowledgment stops
        // correlating; the read has to say it happened.
        assert_eq!(issues.damaged.len(), 1);
        let damaged = &issues.damaged[0];
        assert_eq!(damaged.field, REPLY_TO_FIELD);
        assert_eq!(damaged.line, Some(3));
        assert_eq!(damaged.path, path);
        assert!(damaged.value.contains("????"), "{}", damaged.value);
        assert!(damaged.to_string().contains("reply_to"));
    }

    /// A run of clean reads must not leave stale damage behind for the next
    /// one to claim as its own.
    #[test]
    fn test_a_clean_channel_reports_no_damage() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("channel.jsonl");

        let parent = Message::new("asker", "channel", "question");
        let reply = Message::new("answerer", "channel", "answer").with_reply_to(parent.id);
        crate::storage::jsonl::append_record(&path, &parent).unwrap();
        crate::storage::jsonl::append_record(&path, &reply).unwrap();

        let (messages, issues) =
            crate::storage::jsonl::read_records_reporting::<Message>(&path).unwrap();
        assert_eq!(messages.len(), 2);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn test_extract_mentions() {
        assert_eq!(
            extract_mentions("Hello @Alice and @Bob"),
            vec!["Alice", "Bob"]
        );
        assert_eq!(extract_mentions("No mentions here"), Vec::<String>::new());
        assert_eq!(extract_mentions("@SingleMention"), vec!["SingleMention"]);
        assert_eq!(extract_mentions("Email test@example.com"), vec!["example"]);
        // Test hyphenated agent names (kebab-case)
        assert_eq!(
            extract_mentions("Hey @iron-bear and @swift-falcon"),
            vec!["iron-bear", "swift-falcon"]
        );
        assert_eq!(
            extract_mentions("@multi-word-agent-name test"),
            vec!["multi-word-agent-name"]
        );
    }

    #[test]
    fn test_message_with_meta() {
        let msg =
            Message::new("Agent", "general", "Claiming files").with_meta(MessageMeta::Claim {
                patterns: vec!["src/**/*.rs".to_string()],
                ttl_secs: 3600,
            });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("claim"));
        assert!(json.contains("src/**/*.rs"));
    }

    #[test]
    fn test_message_with_labels() {
        let msg = Message::new("Agent", "general", "Bug fix ready")
            .with_labels(vec!["bug".to_string(), "ready-for-review".to_string()]);

        assert!(msg.has_label("bug"));
        assert!(msg.has_label("ready-for-review"));
        assert!(!msg.has_label("feature"));

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("labels"));
        assert!(json.contains("bug"));

        // Round-trip
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.labels, vec!["bug", "ready-for-review"]);
    }

    #[test]
    fn test_message_with_attachments() {
        let msg = Message::new("Agent", "general", "See attached").with_attachments(vec![
            Attachment::file("config", "src/config.rs"),
            Attachment::inline("snippet", "fn main() {}", Some("rust".to_string())),
            Attachment::url("docs", "https://example.com/docs"),
        ]);

        assert_eq!(msg.attachments.len(), 3);

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("attachments"));
        assert!(json.contains("src/config.rs"));
        assert!(json.contains("fn main()"));

        // Round-trip
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.attachments.len(), 3);
    }

    #[test]
    fn test_has_any_label() {
        let msg = Message::new("Agent", "general", "Test")
            .with_labels(vec!["bug".to_string(), "urgent".to_string()]);

        assert!(msg.has_any_label(&["bug".to_string(), "feature".to_string()]));
        assert!(msg.has_any_label(&["urgent".to_string()]));
        assert!(!msg.has_any_label(&["feature".to_string(), "docs".to_string()]));
        assert!(!msg.has_any_label(&[]));
    }

    #[test]
    fn test_labels_not_serialized_when_empty() {
        let msg = Message::new("Agent", "general", "No labels");
        let json = serde_json::to_string(&msg).unwrap();
        // Empty vecs should not appear in JSON output
        assert!(!json.contains("\"labels\""));
        assert!(!json.contains("\"attachments\""));
    }

    /// A record written by a future rite must deserialize into `Unknown`
    /// rather than failing the parse, and must survive a round-trip verbatim.
    #[test]
    fn test_unknown_message_meta_round_trips() {
        let future = r#"{"type":"reaction","emoji":"+1","target_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}"#;

        let meta: MessageMeta =
            serde_json::from_str(future).expect("unknown meta type must not be a parse error");
        assert!(matches!(meta, MessageMeta::Unknown(_)));

        // Re-serializing preserves the original payload exactly.
        let round_tripped = serde_json::to_value(&meta).unwrap();
        let original: serde_json::Value = serde_json::from_str(future).unwrap();
        assert_eq!(round_tripped, original);
    }

    /// The tolerance stops at the tag: a `type` this build knows, carrying a
    /// body it cannot read, is corruption and must NOT become `Unknown`.
    #[test]
    fn test_corrupt_known_meta_is_an_error() {
        // `claim` is a known tag, but `patterns`/`ttl_secs` are missing.
        let error = serde_json::from_str::<MessageMeta>(r#"{"type":"claim"}"#)
            .expect_err("a damaged known variant must not deserialize");
        assert!(error.to_string().contains("corrupt"), "{}", error);
        assert!(error.to_string().contains("claim"), "{}", error);

        // Wrong field types count too.
        assert!(
            serde_json::from_str::<MessageMeta>(
                r#"{"type":"claim","patterns":"not-a-list","ttl_secs":1}"#
            )
            .is_err()
        );

        // A nested system event with a damaged body fails the outer record.
        assert!(
            serde_json::from_str::<MessageMeta>(
                r#"{"type":"system","event":{"agent_renamed":{}}}"#
            )
            .is_err()
        );

        // And a record with no tag at all is not a shape rite ever writes.
        let error = serde_json::from_str::<MessageMeta>(r#"{"patterns":["a"]}"#)
            .expect_err("a record without a tag must not deserialize");
        assert!(error.to_string().contains("no variant tag"), "{}", error);
    }

    #[test]
    fn test_corrupt_known_system_event_is_an_error() {
        // Known tag, missing `old_name`.
        assert!(serde_json::from_str::<SystemEvent>(r#"{"agent_renamed":{}}"#).is_err());
        // Unknown tag with the same shape stays benign.
        assert!(matches!(
            serde_json::from_str::<SystemEvent>(r#"{"agent_paused":{}}"#).unwrap(),
            SystemEvent::Unknown(_)
        ));
    }

    #[test]
    fn test_corrupt_known_attachment_is_an_error() {
        // `file` is known but `path` is missing.
        assert!(serde_json::from_str::<Attachment>(r#"{"name":"a","type":"file"}"#).is_err());
        // An unknown type with the same missing field is still benign.
        assert!(matches!(
            serde_json::from_str::<Attachment>(r#"{"name":"a","type":"video"}"#)
                .unwrap()
                .content,
            AttachmentContent::Unknown(_)
        ));
    }

    /// Guard against `KNOWN_TAGS` drifting away from the variants. The match is
    /// exhaustive, so adding a variant fails to compile until it is listed.
    #[test]
    fn test_message_meta_known_tags_match_variants() {
        fn tag_of(meta: &MessageMeta) -> Option<&'static str> {
            match meta {
                MessageMeta::Claim { .. } => Some("claim"),
                MessageMeta::ClaimExtended { .. } => Some("claim_extended"),
                MessageMeta::Release { .. } => Some("release"),
                MessageMeta::System { .. } => Some("system"),
                MessageMeta::Deleted { .. } => Some("deleted"),
                MessageMeta::Unknown(_) => None,
            }
        }

        let samples = vec![
            MessageMeta::Claim {
                patterns: vec!["a".to_string()],
                ttl_secs: 1,
            },
            MessageMeta::ClaimExtended {
                patterns: vec!["a".to_string()],
                ttl_secs: 1,
            },
            MessageMeta::Release {
                patterns: vec!["a".to_string()],
            },
            MessageMeta::System {
                event: SystemEvent::AgentRegistered,
            },
            MessageMeta::Deleted {
                target_id: Ulid::nil(),
                deleted_by: "a".to_string(),
                deleted_at: Utc::now(),
            },
        ];
        assert_eq!(
            samples.len(),
            MessageMeta::KNOWN_TAGS.len(),
            "every known tag needs a sample here"
        );

        for sample in &samples {
            let tag = tag_of(sample).expect("samples are known variants");
            assert!(
                MessageMeta::KNOWN_TAGS.contains(&tag),
                "missing tag: {}",
                tag
            );

            let value = serde_json::to_value(sample).unwrap();
            assert_eq!(
                MessageMeta::tag(&value),
                Some(tag),
                "wire tag does not match KNOWN_TAGS entry"
            );
            let parsed: MessageMeta = serde_json::from_value(value).unwrap();
            assert!(!parsed.is_unknown(), "{} fell into the fallback", tag);
        }
    }

    #[test]
    fn test_system_event_known_tags_match_variants() {
        fn tag_of(event: &SystemEvent) -> Option<&'static str> {
            match event {
                SystemEvent::AgentRegistered => Some("agent_registered"),
                SystemEvent::AgentRenamed { .. } => Some("agent_renamed"),
                SystemEvent::ClaimExpired { .. } => Some("claim_expired"),
                SystemEvent::HookFired { .. } => Some("hook_fired"),
                SystemEvent::Unknown(_) => None,
            }
        }

        let samples = vec![
            SystemEvent::AgentRegistered,
            SystemEvent::AgentRenamed {
                old_name: "a".to_string(),
            },
            SystemEvent::ClaimExpired {
                patterns: vec!["a".to_string()],
            },
            SystemEvent::HookFired {
                hook_id: "hk-1".to_string(),
                command: vec!["echo".to_string()],
            },
        ];
        assert_eq!(samples.len(), SystemEvent::KNOWN_TAGS.len());

        for sample in &samples {
            let tag = tag_of(sample).expect("samples are known variants");
            assert!(
                SystemEvent::KNOWN_TAGS.contains(&tag),
                "missing tag: {}",
                tag
            );

            let value = serde_json::to_value(sample).unwrap();
            assert_eq!(SystemEvent::tag(&value), Some(tag));
            let parsed: SystemEvent = serde_json::from_value(value).unwrap();
            assert!(!parsed.is_unknown(), "{} fell into the fallback", tag);
        }
    }

    #[test]
    fn test_attachment_content_known_tags_match_variants() {
        fn tag_of(content: &AttachmentContent) -> Option<&'static str> {
            match content {
                AttachmentContent::File { .. } => Some("file"),
                AttachmentContent::Inline { .. } => Some("inline"),
                AttachmentContent::Url { .. } => Some("url"),
                AttachmentContent::Unknown(_) => None,
            }
        }

        let samples = vec![
            AttachmentContent::File {
                path: "a".to_string(),
            },
            AttachmentContent::Inline {
                content: "a".to_string(),
                language: None,
            },
            AttachmentContent::Url {
                url: "https://example.com".to_string(),
            },
        ];
        assert_eq!(samples.len(), AttachmentContent::KNOWN_TAGS.len());

        for sample in &samples {
            let tag = tag_of(sample).expect("samples are known variants");
            assert!(
                AttachmentContent::KNOWN_TAGS.contains(&tag),
                "missing tag: {}",
                tag
            );

            let value = serde_json::to_value(sample).unwrap();
            assert_eq!(AttachmentContent::tag(&value), Some(tag));
            let parsed: AttachmentContent = serde_json::from_value(value).unwrap();
            assert!(!parsed.is_unknown(), "{} fell into the fallback", tag);
        }
    }

    /// The fallback must not shadow variants this build does know.
    #[test]
    fn test_known_message_meta_still_wins() {
        let meta = MessageMeta::Claim {
            patterns: vec!["src/**".to_string()],
            ttl_secs: 3600,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert_eq!(
            json,
            r#"{"type":"claim","patterns":["src/**"],"ttl_secs":3600}"#
        );

        let parsed: MessageMeta = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, MessageMeta::Claim { .. }));

        let tombstone = MessageMeta::Deleted {
            target_id: Ulid::nil(),
            deleted_by: "agent".to_string(),
            deleted_at: Utc::now(),
        };
        let json = serde_json::to_string(&tombstone).unwrap();
        let parsed: MessageMeta = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, MessageMeta::Deleted { .. }));
    }

    #[test]
    fn test_unknown_system_event_round_trips() {
        let future = r#"{"agent_evicted":{"reason":"idle"}}"#;
        let event: SystemEvent = serde_json::from_str(future).unwrap();
        assert!(matches!(event, SystemEvent::Unknown(_)));
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::from_str::<serde_json::Value>(future).unwrap()
        );

        // Known events keep their existing encoding.
        let known: SystemEvent = serde_json::from_str(r#""agent_registered""#).unwrap();
        assert!(matches!(known, SystemEvent::AgentRegistered));
    }

    #[test]
    fn test_unknown_attachment_type_round_trips() {
        let future = r#"{"name":"clip","type":"video","stream_id":"abc"}"#;
        let attachment: Attachment = serde_json::from_str(future).unwrap();
        assert_eq!(attachment.name, "clip");
        assert!(matches!(attachment.content, AttachmentContent::Unknown(_)));
        assert!(!attachment.is_available());
    }

    /// The whole point of the fallback: one future record in a channel file
    /// must not deny access to the messages around it.
    #[test]
    fn test_future_message_in_channel_still_readable() {
        use std::io::Write;
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("channel.jsonl");

        let before = Message::new("agent", "channel", "before");
        let after = Message::new("agent", "channel", "after");
        crate::storage::jsonl::append_record(&path, &before).unwrap();

        // A message a newer rite could write: unknown meta type + unknown field.
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(
                file,
                r#"{{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"future","channel":"channel","body":"from the future","meta":{{"type":"reaction","emoji":"+1"}},"priority":"high"}}"#
            )
            .unwrap();
        }

        crate::storage::jsonl::append_record(&path, &after).unwrap();

        let messages = read_messages(&path).unwrap();
        assert_eq!(messages.len(), 3, "future record must not be dropped");
        assert_eq!(messages[0].body, "before");
        assert_eq!(messages[1].body, "from the future");
        assert!(matches!(messages[1].meta, Some(MessageMeta::Unknown(_))));
        assert_eq!(messages[2].body, "after");
    }

    /// The two cases must not share a fate: a future record is kept and not
    /// counted; a corrupt record is skipped, counted, and reported.
    #[test]
    fn test_future_record_is_kept_but_corrupt_record_is_counted() {
        use std::io::Write;
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("channel.jsonl");

        crate::storage::jsonl::append_record(&path, &Message::new("agent", "channel", "good"))
            .unwrap();

        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            // Line 2: unknown meta type — a newer rite's variant. Benign.
            writeln!(
                file,
                r#"{{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"newer","channel":"channel","body":"future","meta":{{"type":"reaction","emoji":"+1"}}}}"#
            )
            .unwrap();
            // Line 3: known meta type with a damaged body. Corruption.
            writeln!(
                file,
                r#"{{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FBW","agent":"damaged","channel":"channel","body":"corrupt","meta":{{"type":"claim"}}}}"#
            )
            .unwrap();
        }

        let (messages, issues) =
            crate::storage::jsonl::read_records_reporting::<Message>(&path).unwrap();
        let skipped = &issues.skipped;

        assert_eq!(messages.len(), 2, "the future record must survive");
        assert_eq!(messages[1].body, "future");
        assert!(matches!(messages[1].meta, Some(MessageMeta::Unknown(_))));

        assert_eq!(skipped.len(), 1, "only the corrupt record is skipped");
        assert_eq!(skipped[0].line, Some(3));
        assert!(skipped[0].error.contains("corrupt"), "{}", skipped[0].error);
        assert!(skipped[0].error.contains("claim"), "{}", skipped[0].error);
    }

    #[test]
    fn test_attachment_is_available() {
        // Inline content is always available
        let inline = Attachment::inline("code", "fn main() {}", Some("rust".to_string()));
        assert!(inline.is_available());

        // URLs are always available
        let url = Attachment::url("docs", "https://example.com");
        assert!(url.is_available());

        // File attachment that doesn't exist
        let missing = Attachment::file("missing", "/nonexistent/path/to/file.txt");
        assert!(!missing.is_available());

        // File attachment that exists (use this test file)
        let existing = Attachment::file("message.rs", file!());
        assert!(existing.is_available());
    }
}
