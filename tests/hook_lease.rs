//! Per-(hook, channel) spawn leases and message batching (bn-fsx0).
//!
//! `cooldown_secs` guessed at "is a spawn still running?" with a wall clock,
//! and got it wrong in both directions. A lease answers the question directly:
//! it is a claim on a rite-owned pattern, held by the spawned agent, and
//! triggers that arrive while it is held are batched into the next spawn
//! instead of being dropped.
//!
//! The test that matters most here is [`test_stuck_lease_recovers_when_holder_presence_lapses`]:
//! a lease whose holder is killed must not stop a channel spawning forever. A
//! fleet that silently stops is worse than one that spawns twice.
//!
//! Every test runs against its own `RITE_DATA_DIR` (see `common::TestProject`)
//! and spawns nothing but `sh`.

mod common;

use common::TestProject;
use rite::core::presence::PRESENCE_TTL_SECS;
use rite::storage::AgentStateManager;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Hook command: record the batch this spawn was handed, one line per spawn.
fn record_batch_cmd(out: &Path) -> String {
    format!(
        "printf '%s\\n' \"$RITE_BATCH_MESSAGE_IDS\" >> {}",
        out.display()
    )
}

fn out_file(project: &TestProject) -> PathBuf {
    project.work_dir().join("spawns.txt")
}

fn spawn_lines(out: &Path) -> Vec<String> {
    std::fs::read_to_string(out)
        .unwrap_or_default()
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// Wait for `want` spawns to have recorded themselves. Hooks spawn
/// asynchronously, so polling is the only honest way to observe them.
fn wait_for_spawns(out: &Path, want: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let lines = spawn_lines(out);
        if lines.len() >= want {
            return lines;
        }
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for {} spawn(s) in {}; saw {:?}",
                want,
                out.display(),
                lines
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Assert no *further* spawn happens. Gives a would-be spawn time to show up
/// before concluding it did not.
fn assert_no_more_spawns(out: &Path, expected: usize) {
    std::thread::sleep(Duration::from_millis(600));
    let lines = spawn_lines(out);
    assert_eq!(
        lines.len(),
        expected,
        "expected the lease to hold spawns at {}, got {:?}",
        expected,
        lines
    );
}

/// Queued-but-undelivered triggers, straight out of the queue file.
fn pending_triggers(project: &TestProject) -> Vec<serde_json::Value> {
    let path = project.data_path().join("hook_queue.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    let content = std::fs::read_to_string(&path).expect("failed to read hook queue");
    let mut latest: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        let entry: serde_json::Value = serde_json::from_str(line).expect("invalid queue JSON");
        latest.insert(entry["id"].as_str().unwrap().to_string(), entry);
    }
    latest
        .into_values()
        .filter(|e| !e["delivered"].as_bool().unwrap_or(false))
        .collect()
}

fn claim_records(project: &TestProject) -> Vec<serde_json::Value> {
    let path = project.data_path().join("claims.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .expect("failed to read claims")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("invalid claim JSON"))
        .collect()
}

fn audit_reasons(project: &TestProject) -> Vec<String> {
    let path = project.data_path().join("hooks_audit.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .expect("failed to read hook audit")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            Some(v["reason"].as_str()?.to_string())
        })
        .collect()
}

/// Message ID of the message with exactly this body.
fn message_id(project: &TestProject, channel: &str, body: &str) -> String {
    project
        .channel_messages(channel)
        .into_iter()
        .find(|m| m["body"] == body)
        .unwrap_or_else(|| panic!("no message with body {body:?} in #{channel}"))["id"]
        .as_str()
        .expect("message id should be a string")
        .to_string()
}

/// Simulate the holder's process dying: its heartbeat stops, so presence
/// lapses. Same mechanism `tests/presence.rs` uses.
fn lapse_presence(project: &TestProject, agent: &str) {
    let manager = AgentStateManager::new(project.data_path(), agent);
    let backdated = chrono::Utc::now() - chrono::Duration::seconds(PRESENCE_TTL_SECS + 30);
    manager
        .update(|s| {
            s.last_heartbeat = Some(backdated);
        })
        .expect("failed to backdate heartbeat");
}

/// Age the lease claim so it is past the grace window in which a lease is
/// held regardless of presence. Rewrites `ts` only — the claim stays active,
/// unexpired and owned by the same agent.
fn age_lease_claim(project: &TestProject, secs: i64) {
    let path = project.data_path().join("claims.jsonl");
    let content = std::fs::read_to_string(&path).expect("failed to read claims");
    let backdated = chrono::Utc::now() - chrono::Duration::seconds(secs);
    let rewritten: Vec<String> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut claim: serde_json::Value = serde_json::from_str(line).unwrap();
            let is_lease = claim["patterns"]
                .as_array()
                .is_some_and(|ps| ps.iter().any(|p| p.as_str().is_some_and(is_lease_pattern)));
            if is_lease {
                claim["ts"] = serde_json::Value::String(backdated.to_rfc3339());
            }
            serde_json::to_string(&claim).unwrap()
        })
        .collect();
    std::fs::write(&path, format!("{}\n", rewritten.join("\n"))).expect("failed to write claims");
}

