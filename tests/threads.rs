//! Reply anchoring: `rite send --reply-to` and `rite history --thread`.
//!
//! Every test here runs against an isolated `RITE_DATA_DIR` created by the
//! harness, so nothing touches a real channel or fires a real hook.

mod common;

use common::TestProject;
use std::io::Write;

/// Send a message and return its ULID, taken from the machine-readable output.
fn send(project: &mut TestProject, agent: &str, channel: &str, body: &str) -> String {
    let out = project
        .agent(agent)
        .run(&["send", channel, body, "--format", "json"]);
    out.assert_success();
    id_of(&out.stdout_str())
}

/// Send a reply and return its ULID.
fn reply(
    project: &mut TestProject,
    agent: &str,
    channel: &str,
    body: &str,
    parent: &str,
) -> String {
    let out = project.agent(agent).run(&[
        "send",
        channel,
        body,
        "--reply-to",
        parent,
        "--format",
        "json",
    ]);
    out.assert_success();
    id_of(&out.stdout_str())
}

fn id_of(stdout: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("send must emit JSON: {e}\n{stdout}"));
    value
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("send JSON must carry an id: {stdout}"))
        .to_string()
}

fn thread_json(project: &mut TestProject, agent: &str, args: &[&str]) -> serde_json::Value {
    let out = project.agent(agent).run(args);
    out.assert_success();
    serde_json::from_str(&out.stdout_str()).unwrap_or_else(|e| {
        panic!(
            "history --format json must emit JSON: {e}\n{}",
            out.stdout_str()
        )
    })
}

