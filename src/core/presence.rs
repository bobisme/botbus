//! Agent presence, derived from heartbeats.
//!
//! Claims (`src/core/claim.rs`) and agent presence used to be entirely
//! independent: a claim's TTL said nothing about whether its holder was
//! still around. That's the failure mode this module exists to close — a
//! dead agent's claim otherwise blocks a live agent until the claim's own
//! (often much longer) TTL expires, with no signal that the holder is gone.
//!
//! The fix is *not* to change claim semantics. Claims remain advisory locks
//! with their own TTL, and nothing here ever releases one — see
//! `crate::cli::claim` for the enforcement boundary. Instead, presence is a
//! second, independent signal: "is this claim's holder still alive?" that
//! callers can use to tell a live blocker from a dead one.
//!
//! ## Heartbeats
//!
//! Every `rite` invocation that resolves an agent identity records a
//! heartbeat as a side effect (see `main.rs`), so presence is derived from
//! actual activity rather than something an agent has to remember to set
//! and clear. This mirrors block/buzz's remote-agent design: there is no
//! management channel to tell rite "I'm still here" or "I'm gone" — liveness
//! is the sole status signal.

use chrono::{DateTime, Utc};

use crate::core::project::data_dir;
use crate::storage::agent_state::AgentStateManager;

/// Expected interval between heartbeats, in seconds.
///
/// In practice a heartbeat fires on every `rite` command an active agent
/// runs, so real-world spacing is often much tighter than this. The
/// constant exists to give [`PRESENCE_TTL_SECS`] a documented basis rather
/// than being a bare magic number.
pub const HEARTBEAT_INTERVAL_SECS: i64 = 60;

/// How long a heartbeat is considered current, in seconds.
///
/// Set to **3x** [`HEARTBEAT_INTERVAL_SECS`], not 1x. A single missed or
/// delayed heartbeat — a slow command, a lull in activity, ordinary clock
/// jitter — must never flip a live agent to "lapsed"; that would turn a
/// coordination aid into a false alarm generator. Requiring three
/// consecutive missed intervals before reporting an agent as gone is long
/// enough to absorb normal gaps in activity, while still being short enough
/// that a genuinely dead agent's claims get flagged promptly instead of
/// silently waiting out the claim's own (often much longer) TTL.
pub const PRESENCE_TTL_SECS: i64 = HEARTBEAT_INTERVAL_SECS * 3;

/// Presence of a single agent, derived from its last recorded heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// A heartbeat was seen within the TTL window — agent presumed alive.
    Live,
    /// A heartbeat exists but is older than the TTL — agent presumed gone.
    Lapsed,
    /// No heartbeat has ever been recorded for this agent (it predates this
    /// feature, or has only ever been driven through code paths that don't
    /// emit heartbeats). Deliberately distinct from `Lapsed`: absence of
    /// evidence is not evidence of a dead agent, so callers must not treat
    /// `Unknown` as stale.
    Unknown,
}

impl Presence {
    /// Whether this presence should be reported as stale to callers.
    ///
    /// Only `Lapsed` is stale. `Unknown` intentionally is not — reporting
    /// every agent that has never emitted a heartbeat as stale would flag
    /// claims held by agents that simply predate this feature, which is
    /// exactly the false-positive behavior the 3x TTL multiplier above is
    /// meant to avoid.
    pub fn is_stale(self) -> bool {
        matches!(self, Presence::Lapsed)
    }

    pub fn is_live(self) -> bool {
        matches!(self, Presence::Live)
    }
}

/// Derive presence from a last-heartbeat timestamp, relative to `now`.
///
/// Split out from [`presence_for`] so tests can pin `now` and check the
/// exact TTL boundary without racing the wall clock.
pub fn presence_for_at(last_heartbeat: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Presence {
    match last_heartbeat {
        None => Presence::Unknown,
        Some(ts) => {
            let age_secs = now.signed_duration_since(ts).num_seconds();
            if age_secs <= PRESENCE_TTL_SECS {
                Presence::Live
            } else {
                Presence::Lapsed
            }
        }
    }
}

/// Derive presence from a last-heartbeat timestamp, as of now.
pub fn presence_for(last_heartbeat: Option<DateTime<Utc>>) -> Presence {
    presence_for_at(last_heartbeat, Utc::now())
}

/// Record a heartbeat for `agent`, marking it present as of now.
///
/// Best-effort by design: callers (currently `main.rs`, on every command
/// that resolves an identity) should not fail the underlying command if
/// recording a heartbeat fails, since presence is an auxiliary signal, not
/// the primitive being coordinated.
pub fn record_heartbeat(agent: &str) -> anyhow::Result<DateTime<Utc>> {
    AgentStateManager::new(&data_dir(), agent).record_heartbeat()
}

/// Derive current presence for `agent` from its last recorded heartbeat.
pub fn agent_presence(agent: &str) -> Presence {
    let last_heartbeat = AgentStateManager::new(&data_dir(), agent)
        .get_last_heartbeat()
        .unwrap_or(None);
    presence_for(last_heartbeat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn test_no_heartbeat_is_unknown_not_stale() {
        let presence = presence_for_at(None, Utc::now());
        assert_eq!(presence, Presence::Unknown);
        assert!(!presence.is_stale());
        assert!(!presence.is_live());
    }

    #[test]
    fn test_recent_heartbeat_is_live() {
        let now = Utc::now();
        let last = now - Duration::seconds(5);
        let presence = presence_for_at(Some(last), now);
        assert_eq!(presence, Presence::Live);
        assert!(presence.is_live());
        assert!(!presence.is_stale());
    }

    #[test]
    fn test_heartbeat_older_than_ttl_is_lapsed() {
        let now = Utc::now();
        let last = now - Duration::seconds(PRESENCE_TTL_SECS + 1);
        let presence = presence_for_at(Some(last), now);
        assert_eq!(presence, Presence::Lapsed);
        assert!(presence.is_stale());
        assert!(!presence.is_live());
    }

    /// Exact TTL boundary: a heartbeat exactly `PRESENCE_TTL_SECS` old is
    /// still live (inclusive), one second older is lapsed. Pinning `now`
    /// keeps this exact instead of racing the wall clock.
    #[test]
    fn test_exact_ttl_boundary() {
        let now = Utc::now();

        let exactly_at_ttl = now - Duration::seconds(PRESENCE_TTL_SECS);
        assert_eq!(
            presence_for_at(Some(exactly_at_ttl), now),
            Presence::Live,
            "heartbeat exactly at the TTL boundary should still count as live"
        );

        let one_second_past_ttl = now - Duration::seconds(PRESENCE_TTL_SECS + 1);
        assert_eq!(
            presence_for_at(Some(one_second_past_ttl), now),
            Presence::Lapsed,
            "heartbeat one second past the TTL boundary should be lapsed"
        );
    }

    #[test]
    fn test_ttl_is_documented_3x_multiple_of_heartbeat_interval() {
        // The ratio itself is the contract described in the bone and in
        // this module's docs: a single missed heartbeat must never flap an
        // agent offline. Pin the relationship so it can't silently drift.
        assert_eq!(PRESENCE_TTL_SECS, HEARTBEAT_INTERVAL_SECS * 3);
    }
}
