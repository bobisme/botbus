//! An old writer must not silently delete a field it never heard of (bn-14o5).
//!
//! `hooks.jsonl` is append-only with latest-record-per-id wins, and a firing
//! rewrites the *whole* hook record to bump `last_fired`. A build that
//! predates a field therefore deserializes the hook short and appends a copy
//! without it — the configuration is deleted with no error and no warning.
//! `lease` was the field that made this visible, but every field on `Hook` has
//! always been exposed.
//!
//! The fix is a `serde(flatten)` catch-all: anything the build cannot
//! interpret is kept and written straight back out, so a rewrite can only
//! clobber fields the writer actually owns. These tests pin the three
//! properties that matters:
//!
//! 1. an unknown field survives a real fire-and-rewrite cycle,
//! 2. a lease this build *does* know survives one too, and
//! 3. a plain cooldown hook still serializes byte-for-byte as it did before,
//!    so nothing about the 42 live hooks changes.
//!
//! Every test runs against its own `RITE_DATA_DIR` (see `common::TestProject`)
//! and spawns nothing but `sh`.

mod common;

use common::TestProject;
use rite::core::hook::Hook;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn hooks_file(project: &TestProject) -> PathBuf {
    project.data_path().join("hooks.jsonl")
}

/// Every raw line of `hooks.jsonl`, untouched.
fn hook_lines(project: &TestProject) -> Vec<String> {
    std::fs::read_to_string(hooks_file(project))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// The newest raw line for `hook_id` — what rite would read as the hook.
fn latest_line(project: &TestProject, hook_id: &str) -> String {
    hook_lines(project)
        .into_iter()
        .filter(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .is_some_and(|v| v["id"] == hook_id)
        })
        .next_back()
        .unwrap_or_else(|| panic!("no record for {hook_id} in hooks.jsonl"))
}

fn latest_record(project: &TestProject, hook_id: &str) -> Value {
    serde_json::from_str(&latest_line(project, hook_id)).expect("hook record must be valid JSON")
}

