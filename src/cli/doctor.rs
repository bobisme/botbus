//! Doctor command for environment validation.
//!
//! Checks that the Rite environment is properly configured for agent use.

use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::OutputFormat;
use crate::core::claim::FileClaim;
use crate::core::identity::resolve_agent;
use crate::core::message::Message;
use crate::core::names::is_valid_name;
use crate::core::project::{
    channels_dir, claims_path, data_dir, hook_queue_path, hooks_path, index_path, state_path,
    statuses_path,
};
use crate::core::status::AgentStatusEntry;
use crate::storage::jsonl::{DamagedField, ScanIssues, scan_issues};
use crate::sync::git;

/// How many skipped-line details to show before truncating.
const MAX_SKIPPED_DETAILS: usize = 5;

/// A single check result.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

/// A JSONL line that the current build cannot read, as reported by doctor.
#[derive(Debug, Clone, Serialize)]
pub struct SkippedRecord {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    pub byte_offset: u64,
    pub error: String,
}

impl From<&crate::storage::jsonl::SkippedLine> for SkippedRecord {
    fn from(skip: &crate::storage::jsonl::SkippedLine) -> Self {
        Self {
            file: skip.path.display().to_string(),
            line: skip.line,
            byte_offset: skip.byte_offset,
            error: skip.error.clone(),
        }
    }
}

/// A field dropped from a record that was otherwise read, as reported by doctor.
///
/// Separate from [`SkippedRecord`] because the consequences differ: a skipped
/// line means a message is missing, a damaged field means a message is present
/// but has lost part of its meaning. Today the only one is a reply anchor,
/// whose loss turns a reply into a top-level message.
#[derive(Debug, Clone, Serialize)]
pub struct DamagedFieldRecord {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    pub byte_offset: u64,
    pub field: String,
    pub value: String,
}

impl From<&DamagedField> for DamagedFieldRecord {
    fn from(damaged: &DamagedField) -> Self {
        Self {
            file: damaged.path.display().to_string(),
            line: damaged.line,
            byte_offset: damaged.byte_offset,
            field: damaged.field.to_string(),
            value: damaged.value.clone(),
        }
    }
}

/// Full doctor report.
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
    pub pass_count: usize,
    pub warn_count: usize,
    pub fail_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<String>,
    /// Total number of JSONL lines this build had to skip while reading the
    /// data directory. Recomputed on every run — never persisted.
    pub skipped_line_count: usize,
    /// Details of the skipped lines (all of them, for machine consumers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_records: Vec<SkippedRecord>,
    /// Total number of field values this build had to drop from records it
    /// could otherwise read. Recomputed on every run, like the skip count.
    pub damaged_field_count: usize,
    /// Details of the dropped field values (all of them, for machine consumers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub damaged_fields: Vec<DamagedFieldRecord>,
}

impl DoctorReport {
    fn new() -> Self {
        Self {
            checks: Vec::new(),
            pass_count: 0,
            warn_count: 0,
            fail_count: 0,
            advice: Vec::new(),
            skipped_line_count: 0,
            skipped_records: Vec::new(),
            damaged_field_count: 0,
            damaged_fields: Vec::new(),
        }
    }

    fn add(&mut self, check: Check) {
        match check.status {
            CheckStatus::Pass => self.pass_count += 1,
            CheckStatus::Warn => self.warn_count += 1,
            CheckStatus::Fail => self.fail_count += 1,
        }
        self.checks.push(check);
    }

    fn is_healthy(&self) -> bool {
        self.fail_count == 0
    }
}

/// Run all doctor checks.
pub fn run(format: OutputFormat) -> Result<()> {
    let mut report = DoctorReport::new();

    // Check 1: Data directory exists
    check_data_dir(&mut report);

    // Check 2: Agent identity is set
    check_agent_identity(&mut report);

    // Check 3: Channels directory is writable
    check_channels_dir(&mut report);

    // Check 4: Claims file location is writable
    check_claims(&mut report);

    // Check 5: State file location is writable
    check_state(&mut report);

    // Check 6: Index (FTS) location
    check_index(&mut report);

    // Check 7: Data directory permissions (security)
    check_permissions(&mut report);

    // Check 8: Git availability (for sync features)
    check_git_available(&mut report);

    // Check 9: JSONL records this build cannot read (mixed-version sync)
    check_record_readability(&mut report);

    // Build advice based on failed/warned checks
    for check in &report.checks {
        if let Some(ref suggestion) = check.suggestion
            && check.status == CheckStatus::Fail
        {
            report.advice.push(suggestion.clone());
        }
    }

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        OutputFormat::Pretty | OutputFormat::Text => {
            print_report(&report);
        }
    }

    if !report.is_healthy() {
        std::process::exit(1);
    }

    Ok(())
}

