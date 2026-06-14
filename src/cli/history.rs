//! View message history.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local, Utc};
use colored::Colorize;
use serde::Serialize;
use std::path::Path;

use super::OutputFormat;
use crate::core::channel::resolve_channel;
use crate::core::identity::resolve_agent;
use crate::core::message::{
    Message, read_last_n_messages, read_messages, read_messages_from_offset_limited,
};
use crate::core::project::channel_path;

#[derive(Clone)]
pub struct HistoryOptions {
    pub channel: Option<String>,
    pub count: usize,
    pub follow: bool,
    /// Exit follow mode after N seconds
    pub timeout: Option<u64>,
    /// Exit follow mode after receiving N new messages
    pub follow_count: Option<usize>,
    pub since: Option<String>,
    pub before: Option<String>,
    pub from: Option<String>,
    /// Filter by labels (messages must have ANY of these labels)
    pub labels: Vec<String>,
    /// Read messages after this byte offset (for incremental reading)
    pub after_offset: Option<u64>,
    /// Read messages after this message ID (ULID)
    pub after_id: Option<String>,
    /// Show the offset info for next read
    pub show_offset: bool,
    /// Output format
    pub format: OutputFormat,
    /// Agent identity (for resolving @mentions in channel names)
    pub agent: Option<String>,
}

/// Output from history command, useful for programmatic access.
#[derive(Debug, Serialize)]
pub struct HistoryOutput {
    pub messages: Vec<Message>,
    /// Byte offset for next read (end of file after this read)
    pub next_offset: u64,
    /// ID of the last message returned (if any)
    pub last_id: Option<String>,
    /// Total messages available before count limit was applied (for pagination awareness)
    pub total_available: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<String>,
}

/// View message history.
pub fn run(options: HistoryOptions) -> Result<()> {
    // Resolve channel name (handles @agent → DM channel)
    let agent = options.agent.clone().or_else(|| resolve_agent(None));
    let raw_channel = options
        .channel
        .clone()
        .unwrap_or_else(|| "general".to_string());
    let channel = resolve_channel(&raw_channel, agent.as_deref()).ok_or_else(|| {
        anyhow!(
            "Cannot resolve DM channel '{}' without agent identity.\n\
             Set RITE_AGENT or use --agent flag.",
            raw_channel
        )
    })?;

    let resolved_options = HistoryOptions {
        channel: Some(channel.clone()),
        ..options.clone()
    };
    let output = run_with_output(resolved_options)?;

    match options.format {
        OutputFormat::Json => {
            if options.follow {
                // Emit initial messages as JSONL (one compact JSON object per line)
                {
                    use std::io::Write;
                    for msg in &output.messages {
                        println!("{}", serde_json::to_string(msg)?);
                    }
                    std::io::stdout().flush()?;
                }
                // Continue streaming new messages as JSONL. Seed the follow
                // cursor from the bounded read's next_offset so messages between
                // that read and EOF are not skipped.
                let path = channel_path(&channel);
                follow_channel_json(
                    &path,
                    output.next_offset,
                    options.timeout,
                    options.follow_count,
                )?;
            } else {
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
            return Ok(());
        }
        OutputFormat::Pretty => {
            if output.messages.is_empty() {
                if options.after_offset.is_some() || options.after_id.is_some() {
                    println!("No new messages.");
                } else {
                    println!("No messages match your criteria.");
                }

                // Still show offset info if requested
                if options.show_offset {
                    println!("{}: {}", "next_offset".dimmed(), output.next_offset);
                }
                return Ok(());
            }

            // Print header
            println!("{}", format!("#{}", channel).cyan().bold());

            // Print messages
            for msg in &output.messages {
                print_message(msg);
            }

            // Show offset info for next read
            if options.show_offset {
                println!();
                println!("{}: {}", "next_offset".dimmed(), output.next_offset);
                if let Some(last_id) = &output.last_id {
                    println!("{}: {}", "last_id".dimmed(), last_id);
                }
            }

            // Follow mode
            if options.follow {
                let path = channel_path(&channel);
                follow_channel(
                    &path,
                    output.next_offset,
                    options.timeout,
                    options.follow_count,
                )?;
            }
        }
        OutputFormat::Text => {
            // Text format: concise one-liner per message
            for msg in &output.messages {
                let time_ago = format_time_ago(msg.ts);
                let labels = format_label_badges(&msg.labels);
                println!(
                    "{}  {}  {}  {}{}",
                    msg.id, msg.agent, time_ago, labels, msg.body
                );
            }

            // Follow mode
            if options.follow {
                let path = channel_path(&channel);
                follow_channel(
                    &path,
                    output.next_offset,
                    options.timeout,
                    options.follow_count,
                )?;
            }
        }
    }

    Ok(())
}

fn format_label_badges(labels: &[String]) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!(
            "{} ",
            labels
                .iter()
                .map(|label| format!("[{}]", label))
                .collect::<Vec<_>>()
                .join("")
        )
    }
}

