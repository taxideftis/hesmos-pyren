//! TYPE-3 TraceEvent — the observation unit of the event-sourced core.
//!
//! Taxonomy: 9 families / 13 concrete kinds (표 7) — the single vocabulary for logs and
//! screens. The kind-specific required-attribute table is enforced at the boundary when a
//! [`crate::sink::PendingEvent`] is constructed, so an event violating the taxonomy cannot
//! exist and append rejection (SS-02 rule 3) is a parse-time fact, not a runtime hope.
//!
//! Hash rule (ADR-0006): `hash = sha256(canonical(prev_hash, seq, kind, node, attrs))`.
//! `ts` is a stored field but NOT a hash input — wall-clock must never break byte
//! reproduction (S1). Consequence, documented on purpose: tampering with `ts` alone is
//! not detected by chain verification; the audited truth is the chain fields only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::SchemaError;
use crate::ids::{NodeId, Sha256Hex};

/// Explicit dotted renames (not `rename_all`): the vocabulary spelling is
/// `session.open`-style, which snake_case cannot produce. A test pins serde name ==
/// vocabulary for every variant, so the two can never drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    // 1 session
    #[serde(rename = "session.open")]
    SessionOpen,
    #[serde(rename = "session.close")]
    SessionClose,
    // 2 plan
    #[serde(rename = "plan.compiled")]
    PlanCompiled,
    // 3 gate
    #[serde(rename = "gate.pass")]
    GatePass,
    #[serde(rename = "gate.fail")]
    GateFail,
    // 4 node
    #[serde(rename = "node.start")]
    NodeStart,
    #[serde(rename = "node.stop")]
    NodeStop,
    // 5 handoff
    #[serde(rename = "handoff.request")]
    HandoffRequest,
    #[serde(rename = "handoff.accept")]
    HandoffAccept,
    // 6 llm / 7 tool
    #[serde(rename = "llm.call")]
    LlmCall,
    #[serde(rename = "tool.call")]
    ToolCall,
    // 8 budget
    #[serde(rename = "budget.event")]
    BudgetEvent,
    // 9 trace
    #[serde(rename = "trace.seal")]
    TraceSeal,
}

impl EventKind {
    /// Canonical spelling used by tokens.md §2.4 and the CLI vocabulary — identical to
    /// the serde rename below, asserted by a test.
    pub fn as_vocab(self) -> &'static str {
        match self {
            Self::SessionOpen => "session.open",
            Self::SessionClose => "session.close",
            Self::PlanCompiled => "plan.compiled",
            Self::GatePass => "gate.pass",
            Self::GateFail => "gate.fail",
            Self::NodeStart => "node.start",
            Self::NodeStop => "node.stop",
            Self::HandoffRequest => "handoff.request",
            Self::HandoffAccept => "handoff.accept",
            Self::LlmCall => "llm.call",
            Self::ToolCall => "tool.call",
            Self::BudgetEvent => "budget.event",
            Self::TraceSeal => "trace.seal",
        }
    }
}

/// Required attributes per kind (TYPE-3 `attrs_required`). Keys ending in `_opt` may be
/// absent — they are listed for vocabulary completeness, not enforcement.
pub const REQUIRED_ATTRS: &[(EventKind, &[&str])] = &[
    (EventKind::SessionOpen, &["session_id", "seed", "budget"]),
    (EventKind::SessionClose, &["session_id", "final_state"]),
    (EventKind::PlanCompiled, &["plan_hash", "node_count"]),
    (EventKind::GatePass, &["gate_id", "score"]),
    // All three mandatory (US-11 AC1) — pass omits reason_code, fail never does.
    (EventKind::GateFail, &["gate_id", "reason_code", "score"]),
    (EventKind::NodeStart, &["node_id", "agent_role", "wave"]),
    (
        EventKind::NodeStop,
        &["node_id", "agent_role", "wave", "stop_kind"],
    ),
    (EventKind::HandoffRequest, &["contract_hash", "from"]),
    (EventKind::HandoffAccept, &["contract_hash", "from", "to"]),
    (
        EventKind::LlmCall,
        &["provider", "model", "tokens_in", "tokens_out", "latency_ms"],
    ),
    (
        EventKind::ToolCall,
        &["tool_name", "ok", "latency_ms", "actor"],
    ),
    (EventKind::BudgetEvent, &["level", "spent", "remaining"]),
    (EventKind::TraceSeal, &["chain_head_hash"]),
];

