//! `--name` converges a hook instead of duplicating it (bn-2g15).
//!
//! An external manager like edict has to answer "is my hook already
//! registered, and does it match what I want?". With no stable key it does
//! that by exact-matching the free-text description, and with no upsert it
//! converges by removing every match and adding a fresh one.
//!
//! That is not a safe way to update a hook. The ID is the spawn-lease key
//! (`spawn://<id>/<channel>`), so a new ID means a running spawn holds a
//! lease nobody checks any more. And a recreate rebuilds the record from the
//! caller's template, dropping anything the caller does not set — which is
//! how the bn-20eh canary lost its lease and ran unleased for 19 hours while
//! still looking like the canary.
//!
//! So the rule these tests pin is: a converge preserves everything it was not
//! explicitly told to change.

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

/// The newest record for `hook_id`, having first checked that `hook_id` is
/// still the *only* hook.
///
/// Without that check every "converge preserves X" test passes vacuously if
/// the converge starts issuing new IDs: `latest_record` would keep finding
/// the untouched original while the real hook moved elsewhere. Verified by
/// mutation — an ID-changing converge left three of these tests green until
/// this guard was added.
fn sole_record(project: &TestProject, hook_id: &str) -> Value {
    let hooks = active_hooks(project, &[]);
    assert_eq!(
        hooks.len(),
        1,
        "expected exactly one hook, so the assertions below are about the live one: {hooks:?}"
    );
    assert_eq!(
        hooks[0]["id"], hook_id,
        "the live hook is no longer {hook_id} — the converge replaced it"
    );
    latest_record(project, hook_id)
}

/// Every active hook, as `hooks list` reports it.
fn active_hooks(project: &TestProject, extra: &[&str]) -> Vec<Value> {
    let mut args = vec!["hooks", "list", "--format", "json"];
    args.extend_from_slice(extra);
    let out = project.run_rite_with_env(&args, Some("ops"));
    out.assert_success();
    let parsed: Value = serde_json::from_str(&out.stdout_str()).expect("valid json");
    parsed["hooks"].as_array().cloned().unwrap_or_default()
}

/// Register a hook, returning its ID. `extra` carries the flags under test.
fn add(project: &TestProject, extra: &[&str]) -> String {
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

    let out = project.run_rite_with_env(&args, Some("ops"));
    out.assert_success();

    let line = hook_lines(project)
        .pop()
        .expect("hooks add must write a record");
    serde_json::from_str::<Value>(&line).unwrap()["id"]
        .as_str()
        .expect("hook id")
        .to_string()
}

/// Adding the same name twice converges onto one hook, keeping the ID.
#[test]
fn test_same_name_converges_instead_of_duplicating() {
    let project = TestProject::with_name("hook-name-converge");
    let first = add(&project, &["--name", "edict:rite:responder"]);
    let second = add(&project, &["--name", "edict:rite:responder"]);

    assert_eq!(first, second, "converge must keep the original ID");
    assert_eq!(
        active_hooks(&project, &[]).len(),
        1,
        "converge must not create a second hook"
    );
}

/// The bn-20eh failure, as a test: a converge that says nothing about leasing
/// must not strip the lease.
#[test]
fn test_converge_preserves_lease_when_not_mentioned() {
    let project = TestProject::with_name("hook-name-keeps-lease");
    let id = add(
        &project,
        &[
            "--name",
            "edict:console:responder",
            "--lease",
            "--lease-ttl",
            "1800",
            "--max-batch",
            "7",
        ],
    );

    // A manager that has never heard of --lease re-registers its hook.
    add(&project, &["--name", "edict:console:responder"]);

    let record = sole_record(&project, &id);
    assert_eq!(
        record["lease"]["ttl_secs"], 1800,
        "a converge must not silently drop the lease: {record}"
    );
    assert_eq!(record["lease"]["max_batch"], 7);
}

/// A converge must not reset the cooldown clock either.
#[test]
fn test_converge_preserves_last_fired() {
    let project = TestProject::with_name("hook-name-last-fired");
    let id = add(&project, &["--name", "edict:rite:responder"]);

    let fired_at = "2026-08-12T04:32:25.300037810Z";
    let mut record = latest_record(&project, &id);
    record["last_fired"] = Value::String(fired_at.to_string());
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(hooks_file(&project))
            .expect("open hooks.jsonl");
        writeln!(file, "{}", serde_json::to_string(&record).unwrap()).expect("append");
    }

    add(&project, &["--name", "edict:rite:responder"]);

    assert_eq!(
        sole_record(&project, &id)["last_fired"],
        fired_at,
        "a converge must not reset the cooldown clock"
    );
}

