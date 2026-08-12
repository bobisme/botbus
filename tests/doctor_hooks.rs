//! `rite doctor` flags configuration that cannot possibly work (bn-3q9e,
//! bn-2be4).
//!
//! Doctor used to validate only that storage was readable and writable. It
//! never asked whether anything the configuration pointed at was real, so it
//! reported "Environment is healthy" in two situations where it plainly was
//! not:
//!
//! - Eight of forty-two live hooks pointed at deleted directories. Each one
//!   fired on every matching message, staked its claim, failed to spawn, and
//!   recorded `executed: false` — indistinguishable from a cooldown skip. One
//!   had done that 228 times.
//! - The data directory's git store was corrupt for about 2.5 days after an
//!   unclean shutdown. Every sync commit failed silently. No messages were
//!   lost, because the JSONL is the source of truth, but nothing recorded the
//!   history and nothing said so.

mod common;

use common::TestProject;
use serde_json::Value;

fn doctor(project: &TestProject) -> Value {
    let out = project.run_rite_with_env(&["doctor", "--format", "json"], Some("ops"));
    // Doctor exits non-zero when a check fails, so the status is not asserted
    // here — the checks themselves are what these tests are about.
    serde_json::from_str(&out.stdout_str()).expect("doctor must emit valid json")
}

fn check<'a>(report: &'a Value, name: &str) -> Option<&'a Value> {
    report["checks"]
        .as_array()?
        .iter()
        .find(|c| c["name"] == name)
}

fn add_hook(project: &TestProject, cwd: &str, program: &str) -> String {
    let out = project.run_rite_with_env(
        &[
            "hooks",
            "add",
            "--channel",
            "rite",
            "--mention",
            "worker",
            "--cwd",
            cwd,
            "--",
            program,
            "-c",
            "true",
        ],
        Some("ops"),
    );
    out.assert_success();
    out.stdout_str()
        .split_whitespace()
        .find(|s| s.starts_with("hk-"))
        .expect("hook id")
        .to_string()
}

#[test]
fn test_healthy_hooks_pass() {
    let project = TestProject::with_name("doctor-hooks-ok");
    let cwd = project.work_dir().to_string_lossy().to_string();
    add_hook(&project, &cwd, "sh");

    let report = doctor(&project);
    let hooks = check(&report, "hooks_runnable").expect("hooks_runnable check");
    assert_eq!(hooks["status"], "pass", "{hooks}");
}

/// The eight-dead-hooks case.
#[test]
fn test_missing_cwd_is_flagged() {
    let project = TestProject::with_name("doctor-hooks-dead-cwd");

    // A directory of its own, so removing it does not take the harness's
    // working directory with it.
    let doomed = project.work_dir().join("some-project");
    std::fs::create_dir_all(&doomed).expect("create project dir");
    let id = add_hook(&project, &doomed.to_string_lossy(), "sh");

    // The project directory goes away, exactly as /home/bob/src/botbus did.
    std::fs::remove_dir_all(&doomed).expect("remove project dir");

    let report = doctor(&project);
    let hooks = check(&report, "hooks_runnable").expect("hooks_runnable check");
    assert_eq!(hooks["status"], "warn", "{hooks}");
    let message = hooks["message"].as_str().unwrap_or_default();
    assert!(message.contains(&id), "must name the hook: {message}");
    assert!(
        message.contains("cwd missing"),
        "must say what is wrong: {message}"
    );
}

#[test]
fn test_missing_command_is_flagged() {
    let project = TestProject::with_name("doctor-hooks-no-command");
    let cwd = project.work_dir().to_string_lossy().to_string();
    let id = add_hook(&project, &cwd, "definitely-not-a-real-binary-xyzzy");

    let report = doctor(&project);
    let hooks = check(&report, "hooks_runnable").expect("hooks_runnable check");
    assert_eq!(hooks["status"], "warn", "{hooks}");
    let message = hooks["message"].as_str().unwrap_or_default();
    assert!(message.contains(&id), "must name the hook: {message}");
    assert!(
        message.contains("command not found"),
        "must say what is wrong: {message}"
    );
}

/// Warn, never fail. A hook for a project checked out on another machine is
/// legitimate, and doctor exits non-zero on failure.
#[test]
fn test_broken_hook_does_not_fail_the_run() {
    let project = TestProject::with_name("doctor-hooks-warn-only");
    let cwd = project.work_dir().to_string_lossy().to_string();
    add_hook(&project, &cwd, "definitely-not-a-real-binary-xyzzy");

    let out = project.run_rite_with_env(&["doctor", "--format", "json"], Some("ops"));
    assert!(
        out.success(),
        "an unrunnable hook is a warning, not a failure: {}",
        out.stdout_str()
    );
}

/// No git store means sync was never set up, which is not a problem.
#[test]
fn test_no_data_repo_is_not_reported() {
    let project = TestProject::with_name("doctor-no-git");
    let report = doctor(&project);
    assert!(
        check(&report, "data_repo_git").is_none(),
        "a data dir without a repo must not report on one: {report}"
    );
}

/// The 2026-07-06 case: a git store that cannot read its own HEAD.
#[test]
fn test_corrupt_data_repo_is_flagged() {
    let project = TestProject::with_name("doctor-broken-git");

    // A .git directory that git cannot make sense of.
    let git_dir = project.data_path().join(".git");
    std::fs::create_dir_all(&git_dir).expect("create .git");
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");

    let report = doctor(&project);
    let git = check(&report, "data_repo_git").expect("data_repo_git check");
    assert_eq!(git["status"], "fail", "{git}");
    let suggestion = git["suggestion"].as_str().unwrap_or_default();
    assert!(
        suggestion.contains("source of truth"),
        "must say messages are safe: {suggestion}"
    );
}
