//! Claim staleness derived from agent presence (bn-12i6).
//!
//! Presence and claims are meant to stay decoupled at the lock level: a
//! claim's own TTL and active/released state are untouched by whether its
//! holder is alive. What these tests check is the *reporting* layer built
//! on top - `rite claims list` and `rite claims check` should be able to
//! tell a live holder from a lapsed one, and nothing here should ever
//! release, expire, or otherwise mutate another agent's claim.

mod common;

use common::TestProject;
use rite::core::presence::PRESENCE_TTL_SECS;
use rite::storage::AgentStateManager;

/// Backdate `agent`'s last heartbeat by `age_secs` seconds, directly through
/// the same per-agent state file `rite` itself writes to. This simulates a
/// process that heartbeated once and then died, without a real test having
/// to sleep past the TTL.
fn backdate_heartbeat(project: &TestProject, agent: &str, age_secs: i64) {
    let manager = AgentStateManager::new(project.data_path(), agent);
    let backdated = chrono::Utc::now() - chrono::Duration::seconds(age_secs);
    manager
        .update(|s| {
            s.last_heartbeat = Some(backdated);
        })
        .expect("failed to backdate heartbeat");
}

/// Find a claim's JSON `ClaimInfo` entry for `agent` in `claims list --format json` output.
fn find_claim_info(json: &serde_json::Value, agent: &str) -> serde_json::Value {
    json["claims"]
        .as_array()
        .expect("claims field should be an array")
        .iter()
        .find(|c| c["agent"] == agent)
        .unwrap_or_else(|| panic!("no claim found for agent {agent} in: {json}"))
        .clone()
}

#[test]
fn test_claims_list_reports_live_holder_as_not_stale() {
    let mut project = TestProject::with_name("presence-live-holder");

    let holder = project.agent("LiveHolder");
    // Staking a claim is itself a `rite` invocation, so it records a fresh
    // heartbeat for LiveHolder as a side effect - no backdating needed.
    holder.claim(&["src/**"]).assert_success();

    let checker = project.agent("Checker");
    let output = checker.run(&["claims", "list", "--format", "json"]);
    output.assert_success();

    let json: serde_json::Value = serde_json::from_str(&output.stdout_str())
        .expect("claims list --format json should be valid JSON");
    let claim = find_claim_info(&json, "LiveHolder");

    assert_eq!(claim["active"], true, "claim should still be active");
    assert_eq!(
        claim["stale"], false,
        "a holder with a fresh heartbeat must not be reported stale: {claim}"
    );
}

#[test]
fn test_claims_list_reports_lapsed_holder_as_stale() {
    let mut project = TestProject::with_name("presence-lapsed-holder");

    let holder = project.agent("DeadHolder");
    holder.claim(&["src/**"]).assert_success();

    // Simulate the holder's process dying: push its last heartbeat well
    // past the TTL window.
    backdate_heartbeat(&project, "DeadHolder", PRESENCE_TTL_SECS + 30);

    let checker = project.agent("Checker");
    let output = checker.run(&["claims", "list", "--format", "json"]);
    output.assert_success();

    let json: serde_json::Value = serde_json::from_str(&output.stdout_str()).unwrap();
    let claim = find_claim_info(&json, "DeadHolder");

    // The claim itself is still fully intact - staleness is a report, not
    // a state change to the claim.
    assert_eq!(
        claim["active"], true,
        "a lapsed holder's claim must remain active - staleness is a report, not a release"
    );
    assert_eq!(
        claim["stale"], true,
        "a holder with a heartbeat older than the TTL must be reported stale: {claim}"
    );
}