fn check_data_dir(report: &mut DoctorReport) {
    let path = data_dir();
    if path.exists() {
        report.add(Check {
            name: "data_directory".to_string(),
            status: CheckStatus::Pass,
            message: format!("Data directory exists: {}", path.display()),
            suggestion: None,
        });
    } else {
        report.add(Check {
            name: "data_directory".to_string(),
            status: CheckStatus::Fail,
            message: format!("Data directory missing: {}", path.display()),
            suggestion: Some("Run: rite init".to_string()),
        });
    }
}

fn check_agent_identity(report: &mut DoctorReport) {
    match resolve_agent(None) {
        Some(ref agent) => {
            if is_valid_name(agent) {
                report.add(Check {
                    name: "agent_identity".to_string(),
                    status: CheckStatus::Pass,
                    message: format!("Agent identity: {}", agent),
                    suggestion: None,
                });
            } else {
                report.add(Check {
                    name: "agent_identity".to_string(),
                    status: CheckStatus::Warn,
                    message: format!("Agent name '{}' is not valid kebab-case", agent),
                    suggestion: Some("Use: export RITE_AGENT=$(rite generate-name)".to_string()),
                });
            }
        }
        None => {
            report.add(Check {
                name: "agent_identity".to_string(),
                status: CheckStatus::Warn,
                message: "No agent identity set (RITE_AGENT not defined)".to_string(),
                suggestion: Some("Run: export RITE_AGENT=$(rite generate-name)".to_string()),
            });
        }
    }
}

fn check_channels_dir(report: &mut DoctorReport) {
    let path = channels_dir();
    if path.exists() {
        if is_writable(&path) {
            report.add(Check {
                name: "channels_directory".to_string(),
                status: CheckStatus::Pass,
                message: "Channels directory is writable".to_string(),
                suggestion: None,
            });
        } else {
            report.add(Check {
                name: "channels_directory".to_string(),
                status: CheckStatus::Fail,
                message: format!("Channels directory not writable: {}", path.display()),
                suggestion: Some(format!("Check permissions on {}", path.display())),
            });
        }
    } else {
        report.add(Check {
            name: "channels_directory".to_string(),
            status: CheckStatus::Fail,
            message: "Channels directory missing".to_string(),
            suggestion: Some("Run: rite init".to_string()),
        });
    }
}

fn check_claims(report: &mut DoctorReport) {
    let path = claims_path();
    if let Some(parent) = path.parent() {
        if parent.exists() {
            if is_writable(parent) {
                report.add(Check {
                    name: "claims_storage".to_string(),
                    status: CheckStatus::Pass,
                    message: "Claims storage location is writable".to_string(),
                    suggestion: None,
                });
            } else {
                report.add(Check {
                    name: "claims_storage".to_string(),
                    status: CheckStatus::Fail,
                    message: "Claims storage location not writable".to_string(),
                    suggestion: Some(format!("Check permissions on {}", parent.display())),
                });
            }
        } else {
            report.add(Check {
                name: "claims_storage".to_string(),
                status: CheckStatus::Fail,
                message: "Claims directory missing".to_string(),
                suggestion: Some("Run: rite init".to_string()),
            });
        }
    }
}

fn check_state(report: &mut DoctorReport) {
    let path = state_path();
    if let Some(parent) = path.parent()
        && parent.exists()
    {
        if is_writable(parent) {
            report.add(Check {
                name: "state_storage".to_string(),
                status: CheckStatus::Pass,
                message: "State storage location is writable".to_string(),
                suggestion: None,
            });
        } else {
            report.add(Check {
                name: "state_storage".to_string(),
                status: CheckStatus::Fail,
                message: "State storage location not writable".to_string(),
                suggestion: Some(format!("Check permissions on {}", parent.display())),
            });
        }
    }
}

