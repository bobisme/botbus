//! Integration tests for `rite mentions follow`.
//!
//! These drive the real binary: a long-lived follower process on one side, and
//! `rite send` from other agents on the other. Each follower is bounded by
//! `--count` (exit as soon as the expected records arrive) and `--timeout` (a
//! hard ceiling so a regression fails instead of hanging).

mod common;
use common::{TestProject, rite_bin};

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

/// Spawn `rite mentions follow --format json` for `agent`.
fn spawn_follow(data_path: &Path, work_dir: &Path, agent: &str, extra: &[&str]) -> Child {
    let mut cmd = Command::new(rite_bin());
    cmd.current_dir(work_dir);
    cmd.env("RITE_DATA_DIR", data_path);
    cmd.env_remove("RITE_AGENT");
    cmd.env_remove("AGENT");
    cmd.args(["mentions", "follow", "--agent", agent, "--format", "json"]);
    cmd.args(extra);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("failed to spawn `rite mentions follow`")
}

/// Wait for the follower to exit and parse its stdout as JSONL.
fn collect(child: Child) -> Vec<serde_json::Value> {
    let output = child.wait_with_output().expect("follower did not exit");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!(
                    "stream line is not valid JSON ({e}): {l}\nstderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })
        })
        .collect()
}

fn body(record: &serde_json::Value) -> &str {
    record["message"]["body"].as_str().unwrap_or_default()
}

/// Give the follower time to register its watcher and seed offsets at "now".
fn settle() {
    sleep(Duration::from_millis(800));
}

/// Keep consecutive sends in separate watcher batches so ordering is stable.
fn spaced() {
    sleep(Duration::from_millis(300));
}

/// The stream picks up mentions from every channel, including channels created
/// after startup; it does not replay history, does not echo the agent's own
/// messages, and matches mentions case-insensitively.
#[test]
fn follow_streams_mentions_across_all_channels() {
    let mut project = TestProject::new();
    let alice = project.agent("alice");
    let me = project.agent("rite-dev");

    // Pre-existing history: must NOT be replayed (channels seed at EOF).
    alice
        .send("general", "ancient history for @rite-dev")
        .assert_success();

    let child = spawn_follow(
        &project.data_path,
        &project.work_dir,
        "rite-dev",
        &["--count", "3", "--timeout", "30"],
    );
    settle();

    // 1. Mention in a channel that existed at startup.
    alice
        .send("general", "please review this @rite-dev")
        .assert_success();
    spaced();

    // 2. First-ever message of a channel created AFTER startup, mentioning the
    //    agent with different casing.
    alice
        .send("brand-new", "opening shot @Rite-Dev")
        .assert_success();
    spaced();

    // Self-authored: dropped even though it mentions the agent.
    me.send("general", "talking to myself @rite-dev")
        .assert_success();
    spaced();

    // No mention: dropped.
    alice.send("general", "unrelated chatter").assert_success();
    spaced();

    // 3. Mention in yet another channel — the count-3 stop condition.
    alice
        .send("side-quest", "wrap up @rite-dev")
        .assert_success();

    let records = collect(child);
    let bodies: Vec<&str> = records.iter().map(body).collect();

    assert_eq!(records.len(), 3, "unexpected stream contents: {bodies:?}");
    assert_eq!(bodies[0], "please review this @rite-dev");
    assert_eq!(bodies[1], "opening shot @Rite-Dev");
    assert_eq!(bodies[2], "wrap up @rite-dev");

    assert_eq!(records[0]["route"], "mention");
    assert_eq!(records[0]["channel"], "general");
    assert_eq!(records[0]["reply_target"], "general");
    assert_eq!(records[1]["channel"], "brand-new");
    assert_eq!(records[2]["channel"], "side-quest");

    let joined = bodies.join("\n");
    assert!(!joined.contains("ancient history"), "history was replayed");
    assert!(
        !joined.contains("talking to myself"),
        "self-authored message was echoed back"
    );
    assert!(!joined.contains("unrelated chatter"));
}

