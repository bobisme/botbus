//! `rite wait --reply-to`: block until one specific message is answered.
//!
//! These drive the real binary against an isolated `RITE_DATA_DIR`, so nothing
//! touches a live channel or fires a real hook.
//!
//! The behaviour under test is a contract a script depends on:
//!
//! | outcome                                   | exit | `reason`         |
//! |-------------------------------------------|------|------------------|
//! | the message was answered                  | 0    | `reply`          |
//! | nobody answered inside `-t`               | 1    | `timeout`        |
//! | the id is not a ULID                      | 2    | `invalid_parent` |
//! | the id is a ULID this store has not seen  | 2    | `unknown_parent` |

mod common;
use common::{TestProject, rite_bin};

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

/// Send a message and return its ULID.
fn send(project: &mut TestProject, agent: &str, channel: &str, body: &str) -> String {
    let out = project
        .agent(agent)
        .run(&["send", channel, body, "--format", "json"]);
    out.assert_success();
    let value: serde_json::Value = serde_json::from_str(&out.stdout_str())
        .unwrap_or_else(|e| panic!("send must emit JSON: {e}\n{}", out.stdout_str()));
    value["id"]
        .as_str()
        .expect("send JSON carries an id")
        .into()
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
    let value: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    value["id"].as_str().expect("id").into()
}

/// Spawn `rite wait --format json` in the background.
fn spawn_wait(data_path: &Path, work_dir: &Path, agent: &str, extra: &[&str]) -> Child {
    let mut cmd = Command::new(rite_bin());
    cmd.current_dir(work_dir);
    cmd.env("RITE_DATA_DIR", data_path);
    cmd.env_remove("RITE_AGENT");
    cmd.env_remove("AGENT");
    cmd.args(["wait", "--agent", agent, "--format", "json"]);
    cmd.args(extra);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("failed to spawn `rite wait`")
}

struct Finished {
    code: Option<i32>,
    json: serde_json::Value,
    stderr: String,
}

fn finish(child: Child) -> Finished {
    let output = child.wait_with_output().expect("wait did not exit");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let json = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("wait --format json must emit JSON: {e}\n{stdout}\n{stderr}"));
    Finished {
        code: output.status.code(),
        json,
        stderr,
    }
}

/// Give the waiter time to arm its watcher and run its startup query.
fn settle() {
    sleep(Duration::from_millis(1200));
}

// --- the reply arrives while the wait is running ----------------------------

/// The steady-state half: the tail loop sees a reply appended after the wait
/// began.
#[test]
fn a_reply_that_arrives_during_the_wait_ends_it() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");

    let child = spawn_wait(
        &project.data_path,
        &project.work_dir,
        "requester",
        &["--reply-to", &question, "-t", "30"],
    );
    settle();

    reply(&mut project, "reviewer", "rite", "on it", &question);

    let done = finish(child);
    assert_eq!(done.code, Some(0), "answered wait exits 0: {}", done.stderr);
    assert_eq!(done.json["received"], true);
    assert_eq!(done.json["reason"], "reply");
    assert_eq!(done.json["channel"], "rite");
    assert_eq!(done.json["message"]["body"], "on it");
    assert_eq!(done.json["message"]["agent"], "reviewer");
    assert_eq!(
        done.json["message"]["reply_to"], question,
        "the answering message must carry the anchor a caller correlates on"
    );
    assert_eq!(
        done.json["reply_to"], question,
        "the awaited id is echoed on the result"
    );
    assert!(
        done.json["message"]["id"].as_str().is_some(),
        "a caller must be able to reply to the answer: {}",
        done.json
    );
}

// --- the reply arrived before the wait started ------------------------------

/// The startup half. A reviewer that answers in the window between `rite send`
/// returning and `rite wait` starting must not be missed — that gap is exactly
/// when a hook-spawned reviewer answers, and missing it produces the
/// full-length timeout that makes a requester re-post.
#[test]
fn a_reply_that_landed_before_the_wait_is_still_reported() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");
    reply(
        &mut project,
        "reviewer",
        "rite",
        "already answered",
        &question,
    );

    // A short timeout: if the startup half were missing, the tail loop would
    // never see this reply and the test would fail on the exit code instead of
    // hanging.
    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        &question,
        "-t",
        "5",
        "--format",
        "json",
    ]);

    assert!(
        out.success(),
        "a reply already on disk must satisfy the wait: {}\n{}",
        out.stdout_str(),
        out.stderr_str()
    );
    let json: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(json["received"], true);
    assert_eq!(json["reason"], "reply");
    assert_eq!(json["message"]["body"], "already answered");
}

