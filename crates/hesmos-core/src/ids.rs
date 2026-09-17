//! TYPE-1 identifier newtypes.
//!
//! Every identifier crossing a boundary is a newtype — raw String/u64 never leak (P2,
//! usp.md dam M2). Ulid-based ids serialize as their canonical 26-char Crockford base32
//! string (never as arrays), so canonical JSON bytes are stable across runs.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::SchemaError;

/// Generates an id newtype wrapping `Arc<str>` with string serde and full ordering.
///
/// String ids (node names, tool names…) are content-addressed by value, so derive-style
/// `Arc<str>` ordering (lexicographic via `str`) keeps BTree collections deterministic.
macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Arc<str>);

        impl $name {
            pub fn new(value: impl Into<Arc<str>>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                String::deserialize(d).map($name::new)
            }
        }
    };
}

/// Generates an id newtype wrapping [`ulid::Ulid`], serialized as its canonical string.
macro_rules! ulid_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(ulid::Ulid);

        impl $name {
            /// Generates a fresh id from OS entropy. Only for ids that are NOT hash inputs
            /// (trace chain hashes never include ids generated per-run at different times).
            /// ulid 3.0 has no `Ulid::new()` — the random constructor is `generate()`
            /// (std feature, default-on).
            pub fn generate() -> Self {
                Self(ulid::Ulid::generate())
            }
            pub fn from_u128(value: u128) -> Self {
                Self(ulid::Ulid::from(value))
            }
            pub fn as_u128(&self) -> u128 {
                self.0.into()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let text = String::deserialize(d)?;
                ulid::Ulid::from_string(&text)
                    .map(Self)
                    .map_err(|e| D::Error::custom(format!("invalid ULID `{text}`: {e}")))
            }
        }
    };
}

string_id!(
    /// Unique node id within a compiled plan.
    NodeId
);
string_id!(
    /// Payload schema reference (pre-gate schema validation key, SS-09 rule 1).
    SchemaId
);
string_id!(
    /// Agent role — BTreeMap key in budget envelopes; ordering must be stable.
    AgentRole
);
string_id!(
    /// Tool name in an allowlist (`BTreeSet` — serialization order fixed).
    ToolName
);
string_id!(
    /// Team marker only: data field, no RBAC/UI (charter §5).
    TeamId
);
string_id!(
    /// Model reference, e.g. `glm-5.3-flash`; must match the bathos model plan (SS-17).
    ModelRef
);

ulid_id!(
    /// Session identifier — stable across replay forks.
    SessionId
);
ulid_id!(
    /// Execution instance; every replay fork mints a new run id (TYPE-5).
    RunId
);
ulid_id!(
    /// Envelope id (TYPE-2 first field).
    EnvelopeId
);
ulid_id!(
    /// Envelope request-response linkage (TYPE-2).
    CorrelationId
);

/// Session-local monotonically increasing commit number (`--at step N` of replay, W3-3).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct CommitSeq(pub u64);

/// Topological wave number within a compiled graph.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct WaveIndex(pub u32);

/// 64-char lowercase hex SHA-256 digest; anything else is rejected at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Hex(String);

impl Sha256Hex {
    /// Parses and validates: exactly 64 lowercase hex chars, else [`SchemaError`].
    pub fn parse(value: impl Into<String>) -> Result<Self, SchemaError> {
        let value = value.into();
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Self(value))
        } else {
            Err(SchemaError::InvalidSha256(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Sha256Hex {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Hex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(raw).map_err(D::Error::custom)
    }
}

impl FromStr for Sha256Hex {
    type Err = SchemaError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_rejects_non_canonical_forms() {
        assert!(Sha256Hex::parse("a".repeat(64)).is_ok());
        // Uppercase hex is not the canonical form — rejected (TYPE-1 invariant).
        assert!(Sha256Hex::parse("A".repeat(64)).is_err());
        assert!(Sha256Hex::parse("g".repeat(64)).is_err());
        assert!(Sha256Hex::parse("a".repeat(63)).is_err());
    }

    #[test]
    fn ulid_ids_roundtrip_as_strings() {
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            id: SessionId,
        }
        let original = Wrapper {
            id: SessionId::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab),
        };
        let bytes = crate::canonical_bytes(&original);
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        let encoded = value["id"].as_str().expect("id serialized as string");
        // Canonical string form: 26 Crockford chars (no I/L/O/U) — never an array or
        // number — so canonical bytes stay stable across runs and languages.
        assert_eq!(encoded.len(), 26, "ulid string is 26 chars: {encoded}");
        assert!(
            encoded
                .chars()
                .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c)),
            "crockford alphabet only: {encoded}"
        );
        let back: Wrapper = serde_json::from_slice(&bytes).expect("roundtrip");
        assert_eq!(back.id, original.id);

        // Zero timestamp + random=1 encodes to the all-zero form ending in 1 — pins the
        // bit layout (48-bit ms timestamp << 80 | 80-bit random) against drift.
        let one: Wrapper = Wrapper {
            id: SessionId::from_u128(1),
        };
        assert_eq!(one.id.to_string(), "00000000000000000000000001");
    }
}