fn is_lease_pattern(pattern: &str) -> bool {
    pattern.starts_with(rite::core::hook::LEASE_SCHEME)
}

fn lease_claims(project: &TestProject) -> Vec<serde_json::Value> {
    // Latest record per claim ID wins, exactly as rite reads them.
    let mut latest: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for claim in claim_records(project) {
        let is_lease = claim["patterns"]
            .as_array()
            .is_some_and(|ps| ps.iter().any(|p| p.as_str().is_some_and(is_lease_pattern)));
        if is_lease {
            latest.insert(claim["id"].as_str().unwrap().to_string(), claim);
        }
    }
    latest.into_values().collect()
}

/// A mention hook whose only concurrency control is the spawn lease.
fn add_lease_hook(project: &TestProject, extra: &[&str]) {
    let out = out_file(project);
    let script = record_batch_cmd(&out);
    let cwd = project.work_dir().to_string_lossy().to_string();

    let mut args = vec![
        "hooks",
        "add",
        "--channel",
        "rite",
        "--mention",
        "worker",
        "--lease",
        "--claim-owner",
        "spawn-agent",
        "--cwd",
        &cwd,
    ];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", "sh", "-c", &script]);

    project
        .run_rite_with_env(&args, Some("ops"))
        .assert_success();
}

/// The shape most existing hooks use: fire while a claim is free, stake it
/// for the spawned agent. Adding a lease is what stops the messages that
/// arrive while that claim is held from being thrown away.
fn add_claim_available_lease_hook(project: &TestProject) -> String {
    let out = out_file(project);
    let script = record_batch_cmd(&out);
    let cwd = project.work_dir().to_string_lossy().to_string();

    project
        .run_rite_with_env(
            &[
                "hooks",
                "add",
                "--channel",
                "rite",
                "--claim",
                "agent://spawn-agent",
                "--ttl",
                "600",
                "--lease",
                "--claim-owner",
                "spawn-agent",
                "--cwd",
                &cwd,
                "--",
                "sh",
                "-c",
                &script,
            ],
            Some("ops"),
        )
        .assert_success();

    let listing = project.run_rite_with_env(&["hooks", "list", "--format", "text"], Some("ops"));
    listing.assert_success();
    listing
        .stdout_str()
        .split_whitespace()
        .next()
        .expect("hooks list should name the hook")
        .to_string()
}

/// A trigger that arrives while the hook's *own* claim is held is queued too,
/// not dropped. That claim is the "an agent is already working" signal for
/// every claim-available hook, and dropping what arrives behind it is exactly
/// the work loss a cooldown caused.
#[test]
fn test_trigger_behind_a_held_hook_claim_is_queued() {
    let mut project = TestProject::with_name("hook-lease-claim-busy");
    let hook_id = add_claim_available_lease_hook(&project);

    let human = project.agent("human");
    let out = out_file(&project);

    human.send("rite", "first task").assert_success();
    wait_for_spawns(&out, 1);

    // Free the lease but not the hook's claim — the spawned agent is still
    // holding `agent://spawn-agent`, i.e. still working.
    let spawn_agent = project.agent("spawn-agent");
    spawn_agent
        .run(&["claims", "release", &format!("spawn://{hook_id}/rite")])
        .assert_success();

    human.send("rite", "second task").assert_success();
    assert_no_more_spawns(&out, 1);

    let pending = pending_triggers(&project);
    assert_eq!(
        pending.len(),
        1,
        "work that arrives while the hook's claim is held must be kept: {pending:?}"
    );
    assert!(
        audit_reasons(&project)
            .iter()
            .any(|r| r == "claim unavailable (atomic check) (queued)"),
        "the queue-instead-of-drop decision must be auditable: {:?}",
        audit_reasons(&project)
    );

    // When the agent finishes for real, the queued work goes out with the
    // next trigger.
    spawn_agent.release_all().assert_success();
    human.send("rite", "third task").assert_success();
    let spawns = wait_for_spawns(&out, 2);
    let second = message_id(&project, "rite", "second task");
    assert!(
        spawns[1].contains(&second),
        "the queued trigger must reach the next spawn: {}",
        spawns[1]
    );
}