fn check_index(report: &mut DoctorReport) {
    let path = index_path();
    if let Some(parent) = path.parent()
        && parent.exists()
    {
        if is_writable(parent) {
            let index_exists = path.exists();
            report.add(Check {
                name: "search_index".to_string(),
                status: CheckStatus::Pass,
                message: if index_exists {
                    "Search index exists and location is writable".to_string()
                } else {
                    "Search index location is writable (index not yet created)".to_string()
                },
                suggestion: None,
            });
        } else {
            report.add(Check {
                name: "search_index".to_string(),
                status: CheckStatus::Warn,
                message: "Search index location not writable".to_string(),
                suggestion: Some(format!("Check permissions on {}", parent.display())),
            });
        }
    }
}

fn check_permissions(report: &mut DoctorReport) {
    let path = data_dir();
    if !path.exists() {
        return; // Already reported in check_data_dir
    }

    match fs::metadata(&path) {
        Ok(meta) => {
            let mode = meta.permissions().mode();
            // Check if group/other have write access (security concern)
            let group_write = mode & 0o020 != 0;
            let other_write = mode & 0o002 != 0;

            if group_write || other_write {
                report.add(Check {
                    name: "permissions".to_string(),
                    status: CheckStatus::Warn,
                    message: format!(
                        "Data directory has permissive permissions: {:o}",
                        mode & 0o777
                    ),
                    suggestion: Some(format!("Consider: chmod 700 {}", path.display())),
                });
            } else {
                report.add(Check {
                    name: "permissions".to_string(),
                    status: CheckStatus::Pass,
                    message: format!("Data directory permissions: {:o}", mode & 0o777),
                    suggestion: None,
                });
            }
        }
        Err(e) => {
            report.add(Check {
                name: "permissions".to_string(),
                status: CheckStatus::Warn,
                message: format!("Could not check permissions: {}", e),
                suggestion: None,
            });
        }
    }
}

fn is_writable(path: &Path) -> bool {
    // Try to check write permission
    match fs::metadata(path) {
        Ok(meta) => {
            let mode = meta.permissions().mode();
            // Check if current user has write permission
            // This is a simplified check - proper check would need to verify uid/gid
            (mode & 0o200) != 0 || (mode & 0o020) != 0 || (mode & 0o002) != 0
        }
        Err(_) => false,
    }
}

