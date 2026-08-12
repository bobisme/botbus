//! `rite hooks set` changes a hook without replacing it (bn-3h0j).
//!
//! Before this existed, changing any field meant `hooks remove` followed by
//! `hooks add`. That is not an equivalent operation:
//!
//! - The ID changes, and the ID is the spawn-lease key
//!   (`spawn://<id>/<channel>`). A responder still running holds a lease on
//!   the old ID, so the replacement finds its own lease free and spawns a
//!   second agent alongside it.
//! - `last_fired` resets to null, so a cooldown hook can fire again
//!   immediately.
//! - Every field must be retyped, and anything the retyper does not know
//!   about is silently dropped. This is how the bn-20eh canary lost its
//!   lease: an external tool recreated the hook from its own template.
//!
//! These tests pin the properties that make `set` an update rather than a
//! replacement. Every test runs against its own `RITE_DATA_DIR` and spawns
//! nothing but `sh`.

mod common;

use common::TestProject;
use serde_json::Value;
use std::path::PathBuf;

fn hooks_file(project: &TestProject) -> PathBuf {
    project.data_path().join("hooks.jsonl")
}

fn hook_lines(project: &TestProject) -> Vec<String> {
    std::fs::read_to_string(hooks_file(project))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// The newest raw record for `hook_id` — what rite would read as the hook.
fn latest_record(project: &TestProject, hook_id: &str) -> Value {
    let line = hook_lines(project)
        .into_iter()
        .filter(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .is_some_and(|v| v["id"] == hook_id)
        })
        .next_back()
        .unwrap_or_else(|| panic!("no record for {hook_id} in hooks.jsonl"));
    serde_json::from_str(&line).expect("hook record must be valid JSON")
}

/// Add a mention hook and return its ID. `extra` carries flags such as
/// `--lease`.
fn add_hook(project: &TestProject, extra: &[&str]) -> String {
    let cwd = project.work_dir().to_string_lossy().to_string();
    let mut args = vec![
        "hooks",
        "add",
        "--channel",
        "rite",
        "--mention",
        "worker",
        "--cwd",
        &cwd,
    ];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", "sh", "-c", "true"]);

    project
        .run_rite_with_env(&args, Some("ops"))
        .assert_success();

    let line = hook_lines(project)
        .pop()
        .expect("hooks add must write a record");
    serde_json::from_str::<Value>(&line).unwrap()["id"]
        .as_str()
        .expect("hook id")
        .to_string()
}

fn set(project: &TestProject, hook_id: &str, args: &[&str]) -> common::RiteOutput {
    let mut full = vec!["hooks", "set", hook_id];
    full.extend_from_slice(args);
    project.run_rite_with_env(&full, Some("ops"))
}

/// The point of the command: the ID survives, so the lease key survives.
#[test]
fn test_set_preserves_hook_id() {
    let project = TestProject::with_name("hook-set-id");
    let hook_id = add_hook(&project, &["--lease", "--lease-ttl", "1800"]);

    set(&project, &hook_id, &["--priority", "5"]).assert_success();

    let record = latest_record(&project, &hook_id);
    assert_eq!(record["id"], hook_id.as_str(), "the ID must not change");
    assert_eq!(record["priority"], 5);

    // Exactly one hook is active — set appends, it does not fork the record
    // into a second hook the way remove+add does.
    let listing = project.run_rite_with_env(&["hooks", "list", "--format", "json"], Some("ops"));
    listing.assert_success();
    let parsed: Value = serde_json::from_str(&listing.stdout_str()).expect("valid json");
    let hooks = parsed["hooks"].as_array().expect("hooks array");
    assert_eq!(
        hooks.len(),
        1,
        "set must not create a second hook: {hooks:?}"
    );
}

