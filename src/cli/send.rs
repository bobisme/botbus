//! Send messages to channels or agents.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use tracing::instrument;

use crate::attachments::{AttachmentCache, AttachmentSource, attachments_dir};
use crate::core::channel::{dm_channel_name, is_valid_channel_name};
use crate::core::flags::parse_flags;
use crate::core::identity::require_agent;
use crate::core::message::{Attachment, Message};
use crate::core::project::{channel_path, data_dir};
use crate::storage::jsonl::append_record;
use crate::sync::auto_commit::auto_commit_after_send;

/// Simple message send (no labels or attachments) - for internal use and tests.
pub fn run_simple(target: String, message: String, agent: Option<&str>) -> Result<()> {
    run(target, message, None, vec![], vec![], false, agent)
}

/// Send a message with pre-parsed Attachment structs (for Telegram bridge).
#[instrument(skip(message, _meta, labels, attachments), fields(target = %target, no_hooks))]
pub fn run_with_attachments(
    target: String,
    message: String,
    _meta: Option<String>,
    labels: Vec<String>,
    attachments: Vec<Attachment>,
    no_hooks: bool,
    agent: Option<&str>,
) -> Result<()> {
    let agent_name = require_agent(agent)?;

    let target_str = target.strip_prefix('#').unwrap_or(&target);

    if target_str == "claims" {
        bail!("Cannot send messages to #claims - this is a system channel.");
    }

    let channel = if target_str.starts_with('@') {
        let other_agent = target_str.trim_start_matches('@');
        if other_agent.is_empty() {
            bail!("Invalid DM target: {}", target_str);
        }
        dm_channel_name(&agent_name, other_agent)
    } else {
        if !is_valid_channel_name(target_str) {
            bail!("Invalid channel name: '{}'", target_str);
        }
        target_str.to_string()
    };

    // Parse !flags from message body
    let parsed = parse_flags(&message);
    let hook_flags = parsed.flags;

    // Store original body — flags are meaningful to downstream consumers
    let mut msg = Message::new(&agent_name, &channel, &message);

    if !labels.is_empty() {
        msg = msg.with_labels(labels);
    }

    if !attachments.is_empty() {
        msg = msg.with_attachments(attachments);
    }

    let path = channel_path(&channel);
    append_record(&path, &msg)
        .with_context(|| format!("Failed to send message to #{}", channel))?;

    // Evaluate hooks unless suppressed by CLI flag or !flags in message
    if !no_hooks && !hook_flags.suppress_all() {
        super::hooks::evaluate_hooks_with_flags(
            &channel,
            &msg.id.to_string(),
            &message,
            msg.meta.as_ref(),
            &agent_name,
            &msg.mentions,
            &hook_flags,
        );
    }

    Ok(())
}