/// Report data this build has to drop when reading.
///
/// Two kinds, counted separately because they mean different things:
///
/// - **Skipped lines** — records this build could not read at all. Readers skip
///   them so one bad record cannot deny access to a whole file.
/// - **Damaged fields** — records this build read, minus one value it could not
///   read. Today that is only `Message::reply_to`: an unreadable reply anchor
///   is dropped so the message survives, which quietly turns a reply into a
///   top-level message. Unquietly, thanks to this check.
///
/// Note that a record carrying an unrecognized *type* is neither: it is kept
/// verbatim (see [`crate::core::wire`]) and never appears here. Everything
/// counted here is data this build genuinely could not read.
///
/// The counts are recomputed from disk on every run rather than persisted:
/// whether a value is readable is a property of *this binary's* schema, not of
/// the file, so a stored count would go stale the moment either side is
/// upgraded, and writing to the data directory during a read-only health check
/// would churn git sync.
fn check_record_readability(report: &mut DoctorReport) {
    let mut issues = ScanIssues::default();
    let mut files_scanned = 0usize;
    let mut scan_errors: Vec<String> = Vec::new();

    let mut scan = |result: Result<ScanIssues>,
                    issues: &mut ScanIssues,
                    files_scanned: &mut usize| match result {
        Ok(found) => {
            *files_scanned += 1;
            issues.extend(found);
        }
        Err(e) => scan_errors.push(e.to_string()),
    };

    // Channel files hold messages.
    if let Ok(entries) = fs::read_dir(channels_dir()) {
        let mut channel_files: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        channel_files.sort();

        for path in channel_files {
            scan(
                scan_issues::<Message>(&path),
                &mut issues,
                &mut files_scanned,
            );
        }
    }

    // Top-level record files.
    scan(
        scan_issues::<FileClaim>(&claims_path()),
        &mut issues,
        &mut files_scanned,
    );
    scan(
        scan_issues::<AgentStatusEntry>(&statuses_path()),
        &mut issues,
        &mut files_scanned,
    );
    scan(
        scan_issues::<crate::core::hook::Hook>(&hooks_path()),
        &mut issues,
        &mut files_scanned,
    );
    scan(
        scan_issues::<crate::core::hook::QueuedTrigger>(&hook_queue_path()),
        &mut issues,
        &mut files_scanned,
    );

    let ScanIssues { skipped, damaged } = issues;

    report.skipped_line_count = skipped.len();
    report.skipped_records = skipped.iter().map(SkippedRecord::from).collect();
    report.damaged_field_count = damaged.len();
    report.damaged_fields = damaged.iter().map(DamagedFieldRecord::from).collect();

    if !scan_errors.is_empty() {
        report.add(Check {
            name: "record_readability".to_string(),
            status: CheckStatus::Warn,
            message: format!("Could not scan some data files: {}", scan_errors.join("; ")),
            suggestion: Some(format!("Check permissions on {}", data_dir().display())),
        });
        return;
    }

    if skipped.is_empty() && damaged.is_empty() {
        report.add(Check {
            name: "record_readability".to_string(),
            status: CheckStatus::Pass,
            message: format!("All records readable across {} file(s)", files_scanned),
            suggestion: None,
        });
        return;
    }

    let mut files: Vec<&Path> = skipped
        .iter()
        .map(|s| s.path.as_path())
        .chain(damaged.iter().map(|d| d.path.as_path()))
        .collect();
    files.sort_unstable();
    files.dedup();

    let mut parts: Vec<String> = Vec::new();
    if !skipped.is_empty() {
        parts.push(format!(
            "skipped {} unreadable line(s): {}",
            skipped.len(),
            detail_of(skipped.iter().map(|s| s.to_string()), skipped.len())
        ));
    }
    if !damaged.is_empty() {
        parts.push(format!(
            "dropped {} unreadable field value(s): {}",
            damaged.len(),
            detail_of(damaged.iter().map(|d| d.to_string()), damaged.len())
        ));
    }

    let mut suggestion = String::from(
        "This data is damaged, or was written by a rite that changed a record type this build \
         already knows — records whose type is merely unrecognized are read fine and are not \
         counted here. Inspect the reported file and line; upgrade this install \
         (cargo install rite) if the writer is newer.",
    );
    if !damaged.is_empty() {
        suggestion.push_str(
            " A dropped `reply_to` makes a reply read as a top-level message, so anything \
             waiting on an answer to a specific message will not see it.",
        );
    }

    report.add(Check {
        name: "record_readability".to_string(),
        status: CheckStatus::Warn,
        message: format!("Across {} file(s): {}", files.len(), parts.join("; and ")),
        suggestion: Some(suggestion),
    });
}

/// Join the first few details, noting how many were left out.
fn detail_of(details: impl Iterator<Item = String>, total: usize) -> String {
    let shown: Vec<String> = details.take(MAX_SKIPPED_DETAILS).collect();
    let mut text = shown.join("; ");
    if total > MAX_SKIPPED_DETAILS {
        text.push_str(&format!(" (+{} more)", total - MAX_SKIPPED_DETAILS));
    }
    text
}

fn check_git_available(report: &mut DoctorReport) {
    if git::check_git_available() {
        report.add(Check {
            name: "git_available".to_string(),
            status: CheckStatus::Pass,
            message: "Git is installed and available".to_string(),
            suggestion: None,
        });
    } else {
        report.add(Check {
            name: "git_available".to_string(),
            status: CheckStatus::Warn,
            message: "Git is not installed or not in PATH".to_string(),
            suggestion: Some(
                "Install git to use sync features (rite sync init/push/pull)".to_string(),
            ),
        });
    }
}