/// Block until a firing has appended a fresh record for `hook_id`.
///
/// The `last_fired` bump happens in the sending process, but polling keeps
/// the test honest about ordering rather than assuming it.
fn wait_for_rewrite(project: &TestProject, hook_id: &str, was: usize) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count = hook_lines(project)
            .iter()
            .filter(|l| {
                serde_json::from_str::<Value>(l)
                    .ok()
                    .is_some_and(|v| v["id"] == hook_id)
            })
            .count();
        if count > was {
            return latest_record(project, hook_id);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a rewritten record for {hook_id}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Add a mention hook and return its ID. `extra` carries flags such as
/// `--lease`.
fn add_mention_hook(project: &TestProject, extra: &[&str]) -> String {
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

/// Rewrite the hook's newest record with extra top-level and nested keys, as
/// a *newer* rite that had grown those fields would have written it.
fn plant_unknown_fields(project: &TestProject, hook_id: &str) {
    let mut record = latest_record(project, hook_id);
    record["steer_mode"] = Value::String("inject".to_string());
    record["future_limits"] = serde_json::json!({"max_depth": 3, "tags": ["a", "b"]});
    record["lease"] = serde_json::json!({"ttl_secs": 900, "restart_policy": "never"});

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(hooks_file(project))
        .expect("failed to open hooks.jsonl");
    use std::io::Write;
    writeln!(file, "{}", serde_json::to_string(&record).unwrap()).expect("failed to append");
}

/// A field this build has never heard of must come back out of a firing
/// exactly as it went in. This is the whole bug: without the catch-all the
/// rewrite drops it and the configuration is gone.
#[test]
fn test_unknown_field_survives_a_fire_and_rewrite() {
    let mut project = TestProject::with_name("hook-unknown-field");
    let hook_id = add_mention_hook(&project, &[]);
    plant_unknown_fields(&project, &hook_id);

    let before = hook_lines(&project)
        .iter()
        .filter(|l| serde_json::from_str::<Value>(l).unwrap()["id"] == hook_id.as_str())
        .count();

    let human = project.agent("human");
    human.send("rite", "@worker go").assert_success();

    let after = wait_for_rewrite(&project, &hook_id, before);

    assert!(
        after["last_fired"].is_string(),
        "the firing must still bump last_fired: {after}"
    );
    assert_eq!(
        after["steer_mode"], "inject",
        "an unknown top-level field must survive the rewrite: {after}"
    );
    assert_eq!(
        after["future_limits"],
        serde_json::json!({"max_depth": 3, "tags": ["a", "b"]}),
        "a structured unknown field must survive verbatim: {after}"
    );
    assert_eq!(
        after["lease"]["restart_policy"], "never",
        "an unknown field nested inside a known one must survive too: {after}"
    );
    assert_eq!(
        after["lease"]["ttl_secs"], 900,
        "the known part of the lease must be untouched: {after}"
    );
}

/// The field that exposed the bug, with a build that does know it.
#[test]
fn test_lease_survives_a_fire_by_this_build() {
    let mut project = TestProject::with_name("hook-lease-survives-fire");
    let hook_id = add_mention_hook(
        &project,
        &["--lease", "--lease-ttl", "900", "--max-batch", "7"],
    );

    let before = hook_lines(&project).len();
    let human = project.agent("human");
    human.send("rite", "@worker go").assert_success();

    let after = wait_for_rewrite(&project, &hook_id, before);
    assert_eq!(
        after["lease"],
        serde_json::json!({"ttl_secs": 900, "max_batch": 7}),
        "a lease-enabled hook must keep its lease across a firing: {after}"
    );
}

/// A hook with no lease and nothing unknown must come out of a firing
/// byte-for-byte as it went in, with `last_fired` as the single change. This
/// is the guarantee the 42 live hooks rely on.
///
/// Stated through the type rather than by editing the text: take the record
/// rite wrote at creation, set `last_fired` on it and nothing else, and the
/// bytes must be the line rite wrote when it fired. That survives a key
/// reordering or a value containing `,"`, which a string scan would not.
#[test]
fn test_cooldown_only_hook_is_unchanged_after_a_fire_except_last_fired() {
    let mut project = TestProject::with_name("hook-cooldown-identical");
    let hook_id = add_mention_hook(&project, &["--cooldown", "0s"]);

    let created_line = latest_line(&project, &hook_id);
    let created: Hook = serde_json::from_str(&created_line).expect("hooks add wrote a valid hook");
    assert!(
        created.last_fired.is_none(),
        "a hook that has never fired must not carry last_fired: {created_line}"
    );
    assert!(
        created.extra.is_empty(),
        "a hook rite itself wrote has nothing unknown in it: {created_line}"
    );
    assert!(
        !created_line.contains("\"extra\""),
        "the catch-all must never appear as a key of its own: {created_line}"
    );

    let before = hook_lines(&project).len();
    let human = project.agent("human");
    human.send("rite", "@worker go").assert_success();
    wait_for_rewrite(&project, &hook_id, before);

    let fired_line = latest_line(&project, &hook_id);
    let fired: Hook = serde_json::from_str(&fired_line).expect("the firing wrote a valid hook");
    assert!(
        fired.last_fired.is_some(),
        "the firing must set last_fired: {fired_line}"
    );

    let mut expected = created;
    expected.last_fired = fired.last_fired;
    assert_eq!(
        serde_json::to_string(&expected).unwrap(),
        fired_line,
        "a firing must change nothing but last_fired"
    );
}

/// The checked-in compatibility corpus: `tests/fixtures/hooks_compat.jsonl`.
///
/// Generated from this machine's real `hooks.jsonl` (5697 records, 37 distinct
/// key sets) by keeping one representative record per key set plus extras for
/// every value dimension that affects a round-trip — both timestamp
/// precisions, `active` true and false, `priority` absent/0/1, both condition
/// types, both `claim_release` types, and the full spread of command arity.
/// Identities are redacted: working directories, agent names, channel names,
/// descriptions and command arguments are stable synthetic stand-ins, and only
/// the structure, arity and JSON types are real.
///
/// The last two records are synthetic rather than historical: a lease-bearing
/// hook, and a record as a *newer* rite would write it, carrying fields this
/// build has never heard of.
fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks_compat.jsonl")
}

/// Assert every record in `source` parses as a `Hook` and re-serializes with
/// nothing lost. Works on a *copy* in a temp directory, so a real data
/// directory passed via `RITE_REAL_HOOKS_FILE` is only ever read.
fn assert_round_trips(source: &Path) -> usize {
    let temp = tempfile::tempdir().expect("failed to create temp dir");
    let copy = temp.path().join("hooks.jsonl");
    std::fs::copy(source, &copy)
        .unwrap_or_else(|e| panic!("failed to copy {}: {e}", source.display()));

    let content = std::fs::read_to_string(&copy).expect("failed to read the copy");
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        !lines.is_empty(),
        "{} has no records to check",
        source.display()
    );

    for (n, line) in lines.iter().enumerate() {
        let original: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {} is not JSON: {e}", n + 1));
        let hook: Hook = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {} does not parse as a Hook: {e}", n + 1));
        let reserialized: Value = serde_json::from_str(&serde_json::to_string(&hook).unwrap())
            .expect("a serialized Hook must be JSON");

        // Nothing may be dropped or altered. A rewrite may *add* a key —
        // `priority` gains its default on a record older than that field —
        // which is the long-standing behaviour and loses nothing.
        for (key, value) in original.as_object().expect("a hook record is an object") {
            assert_eq!(
                reserialized.get(key),
                Some(value),
                "line {} of {} lost or changed {key:?} on round-trip",
                n + 1,
                source.display()
            );
        }
    }
    lines.len()
}