/// Send a message to a channel or agent.
#[instrument(skip(message, _meta, labels, attachments), fields(target = %target, no_hooks))]
pub fn run(
    target: String,
    message: String,
    _meta: Option<String>,
    labels: Vec<String>,
    attachments: Vec<String>,
    no_hooks: bool,
    agent: Option<&str>,
) -> Result<()> {
    // Get current agent from env var or explicit flag
    let agent_name = require_agent(agent)?;

    // Strip # prefix if present (common user pattern)
    let target = target.strip_prefix('#').unwrap_or(&target);

    // Block sending to reserved channels
    if target == "claims" {
        bail!(
            "Cannot send messages to #claims - this is a system channel.\n\n\
             The #claims channel is reserved for claim/release announcements.\n\
             Claim actions automatically post to this channel."
        );
    }

    // Determine channel name
    let channel = if target.starts_with('@') {
        // DM to another agent
        let other_agent = target.trim_start_matches('@');
        if other_agent.is_empty() {
            bail!("Invalid DM target: {}", target);
        }
        dm_channel_name(&agent_name, other_agent)
    } else {
        // Regular channel
        if !is_valid_channel_name(target) {
            bail!(
                "Invalid channel name: '{}'\n\n\
                 Channel names must be lowercase alphanumeric with hyphens.\n\
                 Examples: general, backend, webapp-api, project-topic",
                target
            );
        }
        target.to_string()
    };

    // Parse !flags from message body
    let parsed = parse_flags(&message);
    let hook_flags = parsed.flags;

    // Parse attachments (format: "name:path", "path", or "url:https://...")
    let parsed_attachments = parse_attachments_for_channel(&attachments, &channel, &agent_name)?;

    // Store original body — flags are meaningful to downstream consumers
    let mut msg = Message::new(&agent_name, &channel, &message);

    if !labels.is_empty() {
        msg = msg.with_labels(labels);
    }

    if !parsed_attachments.is_empty() {
        msg = msg.with_attachments(parsed_attachments);
    }

    let path = channel_path(&channel);
    append_record(&path, &msg)
        .with_context(|| format!("Failed to send message to #{}", channel))?;

    // Auto-commit after sending (best-effort, silent on failure)
    auto_commit_after_send(&data_dir(), &channel);

    // Evaluate channel hooks (may block briefly for --release-on-exit hooks)
    // Skip if CLI --no-hooks flag is set or !nohooks flag is in message
    let hook_results = if no_hooks || hook_flags.suppress_all() {
        vec![]
    } else {
        super::hooks::evaluate_hooks_with_flags(
            &channel,
            &msg.id.to_string(),
            &message,
            msg.meta.as_ref(),
            &agent_name,
            &msg.mentions,
            &hook_flags,
        )
    };

    // Output confirmation
    if target.starts_with('@') {
        println!("{} Message sent to {}", "Sent:".green(), target.cyan());
        // Tip for DMs - mention the wait command
        println!(
            "{}",
            format!("Tip: rite wait -c {} -t 60 to wait for reply", target).dimmed()
        );
    } else {
        println!("{} Message sent to #{}", "Sent:".green(), channel.cyan());
    }

    // Show hook results
    for result in &hook_results {
        println!(
            "{} Hook {} fired: {}",
            "⚡".dimmed(),
            result.hook_id.cyan(),
            result.command_display.dimmed()
        );
        if result.batch_count > 1 {
            println!(
                "  {} {} triggers (this message plus {} queued behind the last spawn)",
                "Batched:".green(),
                result.batch_count,
                result.batch_count - 1
            );
        }
        if let Some(pattern) = &result.claim_pattern {
            if let Some(ttl) = result.claim_ttl {
                println!("  {} {} (TTL: {}s)", "Claimed:".green(), pattern, ttl);
            } else {
                println!(
                    "  {} {} (released on command exit)",
                    "Claimed:".green(),
                    pattern
                );
            }
            println!(
                "  {}",
                format!("Release: rite release {}", pattern).dimmed()
            );
        }
    }

    Ok(())
}

fn parse_attachments_for_channel(
    specs: &[String],
    channel: &str,
    agent: &str,
) -> Result<Vec<Attachment>> {
    let mut attachments = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_default();

    for spec in specs {
        let attachment = if let Some(url) = spec.strip_prefix("url:") {
            let name = attachment_name_from_url(url);
            Attachment::url(name, url)
        } else if spec.starts_with("http://") || spec.starts_with("https://") {
            // URL attachment
            let name = attachment_name_from_url(spec);
            Attachment::url(name, spec)
        } else {
            // Try the whole spec as a path first (handles colons in filenames)
            let full_path = cwd.join(spec);
            if full_path.exists() {
                let name = std::path::Path::new(spec)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(spec);
                store_file_in_cache(&full_path, name, channel, agent)?
            } else if let Some((name, path)) = spec.split_once(':') {
                // Fall back to name:path syntax
                let full_path = cwd.join(path);
                if !full_path.exists() {
                    bail!("Attachment file not found: {}", spec);
                }
                store_file_in_cache(&full_path, name, channel, agent)?
            } else {
                bail!("Attachment file not found: {}", spec);
            }
        };
        attachments.push(attachment);
    }

    Ok(attachments)
}

fn attachment_name_from_url(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    without_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("link")
        .to_string()
}

