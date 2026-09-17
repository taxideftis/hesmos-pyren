//! TYPE-2 Envelope — the typed node input/output envelope.
//!
//! Exactly seven fields; a `{role, content}` dict crossing a boundary is a schema
//! violation (SS-01 rule 2). `deny_unknown_fields` makes extra-field injection a parse
//! rejection instead of silently dropped data.

use serde::{Deserialize, Serialize};

use crate::ids::{CorrelationId, EnvelopeId, NodeId, SchemaId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub schema_id: SchemaId,
    pub json: serde_json::Value,
}

/// Envelope kinds — only router nodes may send `Control` (TYPE-2 invariant via 표 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeKind {
    Task,
    Result,
    Control,
    Knowledge,
}

/// Where contamination came from. Free-form `origin` (tool name / URL) — kept a plain
/// string because the source vocabulary belongs to the Python tool layer (WP-P2e), not
/// to this schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaintSource {
    pub origin: String,
}

/// Taint marking (PT-12). Transition is mandatory: any Envelope derived from a Tainted
/// one is Tainted (S5) — enforced by [`Envelope::derive`], the only sanctioned derivation
/// path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Taint {
    Clean,
    Tainted { source: TaintSource },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub id: EnvelopeId,
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EnvelopeKind,
    pub payload: Payload,
    pub correlation_id: CorrelationId,
    pub taint: Taint,
}

impl Envelope {
    /// The single sanctioned derivation path: taint propagates unconditionally.
    ///
    /// Callers supply the new `id` explicitly — generating one inside would introduce
    /// per-run entropy into data that later feeds commit records and golden comparisons.
    pub fn derive(
        &self,
        id: EnvelopeId,
        to: NodeId,
        kind: EnvelopeKind,
        payload: Payload,
        correlation_id: CorrelationId,
    ) -> Self {
        Self {
            id,
            from: self.to.clone(),
            to,
            kind,
            payload,
            correlation_id,
            taint: self.taint.clone(),
        }
    }

    /// G0 companion: canonical byte length is the deterministic transfer-size proxy used
    /// by the 8K-token budget check (SS-07 rule 3 — exact tokenization is out of scope;
    /// threshold math lives in the guard's G0 gate against `PolicySet::max_transfer_tokens`).
    pub fn transfer_bytes(&self) -> usize {
        crate::canonical_bytes(self).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Envelope {
        Envelope {
            id: EnvelopeId::from_u128(1),
            from: NodeId::new("a"),
            to: NodeId::new("b"),
            kind: EnvelopeKind::Task,
            payload: Payload {
                schema_id: SchemaId::new("draft.v1"),
                json: serde_json::json!({ "text": "hello" }),
            },
            correlation_id: CorrelationId::from_u128(2),
            taint: Taint::Tainted {
                source: TaintSource {
                    origin: "web.search".into(),
                },
            },
        }
    }

    /// TYPE-2 invariant: canonical roundtrip must be byte-identical, and field order is
    /// declaration order (id first) so hashes over envelopes are stable.
    #[test]
    fn envelope_roundtrip_bytes_identical() {
        let bytes = crate::canonical_bytes(&sample());
        let back: Envelope = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(crate::canonical_bytes(&back), bytes);
        assert!(bytes.starts_with(br#"{"id":"00000000000000000000000001""#));
    }

    /// Missing required field → rejected at the boundary, nothing constructed (US-01 AC2).
    #[test]
    fn missing_field_rejected() {
        let bytes = crate::canonical_bytes(&sample());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value.as_object_mut().expect("obj").remove("taint");
        let err = serde_json::from_value::<Envelope>(value).expect_err("must reject");
        assert!(
            err.to_string().contains("taint"),
            "names the violation: {err}"
        );
    }

    /// Extra fields are a schema violation, not silently dropped data (SS-01 rule 2).
    #[test]
    fn extra_field_rejected() {
        let bytes = crate::canonical_bytes(&sample());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value
            .as_object_mut()
            .expect("obj")
            .insert("role".into(), serde_json::json!("assistant"));
        assert!(serde_json::from_value::<Envelope>(value).is_err());
    }

    /// Taint transition is mandatory: derived envelopes inherit Tainted (S5).
    #[test]
    fn derive_propagates_taint() {
        let base = sample();
        let derived = base.derive(
            EnvelopeId::from_u128(3),
            NodeId::new("c"),
            EnvelopeKind::Result,
            Payload {
                schema_id: SchemaId::new("summary.v1"),
                json: serde_json::json!({"s": 1}),
            },
            CorrelationId::from_u128(2),
        );
        assert_eq!(derived.taint, base.taint);

        let clean = Envelope {
            taint: Taint::Clean,
            ..base
        };
        let derived_clean = clean.derive(
            EnvelopeId::from_u128(4),
            NodeId::new("c"),
            EnvelopeKind::Result,
            Payload {
                schema_id: SchemaId::new("summary.v1"),
                json: serde_json::json!({}),
            },
            CorrelationId::from_u128(2),
        );
        assert_eq!(derived_clean.taint, Taint::Clean);
    }

    /// Sanity-only: transfer size stays deterministic for identical envelopes.
    #[test]
    fn transfer_size_deterministic() {
        assert_eq!(sample().transfer_bytes(), sample().transfer_bytes());
    }
}