/// Sanctioned optional attributes (TYPE-3 `attrs_optional` — W3-5 judge metadata).
/// Note the split contract: `judge.verdict` exists on GateFail only, never on GatePass.
pub const OPTIONAL_ATTR_KEYS: &[(EventKind, &[&str])] = &[
    (
        EventKind::GatePass,
        &[
            "judge.prompt_hash",
            "judge.temperature",
            "judge.model_version",
        ],
    ),
    (
        EventKind::GateFail,
        &[
            "judge.prompt_hash",
            "judge.temperature",
            "judge.model_version",
            "judge.verdict",
        ],
    ),
];

fn required_for(kind: EventKind) -> &'static [&'static str] {
    REQUIRED_ATTRS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, attrs)| *attrs)
        .unwrap_or(&[])
}

/// kind-ordered attribute map. `BTreeMap` keeps canonical key order deterministic; the
/// plain `serde_json::Value` shape keeps the schema forward-compatible with `judge.*`
/// additions without inventing an attr-type algebra here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventAttrs(pub BTreeMap<String, serde_json::Value>);

impl EventAttrs {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Builder-style setter.
    pub fn set(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }

    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.0.get(key)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.as_str())
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.0.get(key).and_then(|v| v.as_u64())
    }

    pub fn get_f32(&self, key: &str) -> Option<f32> {
        self.0.get(key).and_then(|v| v.as_f64()).map(|f| f as f32)
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.0.get(key).and_then(|v| v.as_bool())
    }

    /// Rejects when a kind-required attribute is missing (TYPE-3 invariant 2).
    pub fn validate(&self, kind: EventKind) -> Result<(), SchemaError> {
        let missing: Vec<String> = required_for(kind)
            .iter()
            .filter(|key| !self.0.contains_key(**key))
            .map(|key| (*key).to_string())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(SchemaError::MissingAttrs {
                kind: kind.as_vocab().to_string(),
                missing,
            })
        }
    }
}

/// The hash input tuple for [`chain_hash`]. Field order here IS the canonical order —
/// declared once so the chain formula has exactly one source.
#[derive(Serialize)]
struct ChainInput<'a> {
    prev_hash: &'a Sha256Hex,
    seq: u64,
    kind: EventKind,
    node: Option<&'a NodeId>,
    attrs: &'a EventAttrs,
}