#[test]
fn test_claims_check_distinguishes_live_from_stale_holder() {
    let mut project = TestProject::with_name("presence-check-distinct");

    let live = project.agent("LiveBlocker");
    live.run(&["claims", "stake", "live/**"]).assert_success();

    let dead = project.agent("DeadBlocker");
    dead.run(&["claims", "stake", "dead/**"]).assert_success();
    backdate_heartbeat(&project, "DeadBlocker", PRESENCE_TTL_SECS + 30);

    let checker = project.agent("Checker");

    // Conflict against the live holder.
    let live_check = checker.run(&["claims", "check", "live/foo.rs", "--format", "json"]);
    // check exits 1 when a conflict is found - that's expected here.
    let live_json: serde_json::Value = serde_json::from_str(&live_check.stdout_str()).unwrap();
    assert_eq!(live_json["safe"], false);
    let live_conflict = &live_json["conflicts"][0];
    assert_eq!(live_conflict["agent"], "LiveBlocker");
    assert_eq!(
        live_conflict["stale"], false,
        "conflict with a live holder must not be reported stale: {live_conflict}"
    );

    // Conflict against the dead holder.
    let dead_check = checker.run(&["claims", "check", "dead/foo.rs", "--format", "json"]);
    let dead_json: serde_json::Value = serde_json::from_str(&dead_check.stdout_str()).unwrap();
    assert_eq!(dead_json["safe"], false);
    let dead_conflict = &dead_json["conflicts"][0];
    assert_eq!(dead_conflict["agent"], "DeadBlocker");
    assert_eq!(
        dead_conflict["stale"], true,
        "conflict with a lapsed holder must be reported stale: {dead_conflict}"
    );

    // Both are still reported as real conflicts ("blocked" either way) -
    // staleness changes what a caller is told about the holder, not
    // whether the path is safe to edit.
    assert_eq!(live_check.status.code(), Some(1));
    assert_eq!(dead_check.status.code(), Some(1));
}

/// Hard constraint: staleness must never cause a claim to be auto-released.
/// A stale claim keeps blocking new stakes and keeps surviving other
/// agents' `release --all` exactly like a live claim would - the only
/// difference is what gets reported about the holder.
#[test]
fn test_stale_claim_is_never_auto_released() {
    let mut project = TestProject::with_name("presence-no-auto-release");

    let dead = project.agent("DeadHolder");
    dead.run(&["claims", "stake", "important/**"])
        .assert_success();
    backdate_heartbeat(&project, "DeadHolder", PRESENCE_TTL_SECS + 30);

    // Merely listing/checking claims (which is what surfaces staleness)
    // must not itself mutate anything.
    let checker = project.agent("Checker");
    checker
        .run(&["claims", "list", "--format", "json"])
        .assert_success();
    checker
        .run(&["claims", "check", "important/foo.rs"])
        .assert_failure(); // conflict -> exit 1, still just a report

    let claims = project.active_claims();
    assert_eq!(
        claims.len(),
        1,
        "the stale claim must still be the one and only active claim"
    );
    assert_eq!(
        claims[0]["agent"], "DeadHolder",
        "ownership must be unchanged"
    );
    assert_eq!(claims[0]["active"], true);

    // Another agent trying to release everything must not touch a claim it
    // doesn't own, stale or not.
    let other = project.agent("Other");
    other.release_all().assert_success();

    let claims_after_release_attempt = project.active_claims();
    assert_eq!(
        claims_after_release_attempt.len(),
        1,
        "another agent's release --all must not release a stale claim it doesn't own"
    );

    // A conflicting stake from a third agent must still be denied - a
    // stale claim keeps blocking exactly like a live one would. Staleness
    // is visibility into who's blocking you, never permission to bypass
    // them.
    let third = project.agent("ThirdAgent");
    let stake_attempt = third.run(&["claims", "stake", "important/foo.rs"]);
    stake_attempt.assert_failure();

    let claims_after_stake_attempt = project.active_claims();
    assert_eq!(
        claims_after_stake_attempt.len(),
        1,
        "a stale claim must still block conflicting new stakes"
    );
    assert_eq!(claims_after_stake_attempt[0]["agent"], "DeadHolder");
}

#[test]
fn test_claims_list_agent_with_no_heartbeat_history_is_not_stale() {
    // An agent that has never recorded a heartbeat (e.g. its claim was
    // staked through a code path that predates this feature) must not be
    // reported as stale by default - unknown presence is not evidence of
    // absence. We simulate this by clearing the heartbeat this test's own
    // `stake` call would have recorded.
    let mut project = TestProject::with_name("presence-unknown-not-stale");

    let holder = project.agent("NeverHeartbeat");
    holder
        .run(&["claims", "stake", "legacy/**"])
        .assert_success();

    let manager = AgentStateManager::new(project.data_path(), "NeverHeartbeat");
    manager
        .update(|s| {
            s.last_heartbeat = None;
        })
        .unwrap();

    let checker = project.agent("Checker");
    let output = checker.run(&["claims", "list", "--format", "json"]);
    output.assert_success();

    let json: serde_json::Value = serde_json::from_str(&output.stdout_str()).unwrap();
    let claim = find_claim_info(&json, "NeverHeartbeat");
    assert_eq!(
        claim["stale"], false,
        "an agent with no heartbeat history at all must not be reported stale: {claim}"
    );
}