/// The startup half reads the index, and the index is only synced lazily. A
/// wait must not depend on someone having run `rite search` or
/// `rite index rebuild` first.
#[test]
fn the_startup_half_works_on_a_never_synced_index() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");
    reply(&mut project, "reviewer", "rite", "answered", &question);

    // No `rite search`, no `rite index rebuild` — nothing has ever populated
    // the index in this store.
    assert!(
        !project.data_path().join("index.sqlite").exists(),
        "precondition: the index has not been built yet"
    );

    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        &question,
        "-t",
        "5",
        "--format",
        "json",
    ]);
    assert!(out.success(), "{}\n{}", out.stdout_str(), out.stderr_str());
    let json: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(json["message"]["body"], "answered");
}

/// And with the index already populated, which is the normal production state
/// once anything has run `rite search`. This is the path the startup half is
/// designed for: `SearchIndex::replies_to` answers, and the channel file is
/// opened only to read the record the edge points at.
#[test]
fn the_startup_half_works_on_a_populated_index() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");
    reply(&mut project, "reviewer", "rite", "answered", &question);

    project
        .agent("requester")
        .run(&["index", "rebuild"])
        .assert_success();
    assert!(
        project.data_path().join("index.sqlite").exists(),
        "precondition: the index holds the reply edge"
    );

    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        &question,
        "-t",
        "5",
        "--format",
        "json",
    ]);
    assert!(out.success(), "{}\n{}", out.stdout_str(), out.stderr_str());
    let json: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(json["message"]["body"], "answered");
    assert_eq!(json["reason"], "reply");
}

/// The two halves must not double-count. A reply present before the wait is
/// reported exactly once — one JSON object on stdout, not two.
#[test]
fn an_existing_reply_is_reported_exactly_once() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");
    reply(&mut project, "reviewer", "rite", "answered", &question);

    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        &question,
        "-t",
        "5",
        "--format",
        "json",
    ]);
    out.assert_success();

    let stdout = out.stdout_str();
    assert_eq!(
        stdout.matches("\"received\"").count(),
        1,
        "exactly one result must be emitted:\n{stdout}"
    );
}

// --- the wait must not be satisfied by the wrong message --------------------

/// A reply to a *different* question is not an acknowledgment of this one.
/// A false ack is worse than a timeout: the requester proceeds on an answer it
/// never received.
#[test]
fn a_reply_to_another_message_does_not_end_the_wait() {
    let mut project = TestProject::new();
    let mine = send(&mut project, "requester", "rite", "Review rv-12?");
    let theirs = send(&mut project, "other", "rite", "Review rv-13?");

    let child = spawn_wait(
        &project.data_path,
        &project.work_dir,
        "requester",
        &["--reply-to", &mine, "-t", "8"],
    );
    settle();

    // Noise that must not satisfy the wait: an answer to another question, and
    // a top-level message in the same channel.
    reply(&mut project, "reviewer", "rite", "on rv-13", &theirs);
    project
        .agent("reviewer")
        .send("rite", "unrelated chatter")
        .assert_success();

    let done = finish(child);
    assert_eq!(
        done.code,
        Some(1),
        "only a reply to the named message counts: {}",
        done.json
    );
    assert_eq!(done.json["reason"], "timeout");
    assert_eq!(done.json["received"], false);
}

/// `--from` narrows `--reply-to` rather than widening it: both must hold.
#[test]
fn from_narrows_reply_to() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");

    let child = spawn_wait(
        &project.data_path,
        &project.work_dir,
        "requester",
        &["--reply-to", &question, "--from", "reviewer", "-t", "30"],
    );
    settle();

    // Right anchor, wrong author: not the acknowledgment that was asked for.
    reply(&mut project, "passer-by", "rite", "nice one", &question);
    sleep(Duration::from_millis(600));
    reply(&mut project, "reviewer", "rite", "on it", &question);

    let done = finish(child);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(done.json["message"]["agent"], "reviewer");
    assert_eq!(done.json["message"]["body"], "on it");
}

/// `--from` also excludes on its own: no matching author, no match.
#[test]
fn from_can_exclude_every_reply() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");
    reply(&mut project, "passer-by", "rite", "nice one", &question);

    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        &question,
        "--from",
        "reviewer",
        "-t",
        "3",
        "--format",
        "json",
    ]);

    assert_eq!(out.status.code(), Some(1), "{}", out.stdout_str());
    let json: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(json["reason"], "timeout");
}

// --- timeout ----------------------------------------------------------------

/// No reply: exit 1, `reason: timeout`, and advice that names the thing an
/// agent must not do — send the request again.
#[test]
fn no_reply_times_out_with_exit_1() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");

    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        &question,
        "-t",
        "3",
        "--format",
        "json",
    ]);

    assert_eq!(out.status.code(), Some(1), "timeout exits 1");
    let json: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(json["received"], false);
    assert_eq!(json["reason"], "timeout");
    assert_eq!(
        json["reply_to"], question,
        "a timeout must still say which request went unanswered"
    );
    assert!(
        json["advice"].as_array().is_some_and(|a| !a.is_empty()),
        "a timeout carries advice: {json}"
    );
}

// --- an id that cannot be waited on -----------------------------------------