fn bodies(output: &serde_json::Value) -> Vec<String> {
    output["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|m| m["body"].as_str().unwrap().to_string())
        .collect()
}

/// Append a raw JSONL line, simulating a record synced in from another machine.
fn append_raw(project: &TestProject, channel: &str, line: &str) {
    let path = project
        .data_path()
        .join("channels")
        .join(format!("{}.jsonl", channel));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open channel file");
    writeln!(file, "{}", line).expect("append raw record");
}

fn raw_message(id: &str, body: &str, channel: &str, reply_to: Option<&str>) -> String {
    let anchor = match reply_to {
        Some(parent) => format!(r#","reply_to":"{}""#, parent),
        None => String::new(),
    };
    format!(
        r#"{{"ts":"2026-01-01T00:00:00Z","id":"{}","agent":"remote","channel":"{}","body":"{}"{}}}"#,
        id, channel, body, anchor
    )
}

// --- the happy path ---------------------------------------------------------

#[test]
fn send_reports_the_new_message_id() {
    let mut project = TestProject::new();

    let out = project
        .agent("alice")
        .run(&["send", "general", "hello", "--format", "json"]);
    out.assert_success();

    let value: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    let id = value["id"].as_str().expect("id field");
    assert_eq!(value["channel"], "general");
    assert_eq!(value["agent"], "alice");
    assert!(value.get("reply_to").is_none(), "a root has no anchor");

    // The id names the record that was actually written.
    let written = project.channel_messages("general");
    assert_eq!(written.len(), 1);
    assert_eq!(written[0]["id"].as_str().unwrap(), id);

    // TOON output carries the id too, on its own line.
    let text = project
        .agent("alice")
        .run(&["send", "general", "again", "--format", "text"]);
    text.assert_success();
    let first_line = text.stdout_str().lines().next().unwrap().to_string();
    assert!(first_line.starts_with("id: "), "got: {}", first_line);
}

#[test]
fn reply_round_trips_through_send_and_history() {
    let mut project = TestProject::new();

    let question = send(&mut project, "alice", "general", "who owns review 42?");
    let answer = reply(&mut project, "bob", "general", "I do", &question);

    // The anchor is on the record.
    let written = project.channel_messages("general");
    assert_eq!(written[1]["reply_to"].as_str().unwrap(), question);

    // And the thread comes back whole, from either end. `kind` describes the
    // walk from the anchor: the question is already the root, the answer had
    // to climb to it.
    for (anchor, kind) in [(&question, "root"), (&answer, "resolved")] {
        let output = thread_json(
            &mut project,
            "alice",
            &["history", "general", "--thread", anchor, "--format", "json"],
        );
        let thread = &output["thread"];
        assert_eq!(thread["root"].as_str().unwrap(), question);
        assert_eq!(thread["kind"], kind);
        assert_eq!(thread["complete"], true);
        assert_eq!(thread["size"], 2);
        assert_eq!(thread["depths"], serde_json::json!([0, 1]));
        assert_eq!(bodies(&output), vec!["who owns review 42?", "I do"]);
    }
}

#[test]
fn thread_finds_the_channel_from_the_id_alone() {
    let mut project = TestProject::new();

    let question = send(&mut project, "alice", "backend", "deploy stuck?");
    reply(&mut project, "bob", "backend", "restarting", &question);

    // No channel argument: the default is #general, which does not hold it.
    let output = thread_json(
        &mut project,
        "alice",
        &["history", "--thread", &question, "--format", "json"],
    );
    assert_eq!(output["thread"]["channel"], "backend");
    assert_eq!(output["thread"]["size"], 2);
}

#[test]
fn thread_returns_messages_in_creation_order() {
    let mut project = TestProject::new();

    let root = send(&mut project, "alice", "general", "one");
    let second = reply(&mut project, "bob", "general", "two", &root);
    let third = reply(&mut project, "carol", "general", "three", &second);
    let fourth = reply(&mut project, "dave", "general", "four", &root);

    let output = thread_json(
        &mut project,
        "alice",
        &["history", "general", "--thread", &third, "--format", "json"],
    );

    assert_eq!(bodies(&output), vec!["one", "two", "three", "four"]);
    // Depth follows the anchors, not the order: "four" answers the root.
    assert_eq!(output["thread"]["depths"], serde_json::json!([0, 1, 2, 1]));

    let ids: Vec<String> = output["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![root, second, third, fourth]);
}

// --- the untouched default path --------------------------------------------

#[test]
fn a_channel_without_replies_is_byte_identical_to_before() {
    let mut project = TestProject::new();

    send(&mut project, "alice", "general", "one");
    send(&mut project, "bob", "general", "two");

    // No anchor key anywhere on the wire.
    let raw = std::fs::read_to_string(project.data_path().join("channels/general.jsonl")).unwrap();
    assert!(!raw.contains("reply_to"), "{}", raw);

    // And no thread key in a plain read, so existing parsers see what they saw.
    let output = thread_json(
        &mut project,
        "alice",
        &["history", "general", "--format", "json"],
    );
    assert!(output.get("thread").is_none());
    assert_eq!(bodies(&output), vec!["one", "two"]);
}

#[test]
fn replies_appear_in_flat_history_exactly_like_any_other_message() {
    let mut project = TestProject::new();

    let root = send(&mut project, "alice", "general", "one");
    reply(&mut project, "bob", "general", "two", &root);
    send(&mut project, "carol", "general", "three");

    let output = thread_json(
        &mut project,
        "alice",
        &["history", "general", "--format", "json"],
    );
    assert!(output.get("thread").is_none());
    assert_eq!(bodies(&output), vec!["one", "two", "three"]);
}

// --- degradation ------------------------------------------------------------

#[test]
fn a_reply_to_an_unsynced_parent_is_accepted_with_a_warning() {
    let mut project = TestProject::new();

    // A parent that exists on some other machine but has not arrived here.
    let absent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let out = project.agent("bob").run(&[
        "send",
        "general",
        "answering something I cannot see",
        "--reply-to",
        absent,
        "--format",
        "json",
    ]);
    out.assert_success();

    let value: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(value["reply_to"].as_str().unwrap(), absent);
    let warnings = value["warnings"].as_array().expect("warnings array");
    assert_eq!(warnings.len(), 1, "{:?}", warnings);
    assert!(warnings[0].as_str().unwrap().contains(absent));

    // The thread is returned as a fragment, and says so.
    let orphan = value["id"].as_str().unwrap();
    let output = thread_json(
        &mut project,
        "bob",
        &["history", "general", "--thread", orphan, "--format", "json"],
    );
    let thread = &output["thread"];
    assert_eq!(thread["kind"], "missing_parent");
    assert_eq!(thread["complete"], false);
    assert_eq!(thread["missing_parent"].as_str().unwrap(), absent);
    assert_eq!(thread["root"].as_str().unwrap(), orphan);
    assert_eq!(thread["size"], 1);
}

#[test]
fn a_dangling_child_is_never_shown_as_a_plain_root() {
    let mut project = TestProject::new();
    send(&mut project, "alice", "general", "seed");

    let absent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let orphan = "01ARZ3NDEKTSV4RRFFQ69G5FBW";
    append_raw(
        &project,
        "general",
        &raw_message(orphan, "answer to nothing", "general", Some(absent)),
    );

    let out = project.agent("alice").run(&[
        "history", "general", "--thread", orphan, "--format", "pretty",
    ]);
    out.assert_success();
    let text = out.stdout_str();
    assert!(text.contains("fragment"), "got: {}", text);
    assert!(
        text.contains(absent),
        "the missing parent must be named: {}",
        text
    );
}

#[test]
fn a_tombstoned_parent_degrades_like_a_missing_one() {
    let mut project = TestProject::new();

    let question = send(&mut project, "alice", "general", "who owns review 42?");
    let answer = reply(&mut project, "bob", "general", "I do", &question);

    project
        .agent("alice")
        .run(&["messages", "delete", &question, "-y"])
        .assert_success();

    // The reply survives the deletion of what it answered…
    let flat = thread_json(
        &mut project,
        "alice",
        &["history", "general", "--format", "json"],
    );
    assert_eq!(bodies(&flat), vec!["I do"]);

    // …and its thread reports the hole instead of pretending to be whole.
    let output = thread_json(
        &mut project,
        "alice",
        &[
            "history", "general", "--thread", &answer, "--format", "json",
        ],
    );
    let thread = &output["thread"];
    assert_eq!(thread["kind"], "missing_parent");
    assert_eq!(thread["complete"], false);
    assert_eq!(thread["missing_parent"].as_str().unwrap(), question);
    assert_eq!(thread["size"], 1);

    // Asking for the deleted message itself is an error, not a crash.
    let gone = project.agent("alice").run(&[
        "history", "general", "--thread", &question, "--format", "json",
    ]);
    gone.assert_failure();
}

#[test]
fn a_self_referencing_anchor_does_not_hang() {
    let mut project = TestProject::new();
    send(&mut project, "alice", "general", "seed");

    let looped = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    append_raw(
        &project,
        "general",
        &raw_message(looped, "answering myself", "general", Some(looped)),
    );

    let output = thread_json(
        &mut project,
        "alice",
        &["history", "general", "--thread", looped, "--format", "json"],
    );
    let thread = &output["thread"];
    assert_eq!(thread["kind"], "self_reference");
    assert_eq!(thread["complete"], false);
    assert_eq!(thread["root"].as_str().unwrap(), looped);
    assert_eq!(thread["size"], 1);
}

#[test]
fn a_cycle_synced_from_two_machines_does_not_hang() {
    let mut project = TestProject::new();
    send(&mut project, "alice", "general", "seed");

    // Two machines each anchored their message to the other's.
    let first = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let second = "01ARZ3NDEKTSV4RRFFQ69G5FBW";
    append_raw(
        &project,
        "general",
        &raw_message(first, "from machine one", "general", Some(second)),
    );
    append_raw(
        &project,
        "general",
        &raw_message(second, "from machine two", "general", Some(first)),
    );

    for anchor in [first, second] {
        let output = thread_json(
            &mut project,
            "alice",
            &["history", "general", "--thread", anchor, "--format", "json"],
        );
        let thread = &output["thread"];
        assert_eq!(thread["kind"], "cycle");
        assert_eq!(thread["complete"], false);
        assert_eq!(thread["size"], 2, "both members, counted once each");
    }

    // A plain read is unaffected by the loop.
    let flat = thread_json(
        &mut project,
        "alice",
        &["history", "general", "--format", "json"],
    );
    assert_eq!(flat["messages"].as_array().unwrap().len(), 3);
}

#[test]
fn an_unreadable_anchor_costs_the_thread_not_the_message() {
    let mut project = TestProject::new();
    send(&mut project, "alice", "general", "seed");

    // A shape a future rite might give the field. The record must still read.
    append_raw(
        &project,
        "general",
        r#"{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"future","channel":"general","body":"from the future","reply_to":{"id":"01ARZ3NDEKTSV4RRFFQ69G5FBW"}}"#,
    );

    let read = project
        .agent("alice")
        .run(&["history", "general", "--format", "json"]);
    read.assert_success();
    let output: serde_json::Value = serde_json::from_str(&read.stdout_str()).unwrap();
    assert_eq!(bodies(&output), vec!["seed", "from the future"]);

    // The loss is announced where a human will see it, once.
    let stderr = read.stderr_str();
    assert!(
        stderr.contains("unreadable field value"),
        "the read must say it dropped something: {stderr}"
    );
    assert!(stderr.contains("reply_to"), "{stderr}");
    assert!(stderr.contains("rite doctor"), "{stderr}");
    assert_eq!(
        stderr.matches("unreadable field value").count(),
        1,
        "one note per file per process, not one per read: {stderr}"
    );

    // It reads as top-level, which is where it would have been before
    // threading existed.
    let thread = thread_json(
        &mut project,
        "alice",
        &[
            "history",
            "general",
            "--thread",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--format",
            "json",
        ],
    );
    assert_eq!(thread["thread"]["kind"], "root");
    assert_eq!(thread["thread"]["size"], 1);
}

/// `rite doctor` is where an operator goes to find out what this build cannot
/// read. A dropped anchor belongs there next to the skipped-line count, not
/// only in a stderr line that scrolled past.
#[test]
fn doctor_counts_dropped_reply_anchors() {
    let mut project = TestProject::new();
    let root = send(&mut project, "alice", "general", "seed");
    reply(&mut project, "bob", "general", "a good reply", &root);

    // Clean to start with.
    let clean = project.agent("alice").run(&["doctor", "--format", "json"]);
    let report: serde_json::Value = serde_json::from_str(&clean.stdout_str())
        .unwrap_or_else(|e| panic!("doctor must emit JSON: {e}\n{}", clean.stdout_str()));
    assert_eq!(report["damaged_field_count"], 0);
    assert_eq!(report["skipped_line_count"], 0);

    append_raw(
        &project,
        "general",
        r#"{"ts":"2026-01-01T00:00:00Z","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"mangled","channel":"general","body":"bad anchor","reply_to":"????"}"#,
    );

    let after = project.agent("alice").run(&["doctor", "--format", "json"]);
    let report: serde_json::Value = serde_json::from_str(&after.stdout_str()).unwrap();

    assert_eq!(
        report["skipped_line_count"], 0,
        "the record is readable and must be kept"
    );
    assert_eq!(report["damaged_field_count"], 1);

    let damaged = report["damaged_fields"].as_array().expect("damaged_fields");
    assert_eq!(damaged.len(), 1);
    assert_eq!(damaged[0]["field"], "reply_to");
    assert!(
        damaged[0]["file"]
            .as_str()
            .unwrap()
            .ends_with("general.jsonl"),
        "{:?}",
        damaged[0]
    );
    assert!(damaged[0]["value"].as_str().unwrap().contains("????"));

    // And the check itself warns rather than passing.
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "record_readability")
        .expect("record_readability check");
    assert_eq!(check["status"], "warn");
    assert!(
        check["message"].as_str().unwrap().contains("reply_to"),
        "{:?}",
        check["message"]
    );
}

// --- argument handling ------------------------------------------------------

#[test]
fn a_malformed_reply_to_is_rejected() {
    let mut project = TestProject::new();

    let out = project
        .agent("alice")
        .run(&["send", "general", "hi", "--reply-to", "not-a-ulid"]);
    out.assert_failure();
    out.assert_stderr_contains("ULID");

    // Nothing was written.
    assert!(project.channel_messages("general").is_empty());
}

#[test]
fn a_thread_id_that_exists_nowhere_is_an_error() {
    let mut project = TestProject::new();
    send(&mut project, "alice", "general", "seed");

    let out = project.agent("alice").run(&[
        "history",
        "general",
        "--thread",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "--format",
        "json",
    ]);
    out.assert_failure();
    out.assert_stderr_contains("not in");
}

#[test]
fn thread_refuses_to_combine_with_the_pagination_flags() {
    let mut project = TestProject::new();
    let root = send(&mut project, "alice", "general", "seed");

    for extra in [
        vec!["--after-offset", "0"],
        vec!["--after-id", "01ARZ3NDEKTSV4RRFFQ69G5FAV"],
        vec!["--follow"],
    ] {
        let mut args = vec!["history", "general", "--thread", &root];
        args.extend(extra.iter());
        project.agent("alice").run(&args).assert_failure();
    }
}