/// Fields this build does not understand must survive a converge, or `--name`
/// becomes a new way to reintroduce bn-14o5.
#[test]
fn test_converge_preserves_unknown_fields() {
    let project = TestProject::with_name("hook-name-unknown");
    let id = add(&project, &["--name", "edict:rite:responder"]);

    let mut record = latest_record(&project, &id);
    record["steer_mode"] = Value::String("inject".to_string());
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(hooks_file(&project))
            .expect("open hooks.jsonl");
        writeln!(file, "{}", serde_json::to_string(&record).unwrap()).expect("append");
    }

    add(&project, &["--name", "edict:rite:responder"]);

    assert_eq!(
        sole_record(&project, &id)["steer_mode"],
        "inject",
        "a converge must preserve fields it does not understand"
    );
}

/// A converge still applies what it *was* told — otherwise it is useless.
#[test]
fn test_converge_applies_what_it_specifies() {
    let project = TestProject::with_name("hook-name-applies");
    let id = add(&project, &["--name", "edict:rite:responder"]);

    let cwd = project.work_dir().to_string_lossy().to_string();
    let out = project.run_rite_with_env(
        &[
            "hooks",
            "add",
            "--channel",
            "rite",
            "--mention",
            "worker",
            "--cwd",
            &cwd,
            "--name",
            "edict:rite:responder",
            "--",
            "sh",
            "-c",
            "echo converged",
        ],
        Some("ops"),
    );
    out.assert_success();

    let record = sole_record(&project, &id);
    assert_eq!(
        record["command"],
        serde_json::json!(["sh", "-c", "echo converged"]),
        "a converge must apply the fields it names"
    );
}

/// Names are scoped per channel: the same name on another channel is a
/// different hook, not a collision.
#[test]
fn test_same_name_on_another_channel_is_a_separate_hook() {
    let project = TestProject::with_name("hook-name-per-channel");
    let cwd = project.work_dir().to_string_lossy().to_string();

    let first = add(&project, &["--name", "responder"]);

    let out = project.run_rite_with_env(
        &[
            "hooks",
            "add",
            "--channel",
            "other",
            "--mention",
            "worker",
            "--cwd",
            &cwd,
            "--name",
            "responder",
            "--",
            "sh",
            "-c",
            "true",
        ],
        Some("ops"),
    );
    out.assert_success();

    let hooks = active_hooks(&project, &[]);
    assert_eq!(hooks.len(), 2, "one hook per channel: {hooks:?}");
    let second = hooks
        .iter()
        .find(|h| h["channel"] == "other")
        .expect("hook on #other");
    assert_ne!(second["id"], first.as_str());
}

/// Hooks with no name keep the old behaviour — every add is a new hook. This
/// is every hook registered before this field existed.
#[test]
fn test_unnamed_hooks_still_create_every_time() {
    let project = TestProject::with_name("hook-name-absent");
    let first = add(&project, &[]);
    let second = add(&project, &[]);

    assert_ne!(first, second, "unnamed adds must not converge");
    assert_eq!(active_hooks(&project, &[]).len(), 2);
}

/// `--owner` answers "which hooks are mine" without parsing a naming
/// convention out of the description.
#[test]
fn test_owner_filters_the_listing() {
    let project = TestProject::with_name("hook-owner-filter");
    add(&project, &["--name", "a", "--owner", "edict"]);
    add(&project, &["--name", "b", "--owner", "edict"]);
    add(&project, &["--name", "c", "--owner", "someone-else"]);
    add(&project, &[]);

    assert_eq!(active_hooks(&project, &[]).len(), 4, "all hooks");

    let mine = active_hooks(&project, &["--owner", "edict"]);
    assert_eq!(mine.len(), 2, "only edict's hooks: {mine:?}");
    assert!(mine.iter().all(|h| h["owner"] == "edict"));

    assert!(
        active_hooks(&project, &["--owner", "nobody"]).is_empty(),
        "an owner with no hooks lists nothing"
    );
}

/// An existing unnamed hook can be adopted with `hooks set --name`, so the
/// 38 hooks registered before this feature do not have to be recreated to
/// benefit from it — recreating them is the very thing being avoided.
#[test]
fn test_set_can_adopt_an_existing_hook() {
    let project = TestProject::with_name("hook-name-adopt");
    let id = add(&project, &["--lease", "--lease-ttl", "1800"]);

    project
        .run_rite_with_env(
            &[
                "hooks",
                "set",
                &id,
                "--name",
                "edict:rite:responder",
                "--owner",
                "edict",
            ],
            Some("ops"),
        )
        .assert_success();

    let record = latest_record(&project, &id);
    assert_eq!(record["name"], "edict:rite:responder");
    assert_eq!(record["owner"], "edict");
    assert_eq!(
        record["lease"]["ttl_secs"], 1800,
        "adoption must not disturb the lease"
    );

    // And now a converge finds it.
    let second = add(&project, &["--name", "edict:rite:responder"]);
    assert_eq!(second, id, "an adopted hook converges by name");
}
