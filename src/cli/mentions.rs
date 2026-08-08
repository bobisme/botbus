//! `rite mentions follow` — a single-process, cross-channel mention stream.
//!
//! This is the scalable replacement for "spawn one watcher per channel and
//! filter": one file watcher over `channels_dir()`, per-channel byte offsets,
//! and incremental reads of only the bytes appended since the last read.
//!
//! Mentions are parsed at write time and stored on [`Message::mentions`], so
//! this never re-parses message bodies.

use anyhow::{Context, Result};
use colored::Colorize;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::cli::OutputFormat;
use crate::core::channel::{dm_agents, is_dm_channel};
use crate::core::identity::resolve_agent;
use crate::core::message::{Message, MessageMeta, read_messages_from_offset};
use crate::core::project::channels_dir;
use crate::storage::watch::{debounce_events, filter_channel_events, watch_directory};

/// How long to batch filesystem events before draining the changed channels.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Why a message was forwarded onto the stream.
///
/// Consumers use this to decide how to present or reply to a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    /// A direct message in a DM channel this agent participates in.
    Dm,
    /// A message in a regular channel whose `mentions` include this agent.
    Mention,
}

impl Route {
    pub fn as_str(self) -> &'static str {
        match self {
            Route::Dm => "dm",
            Route::Mention => "mention",
        }
    }
}

/// One emitted stream record. Serialized as a single JSON object per line.
#[derive(Debug, Clone, Serialize)]
pub struct MentionRecord {
    /// Why this message was forwarded.
    pub route: Route,
    /// Channel the message was read from (the file name, not `message.channel`).
    pub channel: String,
    /// Where a reply should be addressed: the channel name, or `@agent` for a DM.
    pub reply_target: String,
    /// The full message record as stored.
    pub message: Message,
}

pub struct FollowOptions {
    /// Stream DMs this agent participates in. On by default at the CLI; the
    /// `--no-dms` flag turns it off. A DM is the most direct form of address
    /// there is, so the default must not silently withhold one.
    pub include_dms: bool,
    /// Only stream messages carrying any of these labels (empty = no filter).
    pub labels: Vec<String>,
    /// Stop after this many seconds (None = run until killed).
    pub timeout: Option<u64>,
    /// Stop after this many records (None = uncapped).
    pub count: Option<usize>,
    /// Output format.
    pub format: OutputFormat,
}

/// Decides whether a message belongs on this agent's stream, and why.
///
/// Kept free of I/O so the routing rules — especially the DM privacy rule —
/// are directly unit-testable.
pub struct MentionFilter {
    /// Lowercased agent name; mention comparison is case-insensitive because
    /// `extract_mentions` preserves whatever case was typed.
    agent_lower: String,
    include_dms: bool,
    labels: Vec<String>,
}

impl MentionFilter {
    pub fn new(agent: &str, include_dms: bool, labels: Vec<String>) -> Self {
        Self {
            agent_lower: agent.to_lowercase(),
            include_dms,
            labels,
        }
    }

    /// Classify a message read from `channel`, returning its route if it should
    /// be forwarded.
    ///
    /// `channel` is the channel file's name, which is authoritative for DM
    /// privacy — `message.channel` is attacker-controlled content of the record.
    pub fn classify(&self, msg: &Message, channel: &str) -> Option<Route> {
        // Never echo the agent's own messages back at it.
        if msg.agent.to_lowercase() == self.agent_lower {
            return None;
        }

        // System records (hook firings, registrations) are bookkeeping, not
        // conversation. `rite inbox --mentions` skips them too, and forwarding
        // them would let a hook's own announcement re-trigger the hook.
        if matches!(msg.meta, Some(MessageMeta::System { .. })) {
            return None;
        }

        if !self.labels.is_empty() && !msg.has_any_label(&self.labels) {
            return None;
        }

        if is_dm_channel(channel) {
            // DM privacy is absolute. Participation is the only thing that can
            // route a message out of a DM channel — a mention never overrides
            // it, and a DM channel whose participants cannot be parsed is
            // treated as private (fail closed).
            if !self.is_dm_participant(channel) {
                return None;
            }
            // A participant's DM routes as `dm` whether or not it also mentions
            // the agent — the channel is already as direct as address gets.
            return self.include_dms.then_some(Route::Dm);
        }

        if msg
            .mentions
            .iter()
            .any(|m| m.to_lowercase() == self.agent_lower)
        {
            return Some(Route::Mention);
        }

        None
    }