fn print_report(report: &DoctorReport) {
    println!("{}", "Rite Doctor".bold());
    println!();

    for check in &report.checks {
        let (icon, color) = match check.status {
            CheckStatus::Pass => ("✓", "green"),
            CheckStatus::Warn => ("!", "yellow"),
            CheckStatus::Fail => ("✗", "red"),
        };

        let icon_colored = match color {
            "green" => icon.green(),
            "yellow" => icon.yellow(),
            "red" => icon.red(),
            _ => icon.normal(),
        };

        println!("{} {}", icon_colored, check.message);

        if let Some(ref suggestion) = check.suggestion {
            println!("  {} {}", "→".dimmed(), suggestion.cyan());
        }
    }

    println!();
    println!(
        "Summary: {} passed, {} warnings, {} failed",
        report.pass_count.to_string().green(),
        report.warn_count.to_string().yellow(),
        report.fail_count.to_string().red()
    );

    if report.is_healthy() {
        println!();
        println!("{}", "Environment is healthy!".green().bold());
    } else {
        println!();
        println!(
            "{}",
            "Environment has issues that need attention.".red().bold()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::project::DATA_DIR_ENV_VAR;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    #[test]
    #[serial]
    fn test_doctor_healthy_environment() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_str().unwrap();

        // Set up environment
        unsafe {
            env::set_var(DATA_DIR_ENV_VAR, temp_path);
            env::set_var("RITE_AGENT", "test-agent");
        }

        // Create required directories
        let channels = temp.path().join("channels");
        fs::create_dir_all(&channels).unwrap();

        let mut report = DoctorReport::new();
        check_data_dir(&mut report);
        check_agent_identity(&mut report);

        // Should have 2 passes (data_dir and agent_identity)
        assert_eq!(report.pass_count, 2);
        assert_eq!(report.fail_count, 0);

        // Cleanup
        unsafe {
            env::remove_var(DATA_DIR_ENV_VAR);
            env::remove_var("RITE_AGENT");
        }
    }

    #[test]
    #[serial]
    fn test_doctor_reports_skipped_lines() {
        use std::io::Write;

        let temp = TempDir::new().unwrap();
        unsafe {
            env::set_var(DATA_DIR_ENV_VAR, temp.path().to_str().unwrap());
        }

        let channels = temp.path().join("channels");
        fs::create_dir_all(&channels).unwrap();
        let channel = channels.join("general.jsonl");

        let good = crate::core::message::Message::new("agent", "general", "hello");
        crate::storage::jsonl::append_record(&channel, &good).unwrap();
        {
            let mut file = fs::OpenOptions::new().append(true).open(&channel).unwrap();
            // Line 2: a variant from a newer rite. Readable, must NOT be counted.
            writeln!(
                file,
                r#"{{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"newer","channel":"general","body":"future","meta":{{"type":"reaction","emoji":"+1"}}}}"#
            )
            .unwrap();
            // Line 3: known meta type with a damaged body. Corruption, must be counted.
            writeln!(
                file,
                r#"{{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FBW","agent":"damaged","channel":"general","body":"corrupt","meta":{{"type":"claim"}}}}"#
            )
            .unwrap();
            // Line 4: missing the required `id`/`ts` fields entirely.
            writeln!(file, r#"{{"kind":"something-new","body":"x"}}"#).unwrap();
        }

        let mut report = DoctorReport::new();
        check_record_readability(&mut report);

        assert_eq!(
            report.skipped_line_count, 2,
            "the future record must not be counted, the two damaged ones must be"
        );
        assert_eq!(report.warn_count, 1);
        assert_eq!(report.skipped_records.len(), 2);
        assert_eq!(report.skipped_records[0].line, Some(3));
        assert!(report.skipped_records[0].error.contains("corrupt"));
        assert_eq!(report.skipped_records[1].line, Some(4));
        assert!(report.skipped_records[0].file.ends_with("general.jsonl"));
        assert!(
            report.checks[0]
                .message
                .contains("skipped 2 unreadable line")
        );
        assert_eq!(
            report.damaged_field_count, 0,
            "a lost line is not a damaged field"
        );

        // A clean data directory passes and reports a zero count.
        fs::remove_file(&channel).unwrap();
        crate::storage::jsonl::append_record(&channel, &good).unwrap();
        let mut clean = DoctorReport::new();
        check_record_readability(&mut clean);
        assert_eq!(clean.skipped_line_count, 0);
        assert_eq!(clean.damaged_field_count, 0);
        assert_eq!(clean.pass_count, 1);

        unsafe {
            env::remove_var(DATA_DIR_ENV_VAR);
        }
    }

    /// A dropped reply anchor is data loss. It must be counted and named, not
    /// merely absent — an anchor that vanishes quietly stops an acknowledgment
    /// from correlating with the request it answers, and nothing would say why.
    #[test]
    #[serial]
    fn test_doctor_reports_dropped_reply_anchors() {
        use std::io::Write;

        let temp = TempDir::new().unwrap();
        unsafe {
            env::set_var(DATA_DIR_ENV_VAR, temp.path().to_str().unwrap());
        }

        let channels = temp.path().join("channels");
        fs::create_dir_all(&channels).unwrap();
        let channel = channels.join("general.jsonl");

        let question = crate::core::message::Message::new("alice", "general", "question");
        let answer = crate::core::message::Message::new("bob", "general", "answer")
            .with_reply_to(question.id);
        crate::storage::jsonl::append_record(&channel, &question).unwrap();
        crate::storage::jsonl::append_record(&channel, &answer).unwrap();
        {
            let mut file = fs::OpenOptions::new().append(true).open(&channel).unwrap();
            // Line 3: an anchor that is not a ULID.
            writeln!(
                file,
                r#"{{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"mangled","channel":"general","body":"bad anchor","reply_to":"????"}}"#
            )
            .unwrap();
            // Line 4: an anchor a newer rite gave a different shape.
            writeln!(
                file,
                r#"{{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FBW","agent":"future","channel":"general","body":"future anchor","reply_to":{{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}}}}"#
            )
            .unwrap();
        }

        let mut report = DoctorReport::new();
        check_record_readability(&mut report);

        assert_eq!(
            report.skipped_line_count, 0,
            "the records themselves are readable and must be kept"
        );
        assert_eq!(report.damaged_field_count, 2);
        assert_eq!(report.warn_count, 1, "lost data cannot be a Pass");

        assert_eq!(report.damaged_fields.len(), 2);
        for record in &report.damaged_fields {
            assert_eq!(record.field, crate::core::message::REPLY_TO_FIELD);
            assert!(record.file.ends_with("general.jsonl"));
        }
        assert_eq!(report.damaged_fields[0].line, Some(3));
        assert!(report.damaged_fields[0].value.contains("????"));
        assert_eq!(report.damaged_fields[1].line, Some(4));

        // The human-visible line names the count, the field, and the file.
        let message = &report.checks[0].message;
        assert!(
            message.contains("dropped 2 unreadable field value"),
            "{message}"
        );
        assert!(message.contains("reply_to"), "{message}");
        assert!(message.contains("general.jsonl"), "{message}");
        assert!(
            report.checks[0]
                .suggestion
                .as_ref()
                .unwrap()
                .contains("reply_to"),
            "the suggestion must explain what a lost anchor costs"
        );

        // A valid anchor is not damage.
        fs::remove_file(&channel).unwrap();
        crate::storage::jsonl::append_record(&channel, &question).unwrap();
        crate::storage::jsonl::append_record(&channel, &answer).unwrap();
        let mut clean = DoctorReport::new();
        check_record_readability(&mut clean);
        assert_eq!(clean.damaged_field_count, 0);
        assert_eq!(clean.pass_count, 1);

        unsafe {
            env::remove_var(DATA_DIR_ENV_VAR);
        }
    }

    #[test]
    #[serial]
    fn test_doctor_missing_identity() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_str().unwrap();

        unsafe {
            env::set_var(DATA_DIR_ENV_VAR, temp_path);
            env::remove_var("RITE_AGENT");
            env::remove_var("AGENT");
        }

        let mut report = DoctorReport::new();
        check_agent_identity(&mut report);

        assert_eq!(report.warn_count, 1);
        assert!(report.checks[0].suggestion.is_some());

        unsafe {
            env::remove_var(DATA_DIR_ENV_VAR);
        }
    }
}
