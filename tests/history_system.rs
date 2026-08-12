//! `rite history` hides machine bookkeeping by default (bn-36gv).
//!
//! Measured 2026-08-12 over the last 500 messages of four live channels:
//! #console 222/499 (44%) were hook-fired system lines, #wraith 33%, #maw
//! 21%, #rite 18%. The lines are long — a whole `vessel spawn --env-inherit
//! …` command — and are never what a reader opened the channel for.
//!
//! Two things the naive version gets wrong, both pinned here:
//!
//! - Hiding silently. A hook that fires and fails to spawn records
//!   `executed: false`, exactly like one skipped for cooldown, so the system
//!   line is often the only surviving evidence anything happened. The count
//!   is always reported.
//! - Hiding claim records. They are also machine-written, but 34,209 of the
//!   34,313 of them live in `#claims`. Folding them in would make
//!   `rite history claims` open on an empty channel.

mod common;

use common::TestProject;
use serde_json::Value;

/// Write a system message directly — the shape a hook firing produces.
fn append_system(project: &TestProject, channel: &str, body: &str) {
    use std::io::Write;
    let path = project
        .data_path()
        .join("channels")
        .join(format!("{channel}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).expect("channels dir");

    let id = ulid::Ulid::new();
    let record = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "id": id.to_string(),
        "agent": "system",
        "channel": channel,
        "body": body,
        // Externally tagged, matching what a real firing writes.
        "meta": {"type": "system", "event": {"hook_fired": {
                 "hook_id": "hk-abc", "command": ["vessel", "spawn"]}}},
    });

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open channel file");
    writeln!(file, "{}", serde_json::to_string(&record).unwrap()).expect("append");
}

fn history(project: &TestProject, args: &[&str]) -> common::RiteOutput {
    let mut full = vec!["history"];
    full.extend_from_slice(args);
    project.run_rite_with_env(&full, Some("reader"))
}

fn history_json(project: &TestProject, args: &[&str]) -> Value {
    let mut full: Vec<&str> = vec!["history"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["--format", "json"]);
    let out = project.run_rite_with_env(&full, Some("reader"));
    out.assert_success();
    serde_json::from_str(&out.stdout_str()).expect("valid json")
}

fn bodies(output: &Value) -> Vec<String> {
    output["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter()
                .map(|m| m["body"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn test_system_messages_are_hidden_by_default() {
    let project = TestProject::with_name("history-hide-system");
    project
        .run_rite_with_env(&["send", "demo", "real message"], Some("alice"))
        .assert_success();
    append_system(&project, "demo", "Hook hk-abc fired: vessel spawn ...");

    let output = history_json(&project, &["demo"]);
    let bodies = bodies(&output);
    assert_eq!(
        bodies,
        vec!["real message"],
        "system message must be hidden"
    );
}

#[test]
fn test_show_system_includes_them() {
    let project = TestProject::with_name("history-show-system");
    project
        .run_rite_with_env(&["send", "demo", "real message"], Some("alice"))
        .assert_success();
    append_system(&project, "demo", "Hook hk-abc fired: vessel spawn ...");

    let output = history_json(&project, &["demo", "--show-system"]);
    assert_eq!(bodies(&output).len(), 2, "--show-system must include them");
    assert!(
        output["hidden_system"].is_null(),
        "nothing was hidden, so the count must be absent: {output}"
    );
}

/// Hiding without saying so is how a broken hook stays unnoticed.
#[test]
fn test_hidden_count_is_reported() {
    let project = TestProject::with_name("history-hidden-count");
    project
        .run_rite_with_env(&["send", "demo", "real message"], Some("alice"))
        .assert_success();
    for _ in 0..3 {
        append_system(&project, "demo", "Hook hk-abc fired: vessel spawn ...");
    }

    let output = history_json(&project, &["demo"]);
    assert_eq!(
        output["hidden_system"], 3,
        "count must be in JSON: {output}"
    );
    let advice = output["advice"]
        .as_array()
        .expect("advice array")
        .iter()
        .any(|a| a.as_str().unwrap_or_default().contains("--show-system"));
    assert!(advice, "advice must name the flag: {output}");

    let text = history(&project, &["demo"]);
    text.assert_success();
    assert!(
        text.stdout_contains("3 system messages hidden"),
        "text output must say what was withheld: {}",
        text.stdout_str()
    );
}

/// `-n 2` must return two readable messages, not two rows of which some
/// vanished.
#[test]
fn test_count_applies_to_visible_messages() {
    let project = TestProject::with_name("history-count-visible");
    for i in 0..4 {
        project
            .run_rite_with_env(&["send", "demo", &format!("real {i}")], Some("alice"))
            .assert_success();
        append_system(&project, "demo", "Hook hk-abc fired: vessel spawn ...");
    }

    let output = history_json(&project, &["demo", "-n", "2"]);
    let bodies = bodies(&output);
    assert_eq!(
        bodies,
        vec!["real 2", "real 3"],
        "-n must count readable messages: {bodies:?}"
    );
}

/// `--from system` that returned nothing would be absurd.
#[test]
fn test_from_system_implies_showing_them() {
    let project = TestProject::with_name("history-from-system");
    project
        .run_rite_with_env(&["send", "demo", "real message"], Some("alice"))
        .assert_success();
    append_system(&project, "demo", "Hook hk-abc fired: vessel spawn ...");

    let output = history_json(&project, &["demo", "--from", "system"]);
    assert_eq!(
        bodies(&output).len(),
        1,
        "--from system must return the system messages: {output}"
    );
}

/// A thread is a conversation the caller named; dropping part of it would
/// misrepresent what was said.
#[test]
fn test_thread_shows_system_messages() {
    let project = TestProject::with_name("history-thread-system");
    let out = project.run_rite_with_env(
        &["send", "demo", "parent", "--format", "json"],
        Some("alice"),
    );
    out.assert_success();
    let parent: Value = serde_json::from_str(&out.stdout_str()).expect("json");
    let parent_id = parent["id"].as_str().expect("id").to_string();

    project
        .run_rite_with_env(
            &["send", "demo", "child", "--reply-to", &parent_id],
            Some("bob"),
        )
        .assert_success();
    append_system(&project, "demo", "Hook hk-abc fired: vessel spawn ...");

    let output = history_json(&project, &["--thread", &parent_id]);
    assert_eq!(output["thread"]["size"], 2, "thread intact: {output}");
    assert!(
        output["hidden_system"].is_null(),
        "a thread hides nothing, so no count: {output}"
    );
}

/// Claim bookkeeping is not "system" for this purpose. `#claims` is 34k such
/// records; hiding them would open that channel empty.
#[test]
fn test_claim_records_are_not_hidden() {
    let project = TestProject::with_name("history-claims-visible");
    project
        .run_rite_with_env(&["claims", "stake", "src/**"], Some("alice"))
        .assert_success();

    let output = history_json(&project, &["claims"]);
    assert!(
        !bodies(&output).is_empty(),
        "claim records must stay visible in #claims: {output}"
    );
    assert!(
        output["hidden_system"].is_null(),
        "no system messages were involved: {output}"
    );
}