    fn is_dm_participant(&self, channel: &str) -> bool {
        match dm_agents(channel) {
            Some((a, b)) => {
                a.to_lowercase() == self.agent_lower || b.to_lowercase() == self.agent_lower
            }
            None => false,
        }
    }

    /// Where a reply to this record should be sent.
    fn reply_target(&self, channel: &str, msg: &Message) -> String {
        if !is_dm_channel(channel) {
            return channel.to_string();
        }
        match dm_agents(channel) {
            Some((a, b)) => {
                let other = if a.to_lowercase() == self.agent_lower {
                    b
                } else {
                    a
                };
                format!("@{}", other)
            }
            None => format!("@{}", msg.agent),
        }
    }

    pub fn record(&self, msg: Message, channel: &str) -> Option<MentionRecord> {
        let route = self.classify(&msg, channel)?;
        let reply_target = self.reply_target(channel, &msg);
        Some(MentionRecord {
            route,
            channel: channel.to_string(),
            reply_target,
            message: msg,
        })
    }
}

/// Per-channel byte offsets into the channel JSONL files.
///
/// Existing channels are seeded at their current end of file ("now"), so
/// startup does not replay history. A channel first seen after startup has no
/// entry and therefore starts at offset 0 — deliberate asymmetry, so a channel
/// whose very first message is the mention is not missed.
#[derive(Debug, Default)]
struct Cursors {
    offsets: HashMap<String, u64>,
}

impl Cursors {
    /// Seed every channel file currently on disk at its end of file.
    fn seeded_at_now(channels_path: &Path) -> Self {
        let mut offsets = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(channels_path) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "jsonl")
                    && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    offsets.insert(name.to_string(), size);
                }
            }
        }
        Self { offsets }
    }

    /// Offset to read a changed channel from.
    ///
    /// Unknown channel → 0 (created after startup, read from the beginning).
    /// A file shorter than the recorded offset was truncated or replaced (git
    /// sync, `channels delete` + recreate), so restart it from 0 rather than
    /// going permanently blind.
    fn read_from(&self, channel: &str, file_len: u64) -> u64 {
        match self.offsets.get(channel) {
            Some(&offset) if offset <= file_len => offset,
            Some(_) => 0,
            None => 0,
        }
    }

    fn advance(&mut self, channel: &str, offset: u64) {
        self.offsets.insert(channel.to_string(), offset);
    }

    /// Forget a channel whose file is gone, so the map stays proportional to
    /// live channels rather than to every channel ever seen.
    fn forget(&mut self, channel: &str) {
        self.offsets.remove(channel);
    }
}