#[test]
fn test_lease_held_second_trigger_is_batched_not_dropped() {
    let mut project = TestProject::with_name("hook-lease-holds");
    add_lease_hook(&project, &[]);

    let human = project.agent("human");
    let out = out_file(&project);

    human.send("rite", "@worker first task").assert_success();
    let spawns = wait_for_spawns(&out, 1);
    assert_eq!(spawns.len(), 1, "first message should spawn");

    // Lease is still held by spawn-agent (nothing released it), so this must
    // not spawn — and must not be lost either.
    human.send("rite", "@worker second task").assert_success();
    assert_no_more_spawns(&out, 1);

    let pending = pending_triggers(&project);
    assert_eq!(
        pending.len(),
        1,
        "the trigger that arrived under the lease must be queued, not dropped: {pending:?}"
    );
    assert_eq!(
        pending[0]["message_id"].as_str().unwrap(),
        message_id(&project, "rite", "@worker second task"),
        "the queued entry must point at the message that was held back"
    );

    assert!(
        audit_reasons(&project)
            .iter()
            .any(|r| r == "lease held (queued)"),
        "the hold must be auditable, not silent: {:?}",
        audit_reasons(&project)
    );
}

#[test]
fn test_released_lease_delivers_queued_batch_deduped() {
    let mut project = TestProject::with_name("hook-lease-batch");
    add_lease_hook(&project, &[]);

    let human = project.agent("human");
    let out = out_file(&project);

    human.send("rite", "@worker first task").assert_success();
    wait_for_spawns(&out, 1);

    // Three more arrive mid-turn; two of them are the identical trigger.
    human.send("rite", "@worker task alpha").assert_success();
    human.send("rite", "@worker task beta").assert_success();
    human.send("rite", "@worker task alpha").assert_success();
    assert_no_more_spawns(&out, 1);

    assert_eq!(
        pending_triggers(&project).len(),
        2,
        "identical triggers from the same sender must collapse into one"
    );

    // The spawned agent finishes its turn and drops its lease.
    let spawn_agent = project.agent("spawn-agent");
    spawn_agent.release_all().assert_success();

    human.send("rite", "@worker task gamma").assert_success();
    let spawns = wait_for_spawns(&out, 2);

    let batch = &spawns[1];
    let alpha = message_id(&project, "rite", "@worker task alpha");
    let beta = message_id(&project, "rite", "@worker task beta");
    let gamma = message_id(&project, "rite", "@worker task gamma");

    let ids: Vec<&str> = batch.split(',').collect();
    assert_eq!(
        ids.len(),
        3,
        "the next spawn should get the two queued triggers plus its own: {batch}"
    );
    assert!(ids.contains(&alpha.as_str()), "missing alpha in {batch}");
    assert!(ids.contains(&beta.as_str()), "missing beta in {batch}");
    assert_eq!(
        ids.last(),
        Some(&gamma.as_str()),
        "the triggering message should come last in {batch}"
    );

    assert!(
        pending_triggers(&project).is_empty(),
        "delivered triggers must not be handed to a second spawn as well"
    );
}