/// `sha256(canonical(prev_hash, seq, kind, node, attrs))` — ts and storage paths are
/// excluded by construction (ADR-0006; the reason is non-obvious so it lives in the
/// module doc too: wall-clock independence is what makes S1 byte reproduction possible).
pub fn chain_hash(
    prev_hash: &Sha256Hex,
    seq: u64,
    kind: EventKind,
    node: Option<&NodeId>,
    attrs: &EventAttrs,
) -> Sha256Hex {
    crate::canonical_sha256(&ChainInput {
        prev_hash,
        seq,
        kind,
        node,
        attrs,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceEvent {
    /// Session-local, 0-based, gap-free monotonic sequence.
    pub seq: u64,
    pub kind: EventKind,
    pub node: Option<NodeId>,
    pub attrs: EventAttrs,
    pub prev_hash: Sha256Hex,
    pub hash: Sha256Hex,
    /// Stored observation time (unix millis). Never a hash input — see module doc.
    pub ts: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::CorrelationId;

    fn sample_event(ts: u64) -> TraceEvent {
        let attrs = EventAttrs::new()
            .set("gate_id", "rubric.v1")
            .set("reason_code", "GATE_REJECT")
            .set("score", 0.42f32);
        let prev = Sha256Hex::parse("a".repeat(64)).expect("hex");
        let hash = chain_hash(
            &prev,
            7,
            EventKind::GateFail,
            Some(&NodeId::new("n1")),
            &attrs,
        );
        TraceEvent {
            seq: 7,
            kind: EventKind::GateFail,
            node: Some(NodeId::new("n1")),
            attrs,
            prev_hash: prev,
            hash,
            ts,
        }
    }

    /// T2 fixture #6: `ts` is excluded from the hash input, so two observations of the
    /// same chain fact at different times carry the identical hash — this is the intended
    /// behavior that makes S1 byte reproduction hold, not a bug.
    #[test]
    fn ts_is_not_a_hash_input() {
        let e1 = sample_event(1_000);
        let e2 = sample_event(9_999_999);
        assert_eq!(e1.hash, e2.hash);
        assert_ne!(e1.ts, e2.ts);
    }

    /// Required attrs table: GateFail demands all three (US-11 AC1); GatePass must NOT
    /// demand reason_code (contract keeps the pass/fail split).
    #[test]
    fn gate_fail_requires_three_attrs() {
        let short = EventAttrs::new().set("gate_id", "g").set("score", 1.0f32);
        assert!(short.validate(EventKind::GateFail).is_err());
        assert!(short.validate(EventKind::GatePass).is_ok());

        let full = short.set("reason_code", "GATE_REJECT");
        assert!(full.validate(EventKind::GateFail).is_ok());
    }

    /// Roundtrip stays byte-identical (seq/attrs ordering fixed by BTree).
    #[test]
    fn trace_event_roundtrip_bytes_identical() {
        let bytes = crate::canonical_bytes(&sample_event(42));
        let back: TraceEvent = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(crate::canonical_bytes(&back), bytes);
    }

    /// serde rename equals the shared vocabulary spelling — logs and screens can never
    /// drift apart (project-context §4).
    #[test]
    fn serde_names_equal_vocabulary() {
        for kind in [
            EventKind::SessionOpen,
            EventKind::SessionClose,
            EventKind::PlanCompiled,
            EventKind::GatePass,
            EventKind::GateFail,
            EventKind::NodeStart,
            EventKind::NodeStop,
            EventKind::HandoffRequest,
            EventKind::HandoffAccept,
            EventKind::LlmCall,
            EventKind::ToolCall,
            EventKind::BudgetEvent,
            EventKind::TraceSeal,
        ] {
            assert_eq!(
                serde_json::to_string(&kind).expect("serialize"),
                format!("\"{}\"", kind.as_vocab())
            );
        }
    }

    /// Ulid-based correlation ids serialize as strings inside attrs too.
    #[test]
    fn attrs_hold_schema_types() {
        let id = CorrelationId::from_u128(9);
        let attrs = EventAttrs::new().set("correlation_id", id.to_string());
        assert_eq!(
            attrs.get_str("correlation_id"),
            Some("00000000000000000000000009")
        );
    }

    /// Attrs are chain-hash inputs: every stored float must survive
    /// serialize → parse → serialize byte-identically, or chain verification breaks on
    /// perfectly honest events. serde_json needs `float_roundtrip` for this (workspace
    /// dep) — this test fails loudly if that feature is ever dropped.
    #[test]
    fn attrs_float_roundtrip_is_byte_stable() {
        // Scores from the concurrent-append T2 case: f32 values whose f64 widening is
        // the classic 1-ULP roundtrip trap.
        for i in 0..8u32 {
            let score = 0.5 - i as f32 / 100.0;
            let attrs = EventAttrs::new().set("score", score);
            let once = crate::canonical_bytes(&attrs);
            let parsed: EventAttrs = serde_json::from_slice(&once).expect("parse own bytes");
            assert_eq!(
                once,
                crate::canonical_bytes(&parsed),
                "float attrs must roundtrip byte-stably (score {score})"
            );
        }
    }
}
