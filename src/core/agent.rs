use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::wire::{self, ForwardCompatible};

/// Registered agent identity within a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Timestamp of registration
    pub ts: DateTime<Utc>,

    /// Unique agent name within this project
    pub name: String,

    /// Optional description or identifier (e.g., "Claude Sonnet 3.5")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Registration event type
    pub event: AgentEvent,
}

impl Agent {
    /// Create a new agent registration record.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            ts: Utc::now(),
            name: name.into(),
            description: None,
            event: AgentEvent::Registered,
        }
    }

    /// Create an agent with a description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Create a rename event record.
    pub fn renamed(new_name: impl Into<String>, old_name: impl Into<String>) -> Self {
        Self {
            ts: Utc::now(),
            name: new_name.into(),
            description: None,
            event: AgentEvent::Renamed {
                old_name: old_name.into(),
            },
        }
    }
}

/// Why an [`Agent`] record was written.
///
/// An event kind this build does not recognize becomes [`AgentEvent::Unknown`]
/// rather than failing the read of the whole agents file. A recognized kind
/// with a broken body is corruption and still fails loudly — see
/// [`crate::core::wire`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", remote = "Self")]
pub enum AgentEvent {
    Registered,
    Renamed {
        old_name: String,
    },
    /// An event kind written by a newer rite, kept verbatim.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ForwardCompatible for AgentEvent {
    const WIRE_NAME: &'static str = "agent event";
    const KNOWN_TAGS: &'static [&'static str] = &["registered", "renamed"];

    fn tag(value: &serde_json::Value) -> Option<&str> {
        wire::external_tag(value)
    }

    fn parse_known(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        AgentEvent::deserialize(value)
    }

    fn unknown(value: serde_json::Value) -> Self {
        AgentEvent::Unknown(value)
    }

    fn is_unknown(&self) -> bool {
        matches!(self, AgentEvent::Unknown(_))
    }
}

impl Serialize for AgentEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        AgentEvent::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for AgentEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        wire::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_roundtrip() {
        let agent = Agent::new("BlueCastle").with_description("Claude Sonnet 3.5");

        let json = serde_json::to_string(&agent).unwrap();
        let parsed: Agent = serde_json::from_str(&json).unwrap();

        assert_eq!(agent.name, parsed.name);
        assert_eq!(agent.description, parsed.description);
    }

    #[test]
    fn test_agent_renamed() {
        let agent = Agent::renamed("NewName", "OldName");

        let json = serde_json::to_string(&agent).unwrap();
        assert!(json.contains("renamed"));
        assert!(json.contains("OldName"));
    }

    /// An event kind added by a newer rite must parse into `Unknown` and
    /// round-trip verbatim instead of failing the read.
    #[test]
    fn test_unknown_agent_event_round_trips() {
        let json =
            r#"{"ts":"2026-01-01T00:00:00Z","name":"future","event":{"retired":{"why":"done"}}}"#;
        let agent: Agent = serde_json::from_str(json).expect("unknown event must not fail parsing");
        assert_eq!(agent.name, "future");
        assert!(matches!(agent.event, AgentEvent::Unknown(_)));

        assert_eq!(
            serde_json::to_value(&agent).unwrap(),
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );

        // A future unit-style event works too.
        let unit: AgentEvent = serde_json::from_str(r#""evicted""#).unwrap();
        assert!(matches!(unit, AgentEvent::Unknown(_)));

        // Known events still take priority over the fallback.
        let known: AgentEvent = serde_json::from_str(r#""registered""#).unwrap();
        assert!(matches!(known, AgentEvent::Registered));
    }

    /// A recognized event kind with a damaged body is corruption, not a future
    /// format, and must fail the parse rather than degrading to `Unknown`.
    #[test]
    fn test_corrupt_known_agent_event_is_an_error() {
        let error = serde_json::from_str::<AgentEvent>(r#"{"renamed":{}}"#)
            .expect_err("a damaged known variant must not deserialize");
        assert!(error.to_string().contains("corrupt"), "{}", error);
        assert!(error.to_string().contains("renamed"), "{}", error);

        // The same shape with an unrecognized kind stays benign.
        assert!(matches!(
            serde_json::from_str::<AgentEvent>(r#"{"retired":{}}"#).unwrap(),
            AgentEvent::Unknown(_)
        ));

        // A whole Agent record inherits the distinction.
        assert!(
            serde_json::from_str::<Agent>(
                r#"{"ts":"2026-01-01T00:00:00Z","name":"a","event":{"renamed":{}}}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Agent>(
                r#"{"ts":"2026-01-01T00:00:00Z","name":"a","event":{"retired":{}}}"#
            )
            .is_ok()
        );
    }

    /// Guard against `KNOWN_TAGS` drifting away from the variants. The match is
    /// exhaustive, so adding a variant fails to compile until it is listed.
    #[test]
    fn test_agent_event_known_tags_match_variants() {
        fn tag_of(event: &AgentEvent) -> Option<&'static str> {
            match event {
                AgentEvent::Registered => Some("registered"),
                AgentEvent::Renamed { .. } => Some("renamed"),
                AgentEvent::Unknown(_) => None,
            }
        }

        let samples = vec![
            AgentEvent::Registered,
            AgentEvent::Renamed {
                old_name: "a".to_string(),
            },
        ];
        assert_eq!(
            samples.len(),
            AgentEvent::KNOWN_TAGS.len(),
            "every known tag needs a sample here"
        );

        for sample in &samples {
            let tag = tag_of(sample).expect("samples are known variants");
            assert!(
                AgentEvent::KNOWN_TAGS.contains(&tag),
                "missing tag: {}",
                tag
            );

            let value = serde_json::to_value(sample).unwrap();
            assert_eq!(AgentEvent::tag(&value), Some(tag));
            let parsed: AgentEvent = serde_json::from_value(value).unwrap();
            assert!(!parsed.is_unknown(), "{} fell into the fallback", tag);
        }
    }
}