/// Read a file, store it in the attachment cache, and return a File attachment.
fn store_file_in_cache(
    file_path: &std::path::Path,
    name: &str,
    channel: &str,
    agent: &str,
) -> Result<Attachment> {
    let canonical_path = file_path
        .canonicalize()
        .with_context(|| format!("Failed to resolve path: {}", file_path.display()))?;

    let bytes = std::fs::read(&canonical_path)
        .with_context(|| format!("Failed to read attachment: {}", canonical_path.display()))?;

    let cache = AttachmentCache::new(attachments_dir())?;
    let stored = cache.store(
        &bytes,
        name,
        AttachmentSource::Cli {
            agent: agent.to_string(),
            channel: channel.to_string(),
        },
    )?;

    Ok(Attachment::file(name, stored.path.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::identity::AGENT_ENV_VAR;
    use crate::core::project::{DATA_DIR_ENV_VAR, ensure_data_dir};
    use crate::storage::jsonl::read_records;
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
    fn test_send_to_channel() {
        let _env = TestEnv::new();

        // Use explicit agent name
        run(
            "test-general".to_string(),
            "Hello, world!".to_string(),
            None,
            vec![],
            vec![],
            false,
            Some("test-sender"),
        )
        .unwrap();

        let messages: Vec<Message> = read_records(&channel_path("test-general")).unwrap();
        assert!(!messages.is_empty());
        let last = messages.last().unwrap();
        assert_eq!(last.body, "Hello, world!");
        assert_eq!(last.agent, "test-sender");
    }

    #[test]
    #[serial]
    fn test_send_dm() {
        let _env = TestEnv::new();

        run(
            "@other-agent".to_string(),
            "Private message".to_string(),
            None,
            vec![],
            vec![],
            false,
            Some("test-sender"),
        )
        .unwrap();

        // DM channel should be created with sorted names
        let dm_path = channel_path("_dm_other-agent_test-sender");
        let messages: Vec<Message> = read_records(&dm_path).unwrap();
        assert!(!messages.is_empty());
        assert_eq!(messages.last().unwrap().body, "Private message");
    }

    #[test]
    #[serial]
    fn test_send_invalid_channel() {
        let _env = TestEnv::new();

        let result = run(
            "INVALID".to_string(),
            "test".to_string(),
            None,
            vec![],
            vec![],
            false,
            Some("test-sender"),
        );
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_send_without_identity() {
        let _env = TestEnv::new();

        // Ensure no env identity
        unsafe {
            env::remove_var(AGENT_ENV_VAR);
            env::remove_var("AGENT");
        }

        let result = run(
            "general".to_string(),
            "test".to_string(),
            None,
            vec![],
            vec![],
            false,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_send_with_labels() {
        let _env = TestEnv::new();

        run(
            "test-labeled".to_string(),
            "Bug fix ready".to_string(),
            None,
            vec!["bug".to_string(), "ready".to_string()],
            vec![],
            false,
            Some("test-sender"),
        )
        .unwrap();

        let messages: Vec<Message> = read_records(&channel_path("test-labeled")).unwrap();
        assert!(!messages.is_empty());
        assert_eq!(messages.last().unwrap().labels, vec!["bug", "ready"]);
    }

    #[test]
    #[serial]
    fn test_send_attachment_metadata_uses_resolved_channel() {
        let _env = TestEnv::new();
        let file_dir = TempDir::new().unwrap();
        let file_path = file_dir.path().join("notes.txt");
        std::fs::write(&file_path, "attachment body").unwrap();

        run(
            "#actual-channel".to_string(),
            "See attached".to_string(),
            None,
            vec![],
            vec![file_path.to_string_lossy().to_string()],
            false,
            Some("test-sender"),
        )
        .unwrap();

        let messages: Vec<Message> = read_records(&channel_path("actual-channel")).unwrap();
        let attachment = messages.last().unwrap().attachments.first().unwrap();
        let crate::core::message::AttachmentContent::File { path } = &attachment.content else {
            panic!("expected file attachment");
        };

        let cache = crate::attachments::AttachmentCache::new(crate::attachments::attachments_dir())
            .unwrap();
        let metadata = cache.read_metadata(std::path::Path::new(path)).unwrap();
        assert_eq!(metadata.source_channel.as_deref(), Some("actual-channel"));
        assert_eq!(metadata.stored_by, "test-sender");
    }

    #[test]
    fn test_parse_url_attachment_prefix() {
        let attachments = parse_attachments_for_channel(
            &["url:https://example.com/files/report.pdf?download=1".to_string()],
            "general",
            "test-sender",
        )
        .unwrap();

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].name, "report.pdf");
        let crate::core::message::AttachmentContent::Url { url } = &attachments[0].content else {
            panic!("expected url attachment");
        };
        assert_eq!(url, "https://example.com/files/report.pdf?download=1");
    }

    #[test]
    #[serial]
    fn test_send_to_claims_channel_blocked() {
        let _env = TestEnv::new();

        // Try to send to #claims (with # prefix)
        let result = run(
            "#claims".to_string(),
            "test message".to_string(),
            None,
            vec![],
            vec![],
            false,
            Some("test-sender"),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("system channel"));

        // Try without # prefix
        let result = run(
            "claims".to_string(),
            "test message".to_string(),
            None,
            vec![],
            vec![],
            false,
            Some("test-sender"),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("system channel"));
    }
}