/// The fleet-stall guard.
///
/// The lease holder is killed without ever releasing anything. Its claim has
/// hours left on its TTL, so nothing about the claim itself will free the
/// channel. What frees it is presence: the holder's heartbeat has lapsed, so
/// the lease is *superseded* — the next trigger stakes its own lease and
/// spawns. The dead holder's claim is not touched.
#[test]
fn test_stuck_lease_recovers_when_holder_presence_lapses() {
    let mut project = TestProject::with_name("hook-lease-stuck");
    add_lease_hook(&project, &[]);

    let human = project.agent("human");
    let out = out_file(&project);

    human.send("rite", "@worker first task").assert_success();
    wait_for_spawns(&out, 1);

    // Work arrives while the (about to die) holder still owns the lease.
    human.send("rite", "@worker queued task").assert_success();
    assert_no_more_spawns(&out, 1);
    assert_eq!(pending_triggers(&project).len(), 1);

    let lease_before = lease_claims(&project);
    assert_eq!(lease_before.len(), 1, "one lease so far");
    let dead_lease_id = lease_before[0]["id"].as_str().unwrap().to_string();

    // The holder dies: killed process, reboot, whatever. It never releases,
    // it never expires, it just stops heartbeating.
    lapse_presence(&project, "spawn-agent");
    age_lease_claim(&project, PRESENCE_TTL_SECS + 30);

    // Spawning must resume, and must carry the work that queued up while the
    // dead holder was nominally in charge.
    human.send("rite", "@worker after death").assert_success();
    let spawns = wait_for_spawns(&out, 2);

    let queued = message_id(&project, "rite", "@worker queued task");
    let after = message_id(&project, "rite", "@worker after death");
    let ids: Vec<&str> = spawns[1].split(',').collect();
    assert_eq!(
        ids,
        vec![queued.as_str(), after.as_str()],
        "the spawn that resumed must carry the queued work: {}",
        spawns[1]
    );

    // The invariant from bn-12i6: nothing auto-releases another agent's
    // claim. The dead holder's lease record is still exactly as written.
    let dead_lease = claim_records(&project)
        .into_iter()
        .filter(|c| c["id"].as_str() == Some(dead_lease_id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        dead_lease.len(),
        1,
        "superseding a lease must not write any record against the holder's claim: {dead_lease:?}"
    );
    assert_eq!(
        dead_lease[0]["active"], true,
        "the dead holder's claim must still be active — staleness is a report, not a release"
    );
    assert_eq!(dead_lease[0]["event"], "created");
    assert_eq!(dead_lease[0]["agent"], "spawn-agent");

    // Supersession is additive: a second, live lease now sits alongside it.
    assert_eq!(
        lease_claims(&project).len(),
        2,
        "the new lease should be staked next to the abandoned one, not on top of it"
    );
}

/// The TTL backstop, for the case presence cannot speak to: a holder that
/// never recorded a heartbeat at all is `Unknown`, and `Unknown` is
/// deliberately not stale. The lease's own expiry has to be what frees the
/// channel.
#[test]
fn test_lease_ttl_frees_channel_when_holder_presence_is_unknown() {
    let mut project = TestProject::with_name("hook-lease-ttl");
    add_lease_hook(&project, &["--lease-ttl", "1"]);

    let human = project.agent("human");
    let out = out_file(&project);

    human.send("rite", "@worker first task").assert_success();
    wait_for_spawns(&out, 1);

    human.send("rite", "@worker queued task").assert_success();
    assert_no_more_spawns(&out, 1);
    assert_eq!(pending_triggers(&project).len(), 1);

    // spawn-agent has never run a rite command, so its presence is Unknown,
    // never Lapsed. Only the TTL can free this lease.
    std::thread::sleep(Duration::from_millis(1400));

    human.send("rite", "@worker later task").assert_success();
    let spawns = wait_for_spawns(&out, 2);

    let queued = message_id(&project, "rite", "@worker queued task");
    assert!(
        spawns[1].contains(&queued),
        "work queued behind an expired lease must still be delivered: {}",
        spawns[1]
    );
}

/// Hooks written before leases existed must behave exactly as they did:
/// cooldown gates them, nothing is queued, and no lease claim is invented on
/// their behalf.
#[test]
fn test_cooldown_hook_without_lease_is_unchanged() {
    let mut project = TestProject::with_name("hook-lease-legacy");
    let out = out_file(&project);
    let script = record_batch_cmd(&out);
    let cwd = project.work_dir().to_string_lossy().to_string();

    let ops = project.agent("ops");
    ops.run(&[
        "hooks",
        "add",
        "--channel",
        "rite",
        "--mention",
        "worker",
        "--cooldown",
        "5m",
        "--cwd",
        &cwd,
        "--",
        "sh",
        "-c",
        &script,
    ])
    .assert_success();

    let human = project.agent("human");

    human.send("rite", "@worker first task").assert_success();
    wait_for_spawns(&out, 1);

    human.send("rite", "@worker second task").assert_success();
    assert_no_more_spawns(&out, 1);

    assert!(
        audit_reasons(&project)
            .iter()
            .any(|r| r == "cooldown active"),
        "a hook without a lease must still be gated by its cooldown: {:?}",
        audit_reasons(&project)
    );
    assert!(
        pending_triggers(&project).is_empty(),
        "a hook without a lease must not start queueing behind the scenes"
    );
    assert!(
        lease_claims(&project).is_empty(),
        "no lease claim should be staked for a hook that has no lease"
    );
    assert!(
        !project.data_path().join("hook_queue.jsonl").exists(),
        "a lease-free data directory should not grow a queue file at all"
    );

    // And the spawn's environment is unchanged: no batch variables.
    assert_eq!(
        spawn_lines(&out),
        vec![""],
        "a hook without a lease must not receive RITE_BATCH_MESSAGE_IDS"
    );
}