/// Run history and return structured output (for programmatic use).
/// Note: channel should already be resolved (no @agent syntax).
pub fn run_with_output(options: HistoryOptions) -> Result<HistoryOutput> {
    // Channel should be pre-resolved by run(), but handle defaults
    let channel = options
        .channel
        .clone()
        .unwrap_or_else(|| "general".to_string());
    let path = channel_path(&channel);

    if !path.exists() {
        return Ok(HistoryOutput {
            messages: Vec::new(),
            next_offset: 0,
            last_id: None,
            total_available: 0,
            advice: vec![],
        });
    }

    // Get file size for next_offset calculation
    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    // Resolve --after-id to a byte offset so it shares the lossless offset-based
    // read path. Routing it through read_messages_from_offset_limited keeps
    // next_offset pointing at the true continuation cursor (immediately after the
    // last returned message). The old after-id path returned EOF as next_offset,
    // so paginating with --after-offset would skip every message between the
    // count-th returned message and EOF.
    let start_offset = match (options.after_offset, &options.after_id) {
        (Some(offset), _) => Some(offset),
        (None, Some(after_id)) => {
            // Unknown id falls back to offset 0 (read from the beginning), which
            // matches the previous "id not found → return from start" behavior.
            Some(crate::core::message::offset_after_message_id(&path, after_id)?.unwrap_or(0))
        }
        (None, None) => None,
    };

    // Read messages based on options
    let (messages, next_offset) = if let Some(offset) = start_offset {
        // Bounded read from a byte offset; next_offset is a valid continuation
        // cursor whether or not the result hit the count limit.
        read_messages_from_offset_limited(&path, offset, options.count)
            .with_context(|| format!("Failed to read channel #{} from offset", channel))?
    } else if options.since.is_some()
        || options.before.is_some()
        || options.from.is_some()
        || !options.labels.is_empty()
    {
        // Need to filter, read all and filter
        let all: Vec<Message> =
            read_messages(&path).with_context(|| format!("Failed to read channel #{}", channel))?;
        (filter_messages(all, &options), file_size)
    } else {
        // Just get last N
        let msgs = read_last_n_messages(&path, options.count)
            .with_context(|| format!("Failed to read channel #{}", channel))?;
        (msgs, file_size)
    };

    let total_available = messages.len();
    let last_id = messages.last().map(|m| m.id.to_string());

    // Build advice. For offset-based reads, more messages remain when the
    // continuation cursor hasn't reached EOF yet.
    let mut advice = Vec::new();
    if start_offset.is_some() && next_offset < file_size {
        advice.push(format!(
            "rite history {} --after-offset {}",
            options.channel.as_ref().unwrap_or(&"general".to_string()),
            next_offset
        ));
    }

    Ok(HistoryOutput {
        messages,
        next_offset,
        last_id,
        total_available,
        advice,
    })
}

fn filter_messages(messages: Vec<Message>, options: &HistoryOptions) -> Vec<Message> {
    let mut filtered: Vec<Message> = messages
        .into_iter()
        .filter(|msg| {
            // Filter by sender
            if let Some(from) = &options.from
                && &msg.agent != from
            {
                return false;
            }

            // Filter by since
            if let Some(since_str) = &options.since
                && let Ok(since) = parse_datetime(since_str)
                && msg.ts < since
            {
                return false;
            }

            // Filter by before
            if let Some(before_str) = &options.before
                && let Ok(before) = parse_datetime(before_str)
                && msg.ts > before
            {
                return false;
            }

            // Filter by labels (message must have ANY of the specified labels)
            if !options.labels.is_empty() && !msg.has_any_label(&options.labels) {
                return false;
            }

            true
        })
        .collect();

    // Limit to count (take last N after filtering)
    let start = filtered.len().saturating_sub(options.count);
    filtered.drain(..start);
    filtered
}