/// Stream every message mentioning `agent` across all channels (plus its DMs).
pub fn follow(options: FollowOptions, explicit_agent: Option<&str>) -> Result<()> {
    let agent = resolve_agent(explicit_agent).ok_or_else(|| {
        anyhow::anyhow!(
            "mentions follow requires agent identity. Set RITE_AGENT or use --agent <name>."
        )
    })?;

    let filter = MentionFilter::new(&agent, options.include_dms, options.labels.clone());

    let channels_path = channels_dir();
    if !channels_path.exists() {
        std::fs::create_dir_all(&channels_path).with_context(|| {
            format!(
                "Failed to create channels directory: {}",
                channels_path.display()
            )
        })?;
    }

    // Register the watcher BEFORE seeding offsets. The reverse order has a hole:
    // a message appended between seeding and watcher registration produces no
    // event and would sit unread until the next unrelated write to that channel.
    let (_watcher, rx) =
        watch_directory(&channels_path).with_context(|| "Failed to watch channels directory")?;

    let mut cursors = Cursors::seeded_at_now(&channels_path);

    if options.format == OutputFormat::Pretty {
        eprintln!(
            "{}",
            format!(
                "Following mentions of @{}{} (Ctrl+C to exit)",
                agent,
                if options.include_dms {
                    " + DMs"
                } else {
                    " (--no-dms: DMs suppressed)"
                }
            )
            .cyan()
            .bold()
        );
    }

    let start = Instant::now();
    let mut emitted: usize = 0;

    loop {
        if let Some(timeout) = options.timeout
            && start.elapsed() >= Duration::from_secs(timeout)
        {
            return Ok(());
        }

        // Blocks up to POLL_INTERVAL, so timeout/count are checked regularly
        // even when nothing is happening.
        let changed = filter_channel_events(debounce_events(&rx, POLL_INTERVAL));

        let mut batch: Vec<MentionRecord> = Vec::new();

        for channel in changed {
            let path = channels_path.join(format!("{}.jsonl", channel));
            let Ok(meta) = std::fs::metadata(&path) else {
                // Deleted or renamed out from under us.
                cursors.forget(&channel);
                continue;
            };

            let offset = cursors.read_from(&channel, meta.len());
            let (messages, new_offset) = match read_messages_from_offset(&path, offset) {
                Ok(result) => result,
                Err(e) => {
                    // A torn append (writer mid-line) or a corrupt record. Do
                    // not advance the cursor; the next event re-reads it.
                    eprintln!("warn: failed to read #{}: {}", channel, e);
                    continue;
                }
            };
            cursors.advance(&channel, new_offset);

            batch.extend(
                messages
                    .into_iter()
                    .filter_map(|msg| filter.record(msg, &channel)),
            );
        }

        // Channels are drained in filesystem-event order, which is arbitrary.
        // Emit each batch in message order so a burst across several channels
        // reaches the consumer chronologically.
        batch.sort_by_key(|r| (r.message.ts, r.message.id));

        for record in &batch {
            emit(record, options.format)?;
            emitted += 1;
            if let Some(max) = options.count
                && emitted >= max
            {
                return Ok(());
            }
        }
    }
}

fn emit(record: &MentionRecord, format: OutputFormat) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    match format {
        // JSONL: exactly one JSON object per line, flushed immediately.
        OutputFormat::Json => writeln!(stdout, "{}", serde_json::to_string(record)?)?,
        OutputFormat::Text => writeln!(
            stdout,
            "{}  {}  {}  {}  {}",
            record.message.id,
            record.route.as_str(),
            record.channel,
            record.message.agent,
            single_line(&record.message.body)
        )?,
        OutputFormat::Pretty => {
            let ts = record
                .message
                .ts
                .with_timezone(&chrono::Local)
                .format("%H:%M");
            writeln!(
                stdout,
                "[{}] {} {}: {}",
                ts.to_string().dimmed(),
                format!("[{}] #{}", record.route.as_str(), record.channel).dimmed(),
                record.message.agent.cyan().bold(),
                record.message.body
            )?
        }
    }
    stdout.flush()?;
    Ok(())
}