/// DM privacy is absolute: a mention never routes a message out of a DM the
/// agent is not a participant of.
///
/// alice DMs bob and mentions carol. carol follows with the default flags, so
/// DMs are in scope — but not that one. The follower is bounded by `--count 2`
/// so a leak shows up as an early exit with two records; correct behaviour
/// yields exactly one (the sentinel) at timeout.
#[test]
fn follow_never_leaks_a_dm_the_agent_is_not_party_to() {
    let mut project = TestProject::new();
    let alice = project.agent("alice");

    let child = spawn_follow(
        &project.data_path,
        &project.work_dir,
        "carol",
        &["--count", "2", "--timeout", "8"],
    );
    settle();

    // Private between alice and bob. Mentions carol; carol must never see it.
    alice
        .send("@bob", "we should loop in @carol on the rewrite")
        .assert_success();
    spaced();

    // Sentinel: proves the stream was alive the whole time.
    alice.send("general", "@carol ping").assert_success();

    let records = collect(child);
    let bodies: Vec<&str> = records.iter().map(body).collect();

    assert_eq!(
        records.len(),
        1,
        "DM leaked to a non-participant: {bodies:?}"
    );
    assert_eq!(bodies[0], "@carol ping");
    assert_eq!(records[0]["route"], "mention");
    assert_eq!(records[0]["channel"], "general");
    assert!(!bodies[0].contains("loop in"));
}

/// `--no-dms` narrows the stream to mentions in regular channels: a DM the
/// agent *is* party to is withheld, and a mention inside that DM does not
/// smuggle it back in.
#[test]
fn follow_withholds_dms_with_no_dms() {
    let mut project = TestProject::new();
    let alice = project.agent("alice");

    let child = spawn_follow(
        &project.data_path,
        &project.work_dir,
        "bob",
        &["--no-dms", "--count", "2", "--timeout", "8"],
    );
    settle();

    alice.send("@bob", "private word, @bob").assert_success();
    spaced();
    alice.send("general", "@bob over here").assert_success();

    let records = collect(child);
    let bodies: Vec<&str> = records.iter().map(body).collect();

    assert_eq!(records.len(), 1, "DM streamed despite --no-dms: {bodies:?}");
    assert_eq!(bodies[0], "@bob over here");
    assert_eq!(records[0]["route"], "mention");
}

/// By default a DM to a participant arrives tagged `dm`, with a `reply_target`
/// addressed back to the sender — no flag required.
#[test]
fn follow_streams_participant_dms_by_default() {
    let mut project = TestProject::new();
    let alice = project.agent("alice");

    let child = spawn_follow(
        &project.data_path,
        &project.work_dir,
        "bob",
        &["--count", "1", "--timeout", "30"],
    );
    settle();

    alice.send("@bob", "got a minute?").assert_success();

    let records = collect(child);
    assert_eq!(records.len(), 1, "expected the DM to be streamed");
    assert_eq!(records[0]["route"], "dm");
    assert_eq!(records[0]["channel"], "_dm_alice_bob");
    assert_eq!(records[0]["reply_target"], "@alice");
    assert_eq!(body(&records[0]), "got a minute?");
}

/// `-L` narrows the stream to labelled traffic.
#[test]
fn follow_filters_by_label() {
    let mut project = TestProject::new();
    let alice = project.agent("alice");

    let child = spawn_follow(
        &project.data_path,
        &project.work_dir,
        "rite-dev",
        &["-L", "review", "--count", "2", "--timeout", "8"],
    );
    settle();

    alice
        .send_with_labels("general", "@rite-dev chit chat", &["chat"])
        .assert_success();
    spaced();
    alice
        .send_with_labels("general", "@rite-dev take a look", &["review"])
        .assert_success();

    let records = collect(child);
    let bodies: Vec<&str> = records.iter().map(body).collect();

    assert_eq!(
        records.len(),
        1,
        "label filter let extra traffic through: {bodies:?}"
    );
    assert_eq!(bodies[0], "@rite-dev take a look");
}

/// The command is useless without an identity and must say so rather than
/// silently streaming nothing.
#[test]
fn follow_requires_an_agent_identity() {
    let project = TestProject::new();
    let output = project.run_rite(&["mentions", "follow", "--format", "json", "--timeout", "1"]);
    output.assert_failure();
    assert!(
        output.stderr_contains("agent identity"),
        "stderr: {}",
        output.stderr_str()
    );
}
