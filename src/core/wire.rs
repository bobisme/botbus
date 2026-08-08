//! Wire-format compatibility for tagged enums.
//!
//! Rite instances sync JSONL files over git, so an older build routinely reads
//! records written by a newer one. The tagged enums on the wire therefore
//! tolerate *unrecognized tags* — and only those. Three inputs that a naive
//! fallback would collapse into one must stay distinct:
//!
//! | input                                    | meaning                       | handling                          |
//! |------------------------------------------|-------------------------------|-----------------------------------|
//! | tag is not in `KNOWN_TAGS`                | variant added by a newer rite | `Unknown`, payload kept verbatim  |
//! | tag is in `KNOWN_TAGS`, body does not fit | corruption                    | hard parse error                  |
//! | no tag at all                             | not a shape rite ever writes  | hard parse error                  |
//!
//! The first case is benign and expected in a mixed-version fleet: ignore what
//! you do not understand, but keep it. The other two are data damage. Letting
//! them fall into `Unknown` would make real corruption invisible — a claim
//! whose `patterns` field was mangled would silently become a meaningless
//! record with nothing reporting it. Instead they fail the parse, which makes
//! the JSONL reader skip the line, count it, and surface it in `rite doctor`.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A tagged enum that tolerates variants written by other rite versions.
///
/// Implementors derive serde with `#[serde(remote = "Self")]` plus a trailing
/// `#[serde(untagged)] Unknown(Value)` variant, then delegate their
/// `Deserialize` impl to [`deserialize`]. The derive supplies the variant
/// dispatch ([`parse_known`](ForwardCompatible::parse_known)); this trait
/// supplies the tag-level policy the derive cannot express.
pub trait ForwardCompatible: Sized {
    /// Name used in diagnostics, e.g. "message meta".
    const WIRE_NAME: &'static str;

    /// Every tag this build understands. Must list exactly the non-`Unknown`
    /// variants; the tests guard the two from drifting apart.
    const KNOWN_TAGS: &'static [&'static str];

    /// Read the variant tag out of a raw record, if it has one.
    fn tag(value: &Value) -> Option<&str>;

    /// The serde-derived parse. Falls back to `Unknown` on any failure, which
    /// is why [`deserialize`] checks the tag before trusting the result.
    fn parse_known(value: &Value) -> Result<Self, serde_json::Error>;

    /// Wrap an unrecognized record.
    fn unknown(value: Value) -> Self;

    /// Whether this is the `Unknown` variant.
    fn is_unknown(&self) -> bool;
}

/// Tag of an internally tagged record, e.g. `{"type":"claim",...}`.
pub fn internal_tag<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field)?.as_str()
}

/// Tag of an externally tagged record: either `"registered"` (unit variant) or
/// `{"renamed":{...}}` (a single-key object).
pub fn external_tag(value: &Value) -> Option<&str> {
    match value {
        Value::String(tag) => Some(tag.as_str()),
        Value::Object(map) if map.len() == 1 => map.keys().next().map(String::as_str),
        _ => None,
    }
}

/// Deserialize a [`ForwardCompatible`] enum, keeping future variants and
/// rejecting corrupt ones. See the module docs for the policy.
pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: ForwardCompatible,
    D: Deserializer<'de>,
{
    use serde::de::Error as _;

    let value = Value::deserialize(deserializer)?;

    let Some(tag) = T::tag(&value) else {
        return Err(D::Error::custom(format!(
            "corrupt {} record: no variant tag",
            T::WIRE_NAME
        )));
    };

    // A tag this build has never heard of is a newer rite's variant, not damage.
    if !T::KNOWN_TAGS.contains(&tag) {
        return Ok(T::unknown(value));
    }

    match T::parse_known(&value) {
        Ok(parsed) if parsed.is_unknown() => Err(D::Error::custom(format!(
            "corrupt {} record: `{}` is a known variant but its body does not match this build's schema",
            T::WIRE_NAME,
            tag
        ))),
        Ok(parsed) => Ok(parsed),
        Err(error) => Err(D::Error::custom(format!(
            "corrupt {} record: `{}`: {}",
            T::WIRE_NAME,
            tag,
            error
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_internal_tag() {
        let value: Value = serde_json::from_str(r#"{"type":"claim","patterns":[]}"#).unwrap();
        assert_eq!(internal_tag(&value, "type"), Some("claim"));

        let untagged: Value = serde_json::from_str(r#"{"patterns":[]}"#).unwrap();
        assert_eq!(internal_tag(&untagged, "type"), None);

        // A non-string tag is not a tag.
        let numeric: Value = serde_json::from_str(r#"{"type":7}"#).unwrap();
        assert_eq!(internal_tag(&numeric, "type"), None);
    }

    #[test]
    fn test_external_tag() {
        let unit: Value = serde_json::from_str(r#""registered""#).unwrap();
        assert_eq!(external_tag(&unit), Some("registered"));

        let struct_variant: Value =
            serde_json::from_str(r#"{"renamed":{"old_name":"a"}}"#).unwrap();
        assert_eq!(external_tag(&struct_variant), Some("renamed"));

        // Ambiguous or non-enum shapes have no tag.
        let two_keys: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        assert_eq!(external_tag(&two_keys), None);
        assert_eq!(external_tag(&Value::Null), None);
    }
}