fn parse_datetime(s: &str) -> Result<DateTime<Utc>> {
    // Try parsing as RFC3339
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Try parsing as just a date
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0).unwrap();
        return Ok(DateTime::from_naive_utc_and_offset(dt, Utc));
    }

    // Try parsing as date + time
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(DateTime::from_naive_utc_and_offset(dt, Utc));
    }

    anyhow::bail!("Could not parse datetime: {}", s)
}

fn print_message(msg: &Message) {
    let local_time: DateTime<Local> = msg.ts.with_timezone(&Local);
    let now = Local::now();

    // Format timestamp with relative dates for recent messages
    let time_str = if local_time.date_naive() == now.date_naive() {
        // Today: just show time
        format!("Today {}", local_time.format("%H:%M"))
    } else if local_time.date_naive() == now.date_naive() - chrono::Days::new(1) {
        // Yesterday
        format!("Yesterday {}", local_time.format("%H:%M"))
    } else {
        // Older: show full date and time
        local_time.format("%Y-%m-%d %H:%M").to_string()
    };

    // Color the agent name consistently
    let agent_colored = colorize_agent(&msg.agent);

    // Format labels
    let labels_str = if msg.labels.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            msg.labels
                .iter()
                .map(|l| format!("[{}]", l).yellow().to_string())
                .collect::<Vec<_>>()
                .join("")
        )
    };

    // Format attachment indicator
    let attach_str = if msg.attachments.is_empty() {
        String::new()
    } else {
        format!(" {}", format!("[{}]", msg.attachments.len()).magenta())
    };

    println!(
        "[{}] {}:{}{} {}",
        time_str.dimmed(),
        agent_colored,
        labels_str,
        attach_str,
        msg.body
    );

    // Show attachment details if present
    for attachment in &msg.attachments {
        if !attachment.is_available() {
            println!(
                "    {} {}",
                "⚠".dimmed(),
                format!("Attachment: {} — not available locally", attachment.name).dimmed()
            );
        }
    }
}