/// Collapse newlines so a text record stays on one line.
fn single_line(body: &str) -> String {
    body.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::message::SystemEvent;
    use tempfile::TempDir;

    fn msg(agent: &str, channel: &str, body: &str) -> Message {
        Message::new(agent, channel, body)
    }

    #[test]
    fn mention_in_regular_channel_routes_as_mention() {
        let filter = MentionFilter::new("rite-dev", false, vec![]);
        let m = msg("other", "rite", "can @rite-dev look at this?");
        assert_eq!(filter.classify(&m, "rite"), Some(Route::Mention));
    }

    #[test]
    fn non_mention_is_not_forwarded() {
        let filter = MentionFilter::new("rite-dev", false, vec![]);
        let m = msg("other", "rite", "unrelated chatter");
        assert_eq!(filter.classify(&m, "rite"), None);

        // A mention of a different agent must not match on a prefix.
        let m = msg("other", "rite", "ping @rite-dev-two");
        assert_eq!(filter.classify(&m, "rite"), None);
    }

    #[test]
    fn mention_matching_is_case_insensitive() {
        let filter = MentionFilter::new("rite-dev", false, vec![]);
        let m = msg("other", "rite", "hey @Rite-Dev and @RITE-DEV");
        assert_eq!(filter.classify(&m, "rite"), Some(Route::Mention));

        // ...and so is the agent side of the comparison.
        let filter = MentionFilter::new("Rite-Dev", false, vec![]);
        let m = msg("other", "rite", "hey @rite-dev");
        assert_eq!(filter.classify(&m, "rite"), Some(Route::Mention));
    }

    #[test]
    fn self_authored_messages_are_dropped() {
        let filter = MentionFilter::new("rite-dev", true, vec![]);

        // Even when the agent mentions itself.
        let m = msg("rite-dev", "rite", "note to self @rite-dev");
        assert_eq!(filter.classify(&m, "rite"), None);

        // Case-insensitively, and in its own DMs.
        let m = msg("Rite-Dev", "_dm_alice_rite-dev", "hi");
        assert_eq!(filter.classify(&m, "_dm_alice_rite-dev"), None);
    }

    /// A participant's DM is delivered by default (the CLI passes
    /// `include_dms: true` unless `--no-dms` is given), and suppressed only
    /// when the caller explicitly opts out.
    #[test]
    fn dm_to_participant_routes_as_dm_unless_suppressed() {
        let m = msg("alice", "_dm_alice_rite-dev", "got a minute?");

        let filter = MentionFilter::new("rite-dev", true, vec![]);
        assert_eq!(filter.classify(&m, "_dm_alice_rite-dev"), Some(Route::Dm));

        // --no-dms
        let filter = MentionFilter::new("rite-dev", false, vec![]);
        assert_eq!(filter.classify(&m, "_dm_alice_rite-dev"), None);
    }

    /// A DM that also mentions the agent still routes as `dm`, not `mention` —
    /// the DM channel is the reason it was forwarded.
    #[test]
    fn a_mention_inside_ones_own_dm_routes_as_dm() {
        let m = msg("alice", "_dm_alice_rite-dev", "ping @rite-dev");

        let filter = MentionFilter::new("rite-dev", true, vec![]);
        assert_eq!(filter.classify(&m, "_dm_alice_rite-dev"), Some(Route::Dm));

        // ...and --no-dms suppresses it: the mention does not smuggle it back in.
        let filter = MentionFilter::new("rite-dev", false, vec![]);
        assert_eq!(filter.classify(&m, "_dm_alice_rite-dev"), None);
    }

    /// DM privacy is absolute: a mention never overrides DM participation.
    #[test]
    fn mention_never_leaks_a_dm_the_agent_is_not_party_to() {
        // alice and bob are talking; carol is mentioned but is not a party.
        let m = msg("alice", "_dm_alice_bob", "we should ask @carol about this");
        assert!(m.mentions.iter().any(|x| x == "carol"));

        // With DMs requested...
        let filter = MentionFilter::new("carol", true, vec![]);
        assert_eq!(filter.classify(&m, "_dm_alice_bob"), None);

        // ...and without.
        let filter = MentionFilter::new("carol", false, vec![]);
        assert_eq!(filter.classify(&m, "_dm_alice_bob"), None);

        // Case differences must not create a back door.
        let m = msg("alice", "_dm_alice_bob", "ping @Carol");
        let filter = MentionFilter::new("carol", true, vec![]);
        assert_eq!(filter.classify(&m, "_dm_alice_bob"), None);

        // A participant of that same DM still receives it.
        let filter = MentionFilter::new("bob", true, vec![]);
        assert_eq!(filter.classify(&m, "_dm_alice_bob"), Some(Route::Dm));
    }

    #[test]
    fn unparseable_dm_channel_fails_closed() {
        let m = msg("alice", "_dm_broken", "hi @carol");
        let filter = MentionFilter::new("carol", true, vec![]);
        assert_eq!(filter.classify(&m, "_dm_broken"), None);
    }

    #[test]
    fn dm_privacy_uses_the_file_name_not_the_record_channel() {
        // A record claiming to live in a public channel, stored in someone
        // else's DM file, must not leak.
        let mut m = msg("alice", "_dm_alice_bob", "hey @carol");
        m.channel = "general".to_string();
        let filter = MentionFilter::new("carol", true, vec![]);
        assert_eq!(filter.classify(&m, "_dm_alice_bob"), None);
    }

    #[test]
    fn labels_filter_the_stream() {
        let filter = MentionFilter::new("rite-dev", true, vec!["review".to_string()]);

        let m = msg("other", "rite", "@rite-dev please look").with_labels(vec!["chat".to_string()]);
        assert_eq!(filter.classify(&m, "rite"), None);

        let m =
            msg("other", "rite", "@rite-dev please look").with_labels(vec!["review".to_string()]);
        assert_eq!(filter.classify(&m, "rite"), Some(Route::Mention));

        let m = msg("alice", "_dm_alice_rite-dev", "ping");
        assert_eq!(filter.classify(&m, "_dm_alice_rite-dev"), None);
    }

    #[test]
    fn system_records_are_not_forwarded() {
        let filter = MentionFilter::new("rite-dev", true, vec![]);
        let m = msg("system", "rite", "Hook hk-1 fired: notify @rite-dev").with_meta(
            MessageMeta::System {
                event: SystemEvent::AgentRegistered,
            },
        );
        assert_eq!(filter.classify(&m, "rite"), None);
    }

    #[test]
    fn reply_target_is_the_channel_or_the_other_dm_party() {
        let filter = MentionFilter::new("rite-dev", true, vec![]);

        let m = msg("alice", "rite", "@rite-dev hi");
        let record = filter.record(m, "rite").unwrap();
        assert_eq!(record.reply_target, "rite");
        assert_eq!(record.route, Route::Mention);

        let m = msg("alice", "_dm_alice_rite-dev", "hi");
        let record = filter.record(m, "_dm_alice_rite-dev").unwrap();
        assert_eq!(record.reply_target, "@alice");
        assert_eq!(record.route, Route::Dm);
    }

    #[test]
    fn record_serializes_as_a_single_json_line() {
        let filter = MentionFilter::new("rite-dev", false, vec![]);
        let m = msg("alice", "rite", "@rite-dev multi\nline body");
        let record = filter.record(m, "rite").unwrap();
        let line = serde_json::to_string(&record).unwrap();
        assert!(
            !line.contains('\n'),
            "JSONL record must not contain a newline"
        );
        assert!(line.contains("\"route\":\"mention\""));
    }

    #[test]
    fn existing_channels_are_seeded_at_end_of_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("general.jsonl");
        crate::storage::jsonl::append_record(&path, &msg("alice", "general", "old news")).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();

        let cursors = Cursors::seeded_at_now(temp.path());
        assert_eq!(cursors.read_from("general", size), size);

        // Nothing is replayed from a channel seeded at "now".
        let (replayed, _) = read_messages_from_offset(&path, size).unwrap();
        assert!(replayed.is_empty());
    }

    #[test]
    fn channels_created_after_startup_are_read_from_offset_zero() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("general.jsonl"), "").unwrap();

        let cursors = Cursors::seeded_at_now(temp.path());

        // A channel that did not exist at seed time starts at 0, so its very
        // first message is delivered.
        let path = temp.path().join("brand-new.jsonl");
        crate::storage::jsonl::append_record(&path, &msg("alice", "brand-new", "@rite-dev first"))
            .unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(cursors.read_from("brand-new", len), 0);

        let (messages, _) = read_messages_from_offset(&path, 0).unwrap();
        let filter = MentionFilter::new("rite-dev", false, vec![]);
        assert_eq!(
            filter.classify(&messages[0], "brand-new"),
            Some(Route::Mention)
        );
    }

    #[test]
    fn truncated_file_restarts_from_zero() {
        let mut cursors = Cursors::default();
        cursors.advance("general", 500);
        assert_eq!(cursors.read_from("general", 900), 500);
        // File shrank: it was rewritten or replaced.
        assert_eq!(cursors.read_from("general", 100), 0);
    }

    #[test]
    fn deleted_channels_are_forgotten() {
        let mut cursors = Cursors::default();
        cursors.advance("gone", 42);
        cursors.forget("gone");
        assert!(cursors.offsets.is_empty());
    }
}