/// A ULID this store has never seen cannot ever be answered. Blocking on it
/// would burn the whole timeout and then report the one thing that is false —
/// that nobody answered — which is how a mistyped id becomes a re-post.
#[test]
fn an_unknown_parent_fails_fast_with_exit_2() {
    let mut project = TestProject::new();
    project
        .agent("requester")
        .send("rite", "some history")
        .assert_success();

    let absent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let started = std::time::Instant::now();
    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        absent,
        "-t",
        "60",
        "--format",
        "json",
    ]);

    assert_eq!(
        out.status.code(),
        Some(2),
        "an unknown id is not a timeout: {}\n{}",
        out.stdout_str(),
        out.stderr_str()
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "it must not wait out the timeout"
    );

    let json: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(json["received"], false);
    assert_eq!(json["reason"], "unknown_parent");
    assert_eq!(json["reply_to"], absent);
    assert!(
        json["advice"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n.as_str().unwrap_or("").contains(absent))),
        "the advice must name the id: {json}"
    );
}

/// An id that is not a ULID at all fails the same way, and with the same exit
/// code: the caller's problem is identical.
#[test]
fn a_malformed_parent_fails_fast_with_exit_2() {
    let mut project = TestProject::new();

    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        "not-a-ulid",
        "-t",
        "60",
        "--format",
        "json",
    ]);

    assert_eq!(out.status.code(), Some(2), "{}", out.stdout_str());
    let json: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(json["reason"], "invalid_parent");
}

/// The escape hatch: a parent still syncing in from another machine. The wait
/// runs, and times out normally rather than refusing.
#[test]
fn allow_missing_parent_waits_anyway() {
    let mut project = TestProject::new();

    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "--allow-missing-parent",
        "-t",
        "3",
        "--format",
        "json",
    ]);

    assert_eq!(
        out.status.code(),
        Some(1),
        "with the flag it waits and then times out: {}",
        out.stdout_str()
    );
    let json: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(json["reason"], "timeout");
}

/// A reply that arrives for a parent this store has not seen still satisfies
/// the wait. The anchor is the correlation id; the parent record is not needed
/// to recognise the answer.
#[test]
fn allow_missing_parent_still_matches_the_reply() {
    let mut project = TestProject::new();
    let absent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    let child = spawn_wait(
        &project.data_path,
        &project.work_dir,
        "requester",
        &["--reply-to", absent, "--allow-missing-parent", "-t", "30"],
    );
    settle();

    reply(&mut project, "reviewer", "rite", "on it", absent);

    let done = finish(child);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(done.json["reason"], "reply");
    assert_eq!(done.json["message"]["body"], "on it");
}

// --- own messages -----------------------------------------------------------

/// Answering yourself is not an acknowledgment. `wait` already skips the
/// caller's own messages, and `--reply-to` keeps that rule.
#[test]
fn your_own_reply_does_not_acknowledge_you() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");
    reply(&mut project, "requester", "rite", "bump", &question);

    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        &question,
        "-t",
        "3",
        "--format",
        "json",
    ]);

    assert_eq!(
        out.status.code(),
        Some(1),
        "a self-reply is not an ack: {}",
        out.stdout_str()
    );
}

// --- channels ---------------------------------------------------------------

/// A reply in a DM satisfies a `--reply-to` wait: with no `-c`, every channel
/// is watched, exactly as `--mentions` does.
#[test]
fn a_reply_in_another_channel_still_counts() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");

    let child = spawn_wait(
        &project.data_path,
        &project.work_dir,
        "requester",
        &["--reply-to", &question, "-t", "30"],
    );
    settle();

    reply(&mut project, "reviewer", "@requester", "on it", &question);

    let done = finish(child);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(done.json["message"]["body"], "on it");
}

/// `-c` narrows `--reply-to` to the named channels.
#[test]
fn channels_narrow_reply_to() {
    let mut project = TestProject::new();
    let question = send(&mut project, "requester", "rite", "Review rv-12?");
    // The answer exists, but not in the channel the caller restricted to.
    reply(&mut project, "reviewer", "general", "on it", &question);

    let out = project.agent("requester").run(&[
        "wait",
        "--reply-to",
        &question,
        "-c",
        "rite",
        "-t",
        "3",
        "--format",
        "json",
    ]);

    assert_eq!(
        out.status.code(),
        Some(1),
        "a reply outside -c must not match: {}",
        out.stdout_str()
    );
}

// --- no regression for the flagless command ---------------------------------

/// Without `--reply-to` the command behaves exactly as before.
#[test]
fn wait_without_reply_to_is_unchanged() {
    let mut project = TestProject::new();

    let child = spawn_wait(
        &project.data_path,
        &project.work_dir,
        "listener",
        &["-c", "rite", "-t", "30"],
    );
    settle();

    project
        .agent("talker")
        .send("rite", "anything at all")
        .assert_success();

    let done = finish(child);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(done.json["reason"], "message");
    assert!(
        done.json.get("reply_to").is_none(),
        "no anchor was awaited, so none is reported: {}",
        done.json
    );
}