/// A lease must survive an edit that says nothing about leases. This is the
/// bn-20eh failure in miniature: the canary lost its lease to a rewrite that
/// was only meant to refresh the command.
#[test]
fn test_set_preserves_lease_when_not_mentioned() {
    let project = TestProject::with_name("hook-set-keeps-lease");
    let hook_id = add_hook(
        &project,
        &["--lease", "--lease-ttl", "1800", "--max-batch", "7"],
    );

    set(
        &project,
        &hook_id,
        &["--description", "edict:rite:responder"],
    )
    .assert_success();

    let record = latest_record(&project, &hook_id);
    assert_eq!(
        record["lease"]["ttl_secs"], 1800,
        "an unrelated edit must not drop the lease: {record}"
    );
    assert_eq!(record["lease"]["max_batch"], 7);
    assert_eq!(record["description"], "edict:rite:responder");
}

/// Tuning one lease knob must not reset the other.
#[test]
fn test_lease_ttl_alone_keeps_max_batch() {
    let project = TestProject::with_name("hook-set-lease-knobs");
    let hook_id = add_hook(
        &project,
        &["--lease", "--lease-ttl", "600", "--max-batch", "9"],
    );

    set(&project, &hook_id, &["--lease-ttl", "1200"]).assert_success();

    let record = latest_record(&project, &hook_id);
    assert_eq!(record["lease"]["ttl_secs"], 1200);
    assert_eq!(
        record["lease"]["max_batch"], 9,
        "--lease-ttl must not silently reset --max-batch: {record}"
    );
}

/// `--lease-ttl` implies the hook is leased, so enabling a lease on a
/// cooldown hook does not require repeating `--lease`.
#[test]
fn test_lease_ttl_enables_lease_on_cooldown_hook() {
    let project = TestProject::with_name("hook-set-lease-implied");
    let hook_id = add_hook(&project, &["--cooldown", "30s"]);

    assert!(
        latest_record(&project, &hook_id)["lease"].is_null(),
        "fixture must start unleased"
    );

    set(&project, &hook_id, &["--lease-ttl", "900"]).assert_success();

    let record = latest_record(&project, &hook_id);
    assert_eq!(record["lease"]["ttl_secs"], 900);
}

/// `--no-lease` restores cooldown behaviour.
#[test]
fn test_no_lease_clears_the_lease() {
    let project = TestProject::with_name("hook-set-no-lease");
    let hook_id = add_hook(&project, &["--lease", "--lease-ttl", "1800"]);

    set(&project, &hook_id, &["--no-lease"]).assert_success();

    let record = latest_record(&project, &hook_id);
    assert!(
        record["lease"].is_null(),
        "--no-lease must clear the lease: {record}"
    );
}

/// Fields the edit does not mention keep their values — including the ones a
/// hand-retyped `hooks add` is most likely to get wrong.
#[test]
fn test_unspecified_fields_are_untouched() {
    let project = TestProject::with_name("hook-set-untouched");
    let hook_id = add_hook(
        &project,
        &[
            "--cooldown",
            "45s",
            "--priority",
            "3",
            "--claim",
            "agent://worker",
            "--ttl",
            "600",
            "--claim-owner",
            "worker",
            "--description",
            "edict:rite:responder",
            "--require-flag",
            "dev",
        ],
    );
    let before = latest_record(&project, &hook_id);

    set(&project, &hook_id, &["--priority", "1"]).assert_success();

    let after = latest_record(&project, &hook_id);
    for field in [
        "cooldown_secs",
        "claim_release",
        "claim_owner",
        "description",
        "require_flag",
        "condition",
        "command",
        "cwd",
        "channel",
        "created_at",
        "created_by",
    ] {
        assert_eq!(
            before[field], after[field],
            "{field} changed but the edit never mentioned it"
        );
    }
    assert_eq!(after["priority"], 1);
}