fn colorize_agent(name: &str) -> colored::ColoredString {
    // Simple hash to pick a color
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

fn format_time_ago(ts: DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(ts);

    if duration.num_seconds() < 60 {
        "just now".to_string()
    } else if duration.num_minutes() < 60 {
        format!("{}m ago", duration.num_minutes())
    } else if duration.num_hours() < 24 {
        format!("{}h ago", duration.num_hours())
    } else {
        format!("{}d ago", duration.num_days())
    }
}

fn follow_channel_json(
    path: &Path,
    start_offset: u64,
    timeout_secs: Option<u64>,
    follow_count: Option<usize>,
) -> Result<()> {
    use crate::core::message::read_messages_from_offset;
    use crate::core::project::channels_dir;
    use crate::storage::watch::{debounce_events, filter_channel_events, watch_directory};
    use std::io::Write;
    use std::time::{Duration, Instant};

    let channels = channels_dir();
    // Register the watcher before draining so any message written after the
    // drain is guaranteed to produce an event the loop will pick up.
    let (_watcher, rx) = watch_directory(&channels)?;

    // Seed the cursor from the caller's offset (the initial bounded read's
    // next_offset), not the file's current EOF. Seeding from EOF would skip
    // messages that landed between the bounded read and EOF.
    let mut offset = start_offset;
    let start = Instant::now();
    let mut messages_received: usize = 0;

    // Drain any startup backlog already present between the seed offset and EOF
    // so it is delivered immediately rather than waiting for the next event.
    {
        let (backlog, new_offset) = read_messages_from_offset(path, offset)?;
        for msg in &backlog {
            println!("{}", serde_json::to_string(msg)?);
            std::io::stdout().flush()?;
            messages_received += 1;
            if let Some(max_count) = follow_count
                && messages_received >= max_count
            {
                return Ok(());
            }
        }
        offset = new_offset;
    }

    loop {
        if let Some(timeout) = timeout_secs
            && start.elapsed() >= Duration::from_secs(timeout)
        {
            break;
        }

        if let Some(max_count) = follow_count
            && messages_received >= max_count
        {
            break;
        }

        let changed = debounce_events(&rx, Duration::from_millis(100));
        let channel_changes = filter_channel_events(changed);

        let channel_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if channel_changes.contains(&channel_name.to_string()) {
            let (new_messages, new_offset) = read_messages_from_offset(path, offset)?;
            for msg in &new_messages {
                println!("{}", serde_json::to_string(msg)?);
                std::io::stdout().flush()?;
                messages_received += 1;

                if let Some(max_count) = follow_count
                    && messages_received >= max_count
                {
                    return Ok(());
                }
            }
            offset = new_offset;
        }
    }

    Ok(())
}

fn follow_channel(
    path: &Path,
    start_offset: u64,
    timeout_secs: Option<u64>,
    follow_count: Option<usize>,
) -> Result<()> {
    use crate::core::message::read_messages_from_offset;
    use crate::core::project::channels_dir;
    use crate::storage::watch::{debounce_events, filter_channel_events, watch_directory};
    use std::time::{Duration, Instant};

    println!("{}", "--- Following (Ctrl+C to exit) ---".dimmed());

    let channels = channels_dir();
    // Register the watcher before draining so any message written after the
    // drain is guaranteed to produce an event the loop will pick up.
    let (_watcher, rx) = watch_directory(&channels)?;

    // Seed the cursor from the caller's offset (the initial read's next_offset),
    // not the file's current EOF, so messages between the initial read and EOF
    // are not skipped.
    let mut offset = start_offset;

    // Track timeout and message count
    let start = Instant::now();
    let mut messages_received: usize = 0;

    // Drain any startup backlog already present between the seed offset and EOF.
    {
        let (backlog, new_offset) = read_messages_from_offset(path, offset)?;
        for msg in &backlog {
            print_message(msg);
            messages_received += 1;
            if let Some(max_count) = follow_count
                && messages_received >= max_count
            {
                println!(
                    "{}",
                    format!("--- Received {} messages ---", max_count).dimmed()
                );
                return Ok(());
            }
        }
        offset = new_offset;
    }

    loop {
        // Check timeout
        if let Some(timeout) = timeout_secs
            && start.elapsed() >= Duration::from_secs(timeout)
        {
            println!("{}", format!("--- Timeout after {}s ---", timeout).dimmed());
            break;
        }

        // Check message count limit
        if let Some(max_count) = follow_count
            && messages_received >= max_count
        {
            println!(
                "{}",
                format!("--- Received {} messages ---", max_count).dimmed()
            );
            break;
        }

        let changed = debounce_events(&rx, Duration::from_millis(100));
        let channel_changes = filter_channel_events(changed);

        // Check if our channel was updated
        let channel_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

        if channel_changes.contains(&channel_name.to_string()) {
            let (new_messages, new_offset): (Vec<Message>, u64) =
                read_messages_from_offset(path, offset)?;

            for msg in &new_messages {
                print_message(msg);
                messages_received += 1;

                // Check if we've hit the message limit after each message
                if let Some(max_count) = follow_count
                    && messages_received >= max_count
                {
                    println!(
                        "{}",
                        format!("--- Received {} messages ---", max_count).dimmed()
                    );
                    return Ok(());
                }
            }

            offset = new_offset;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::send;
    use crate::core::project::{DATA_DIR_ENV_VAR, ensure_data_dir};
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TestEnv {
        _dir: TempDir,
    }

    impl TestEnv {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            unsafe {
                env::set_var(DATA_DIR_ENV_VAR, dir.path());
            }
            ensure_data_dir().unwrap();
            Self { _dir: dir }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            unsafe {
                env::remove_var(DATA_DIR_ENV_VAR);
            }
        }
    }

    #[test]
    #[serial]
    fn test_history_basic() {
        let _env = TestEnv::new();
        send::run_simple(
            "test-history".to_string(),
            "Message 1".to_string(),
            Some("test-historian"),
        )
        .unwrap();
        send::run_simple(
            "test-history".to_string(),
            "Message 2".to_string(),
            Some("test-historian"),
        )
        .unwrap();

        let options = HistoryOptions {
            channel: Some("test-history".to_string()),
            count: 50,
            follow: false,
            timeout: None,
            follow_count: None,
            since: None,
            before: None,
            from: None,
            labels: vec![],
            after_offset: None,
            after_id: None,
            show_offset: false,
            format: OutputFormat::Text,
            agent: None,
        };

        run(options).unwrap();
    }

    #[test]
    #[serial]
    fn test_history_empty_channel() {
        let _env = TestEnv::new();

        let options = HistoryOptions {
            channel: Some("nonexistent".to_string()),
            count: 50,
            follow: false,
            timeout: None,
            follow_count: None,
            since: None,
            before: None,
            from: None,
            labels: vec![],
            after_offset: None,
            after_id: None,
            show_offset: false,
            format: OutputFormat::Text,
            agent: None,
        };

        run(options).unwrap();
    }

    #[test]
    #[serial]
    fn test_history_after_offset_next_offset_does_not_skip_limited_messages() {
        let _env = TestEnv::new();
        send::run_simple(
            "test-history-page".to_string(),
            "Message 1".to_string(),
            Some("test-historian"),
        )
        .unwrap();
        send::run_simple(
            "test-history-page".to_string(),
            "Message 2".to_string(),
            Some("test-historian"),
        )
        .unwrap();
        send::run_simple(
            "test-history-page".to_string(),
            "Message 3".to_string(),
            Some("test-historian"),
        )
        .unwrap();

        let options = HistoryOptions {
            channel: Some("test-history-page".to_string()),
            count: 1,
            follow: false,
            timeout: None,
            follow_count: None,
            since: None,
            before: None,
            from: None,
            labels: vec![],
            after_offset: Some(0),
            after_id: None,
            show_offset: false,
            format: OutputFormat::Text,
            agent: None,
        };

        let first_page = run_with_output(options.clone()).unwrap();
        assert_eq!(first_page.messages.len(), 1);
        assert_eq!(first_page.messages[0].body, "Message 1");
        assert!(first_page.next_offset > 0);
        assert!(
            first_page.next_offset
                < std::fs::metadata(channel_path("test-history-page"))
                    .unwrap()
                    .len()
        );

        let second_page = run_with_output(HistoryOptions {
            after_offset: Some(first_page.next_offset),
            ..options
        })
        .unwrap();
        assert_eq!(second_page.messages.len(), 1);
        assert_eq!(second_page.messages[0].body, "Message 2");
    }

    #[test]
    #[serial]
    fn test_after_id_pagination_does_not_skip_after_count_limit() {
        let _env = TestEnv::new();
        for i in 1..=4 {
            send::run_simple(
                "test-after-id".to_string(),
                format!("Message {i}"),
                Some("test-historian"),
            )
            .unwrap();
        }

        let base = HistoryOptions {
            channel: Some("test-after-id".to_string()),
            count: 50,
            follow: false,
            timeout: None,
            follow_count: None,
            since: None,
            before: None,
            from: None,
            labels: vec![],
            after_offset: None,
            after_id: None,
            show_offset: false,
            format: OutputFormat::Text,
            agent: None,
        };

        // Grab the id of the first message.
        let all = run_with_output(base.clone()).unwrap();
        assert_eq!(all.messages.len(), 4);
        let first_id = all.messages[0].id.to_string();

        // Page after the first message, limited to 1 → "Message 2", and the
        // continuation cursor must point just past it, not at EOF.
        let page1 = run_with_output(HistoryOptions {
            after_id: Some(first_id),
            count: 1,
            ..base.clone()
        })
        .unwrap();
        assert_eq!(page1.messages.len(), 1);
        assert_eq!(page1.messages[0].body, "Message 2");
        let file_size = std::fs::metadata(channel_path("test-after-id"))
            .unwrap()
            .len();
        assert!(
            page1.next_offset < file_size,
            "after-id next_offset must be a continuation cursor, not EOF"
        );

        // Continuing from that cursor must yield messages 3 and 4, not skip them.
        let page2 = run_with_output(HistoryOptions {
            after_offset: Some(page1.next_offset),
            ..base
        })
        .unwrap();
        let bodies: Vec<String> = page2.messages.iter().map(|m| m.body.clone()).collect();
        assert_eq!(bodies, vec!["Message 3", "Message 4"]);
    }

    #[test]
    fn test_parse_datetime() {
        assert!(parse_datetime("2026-01-23").is_ok());
        assert!(parse_datetime("2026-01-23T12:00:00Z").is_ok());
        assert!(parse_datetime("2026-01-23 12:00:00").is_ok());
        assert!(parse_datetime("invalid").is_err());
    }
}