/// Every shape of hook record rite has ever written must survive a
/// round-trip. Runs against the checked-in corpus everywhere, including CI.
#[test]
fn test_hook_records_round_trip_without_loss() {
    let fixture = fixture_path();
    let count = assert_round_trips(&fixture);
    assert!(
        count >= 40,
        "the compat corpus has shrunk to {count} records — it is meant to cover \
         every historical hook shape, so check what was dropped"
    );

    // Guard the properties the corpus exists to carry, so a regenerated or
    // hand-edited fixture cannot quietly stop testing them.
    let content = std::fs::read_to_string(&fixture).unwrap();
    let records: Vec<Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let has = |f: &dyn Fn(&Value) -> bool| records.iter().any(|r| f(r));
    assert!(
        has(&|r| r.get("priority").is_none()),
        "the corpus must keep records that predate `priority`"
    );
    assert!(
        has(&|r| r.get("last_fired").is_none()),
        "the corpus must keep records that have never fired"
    );
    assert!(
        has(&|r| r["active"] == false),
        "the corpus must keep a deactivated hook"
    );
    assert!(
        has(&|r| r["condition"]["type"] == "mention_received"),
        "the corpus must keep a mention hook"
    );
    assert!(
        has(&|r| r["claim_release"]["type"] == "on_exit"),
        "the corpus must keep an on-exit claim release"
    );
    assert!(
        has(&|r| r["created_at"].as_str().unwrap().len() != 30),
        "the corpus must keep both timestamp precisions"
    );
    assert!(
        has(&|r| r.get("steer_mode").is_some()),
        "the corpus must keep a record written by a newer rite"
    );

    let unique_key_sets: std::collections::HashSet<Vec<&String>> = records
        .iter()
        .map(|r| r.as_object().unwrap().keys().collect())
        .collect();
    assert!(
        unique_key_sets.len() >= 35,
        "the corpus covers only {} distinct key sets",
        unique_key_sets.len()
    );
}

/// Local check against a real `hooks.jsonl`, which has far more records than
/// the corpus can carry. Set `RITE_REAL_HOOKS_FILE` to run it; the file is
/// copied before it is read and is never written or fired.
#[test]
fn test_real_hooks_file_round_trips_when_provided() {
    let Ok(explicit) = std::env::var("RITE_REAL_HOOKS_FILE") else {
        return;
    };
    let path = PathBuf::from(explicit);
    assert!(
        path.exists(),
        "RITE_REAL_HOOKS_FILE points at a file that does not exist: {}",
        path.display()
    );
    let count = assert_round_trips(&path);
    eprintln!("round-tripped {count} records from {}", path.display());
}