/// `last_fired` must survive an edit.
///
/// remove+add resets it to null, which hands a cooldown hook a free firing
/// the instant it is reconfigured. An update has no reason to forget when the
/// hook last ran.
#[test]
fn test_last_fired_survives_an_edit() {
    let project = TestProject::with_name("hook-set-last-fired");
    let hook_id = add_hook(&project, &[]);

    // Plant a firing time rather than waiting for a real one — this test is
    // about the rewrite, not about the firing path.
    let fired_at = "2026-08-12T04:32:25.300037810Z";
    let mut record = latest_record(&project, &hook_id);
    record["last_fired"] = Value::String(fired_at.to_string());
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(hooks_file(&project))
            .expect("open hooks.jsonl");
        writeln!(file, "{}", serde_json::to_string(&record).unwrap()).expect("append");
    }

    set(&project, &hook_id, &["--priority", "4"]).assert_success();

    let after = latest_record(&project, &hook_id);
    assert_eq!(
        after["last_fired"], fired_at,
        "an edit must not reset the cooldown clock: {after}"
    );
}

/// A field this build has never heard of must survive an edit, or `set`
/// becomes a new way to reintroduce bn-14o5.
#[test]
fn test_unknown_fields_survive_an_edit() {
    let project = TestProject::with_name("hook-set-unknown-fields");
    let hook_id = add_hook(&project, &["--lease"]);

    // Append a record as a *newer* rite that had grown these fields.
    let mut record = latest_record(&project, &hook_id);
    record["steer_mode"] = Value::String("inject".to_string());
    record["future_limits"] = serde_json::json!({"max_depth": 3});
    record["lease"] = serde_json::json!({"ttl_secs": 900, "restart_policy": "never"});
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(hooks_file(&project))
            .expect("open hooks.jsonl");
        writeln!(file, "{}", serde_json::to_string(&record).unwrap()).expect("append");
    }

    set(&project, &hook_id, &["--priority", "2"]).assert_success();

    let after = latest_record(&project, &hook_id);
    assert_eq!(after["steer_mode"], "inject", "top-level unknown dropped");
    assert_eq!(after["future_limits"]["max_depth"], 3);
    assert_eq!(
        after["lease"]["restart_policy"], "never",
        "unknown lease field dropped: {after}"
    );
    assert_eq!(after["lease"]["ttl_secs"], 900, "known lease field changed");
    assert_eq!(after["priority"], 2);
}

/// Repointing a hook at a directory that does not exist is the mistake this
/// command is most likely to be used to fix, so it must not be accepted.
#[test]
fn test_set_rejects_missing_cwd() {
    let project = TestProject::with_name("hook-set-bad-cwd");
    let hook_id = add_hook(&project, &[]);
    let before = latest_record(&project, &hook_id);

    let result = set(
        &project,
        &hook_id,
        &["--cwd", "/nonexistent/path/for/rite/test"],
    );
    assert!(
        !result.success(),
        "missing cwd must fail: {}",
        result.stdout_str()
    );

    let after = latest_record(&project, &hook_id);
    assert_eq!(before, after, "a rejected edit must write nothing");
}

/// An edit that asks for nothing is a mistake, not a no-op append.
#[test]
fn test_empty_edit_is_rejected() {
    let project = TestProject::with_name("hook-set-empty");
    let hook_id = add_hook(&project, &[]);
    let before = hook_lines(&project).len();

    let result = set(&project, &hook_id, &[]);
    assert!(!result.success(), "empty edit must fail");
    assert_eq!(
        hook_lines(&project).len(),
        before,
        "a rejected edit must write nothing"
    );
}

#[test]
fn test_set_unknown_hook_fails() {
    let project = TestProject::with_name("hook-set-missing");
    add_hook(&project, &[]);

    let result = set(&project, "hk-nope", &["--priority", "1"]);
    assert!(!result.success(), "unknown hook id must fail");
}

/// The command can be replaced wholesale, which is what repointing a stale
/// `--env-inherit` list needs.
#[test]
fn test_set_replaces_command() {
    let project = TestProject::with_name("hook-set-command");
    let hook_id = add_hook(&project, &[]);

    set(&project, &hook_id, &["--", "sh", "-c", "echo replaced"]).assert_success();

    let record = latest_record(&project, &hook_id);
    assert_eq!(
        record["command"],
        serde_json::json!(["sh", "-c", "echo replaced"])
    );
}
