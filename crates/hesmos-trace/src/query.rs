//! AP-1/AP-6 structure projections (WP-P3a): reading a stored chain as STRUCTURE.
//!
//! Two consumers, one projection:
//! - `hesmos eval` (CLI-5) projects a fork's chain and compares it to the blessed
//!   golden sample — SS-22 rule 1: the comparison is structural (path · gate
//!   verdicts), never free text.
//! - `trace replay`'s reproduction summary (ui-spec §5.2) projects origin and fork
//!   and reports where their structures agree.
//!
//! The projection is RECONSTRUCTED from the event stream on every call and stored
//! nowhere (AP-6 — derived views keep no storage; only the golden sample lives in
//! `eval/suites/`, and it stores exactly this type serialized).
//!
//! Scope discipline: the projection names what SS-22 names — node path (start order)
//! and gate verdicts. Payloads, tokens, latencies and scores are deliberately NOT
//! structure: a float score drift with identical pass/fail shape is not a path or
//! verdict change, and byte-level identity is already S1's job (ADR-0006). A
//! gate.fail's `attempts_left` IS kept: a retryable fail and a terminal fail are
//! different verdict shapes, and pinning it makes the golden detect bounded-retry
//! policy drift too.

use hesmos_core::{EventKind, TraceEvent};
use serde::{Deserialize, Serialize};

/// Reported mismatch cap: the diff stays readable; `matches: false` is the verdict
/// and the list is the diagnosis, not an exhaustive dump.
pub const MAX_REPORTED_MISMATCHES: usize = 8;

/// One node of the execution path, in start order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeStep {
    pub node: String,
    pub wave: u64,
}

/// The pass/fail half of a gate verdict (SS-22 "게이트 판정" — no scores).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictStep {
    Pass,
    Fail,
}

/// One gate judgment, in event order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateStep {
    pub node: Option<String>,
    pub gate_id: String,
    pub verdict: VerdictStep,
    /// Terminal fail reason (GATE_REJECT etc.). Absent on passes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    /// Retryable-fail marker (remaining budget INCLUDING this grant). Absent on
    /// terminal fails — its presence/absence is part of the verdict shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts_left: Option<u64>,
}

/// The AP-6 projection of one session: path + gate verdicts. This exact type is what
/// `--bless` serializes into the golden sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStructure {
    pub nodes: Vec<NodeStep>,
    pub gates: Vec<GateStep>,
}

/// Projects the chain into its structure. Pure — the stream is not modified and
/// nothing is stored (AP-6: reconstruct on every read).
pub fn project_structure(events: &[TraceEvent]) -> SessionStructure {
    let mut nodes = Vec::new();
    let mut gates = Vec::new();
    for e in events {
        match e.kind {
            EventKind::NodeStart => nodes.push(NodeStep {
                node: e
                    .node
                    .as_ref()
                    .map(|n| n.as_str().to_string())
                    .unwrap_or_default(),
                wave: e.attrs.get_u64("wave").unwrap_or(0),
            }),
            EventKind::GatePass | EventKind::GateFail => gates.push(GateStep {
                node: e.node.as_ref().map(|n| n.as_str().to_string()),
                gate_id: e.attrs.get_str("gate_id").unwrap_or("?").to_string(),
                verdict: match e.kind {
                    EventKind::GatePass => VerdictStep::Pass,
                    _ => VerdictStep::Fail,
                },
                reason_code: match e.kind {
                    EventKind::GateFail => e.attrs.get_str("reason_code").map(String::from),
                    _ => None,
                },
                attempts_left: match e.kind {
                    EventKind::GateFail => e.attrs.get_u64("attempts_left"),
                    _ => None,
                },
            }),
            _ => {}
        }
    }
    SessionStructure { nodes, gates }
}

/// One structural difference, itemized for the CLI diff render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructureMismatch {
    NodeCount {
        expected: usize,
        actual: usize,
    },
    NodeStep {
        index: usize,
        expected: NodeStep,
        actual: NodeStep,
    },
    GateCount {
        expected: usize,
        actual: usize,
    },
    GateStep {
        index: usize,
        expected: GateStep,
        actual: GateStep,
    },
}

/// The structural comparison verdict. Element-wise even when counts differ, so the
/// diff shows WHERE the path or verdicts diverged, not just that they did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureComparison {
    pub matches: bool,
    pub mismatches: Vec<StructureMismatch>,
}

/// Compares an actual structure against the blessed expectation.
pub fn compare_structure(
    expected: &SessionStructure,
    actual: &SessionStructure,
) -> StructureComparison {
    let mut mismatches = Vec::new();
    if expected.nodes.len() != actual.nodes.len() {
        mismatches.push(StructureMismatch::NodeCount {
            expected: expected.nodes.len(),
            actual: actual.nodes.len(),
        });
    }
    for (i, (e, a)) in expected.nodes.iter().zip(actual.nodes.iter()).enumerate() {
        if e != a {
            mismatches.push(StructureMismatch::NodeStep {
                index: i,
                expected: e.clone(),
                actual: a.clone(),
            });
        }
    }
    if expected.gates.len() != actual.gates.len() {
        mismatches.push(StructureMismatch::GateCount {
            expected: expected.gates.len(),
            actual: actual.gates.len(),
        });
    }
    for (i, (e, a)) in expected.gates.iter().zip(actual.gates.iter()).enumerate() {
        if e != a {
            mismatches.push(StructureMismatch::GateStep {
                index: i,
                expected: e.clone(),
                actual: a.clone(),
            });
        }
    }
    mismatches.truncate(MAX_REPORTED_MISMATCHES);
    StructureComparison {
        matches: mismatches.is_empty(),
        mismatches,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::{EventAttrs, EventKind, NodeId, Sha256Hex};

    /// Builds a chain from a mini event script: (kind, node, attrs).
    fn chain(script: &[(EventKind, Option<&str>, EventAttrs)]) -> Vec<TraceEvent> {
        let mut prev = Sha256Hex::parse(crate::log::GENESIS_PREV_HASH).expect("genesis");
        let mut out = Vec::new();
        for (i, (kind, node, attrs)) in script.iter().enumerate() {
            let hash = hesmos_core::chain_hash(&prev, i as u64, *kind, None, attrs);
            out.push(TraceEvent {
                seq: i as u64,
                kind: *kind,
                node: node.map(NodeId::new),
                prev_hash: prev.clone(),
                hash: hash.clone(),
                attrs: attrs.clone(),
                ts: 0,
            });
            prev = hash;
        }
        out
    }

    fn start(node: &str, wave: u64) -> (EventKind, Option<&str>, EventAttrs) {
        (
            EventKind::NodeStart,
            Some(node),
            EventAttrs::new().set("wave", wave),
        )
    }

    fn pass<'a>(node: &'a str, gate: &'a str) -> (EventKind, Option<&'a str>, EventAttrs) {
        (
            EventKind::GatePass,
            Some(node),
            EventAttrs::new().set("gate_id", gate).set("score", 0.9f32),
        )
    }

    fn fail<'a>(
        node: &'a str,
        gate: &'a str,
        reason: &'a str,
    ) -> (EventKind, Option<&'a str>, EventAttrs) {
        (
            EventKind::GateFail,
            Some(node),
            EventAttrs::new()
                .set("gate_id", gate)
                .set("reason_code", reason)
                .set("score", 0.0f32),
        )
    }

    fn other() -> (EventKind, Option<&'static str>, EventAttrs) {
        // A non-structural event (llm.call): must NOT enter the projection.
        (
            EventKind::LlmCall,
            Some("n"),
            EventAttrs::new().set("tokens_in", 5u64),
        )
    }

    #[test]
    fn projection_is_path_and_verdicts_only() {
        let events = chain(&[start("a", 0), other(), pass("a", "g0"), start("b", 1)]);
        let s = project_structure(&events);
        assert_eq!(
            s.nodes,
            vec![
                NodeStep {
                    node: "a".into(),
                    wave: 0
                },
                NodeStep {
                    node: "b".into(),
                    wave: 1
                },
            ]
        );
        assert_eq!(
            s.gates,
            vec![GateStep {
                node: Some("a".into()),
                gate_id: "g0".into(),
                verdict: VerdictStep::Pass,
                reason_code: None,
                attempts_left: None,
            }]
        );
    }

    #[test]
    fn fail_verdict_carries_reason_and_retry_shape() {
        let events = chain(&[
            fail("a", "rubric.v1", "GATE_REJECT"),
            fail("a", "schema", "GATE_REJECT"),
        ]);
        let s = project_structure(&events);
        assert_eq!(s.gates[0].verdict, VerdictStep::Fail);
        assert_eq!(s.gates[0].reason_code.as_deref(), Some("GATE_REJECT"));
        assert_eq!(s.gates[0].attempts_left, None);
    }

    #[test]
    fn identical_structures_compare_clean() {
        let script = [start("a", 0), pass("a", "g0")];
        let a = project_structure(&chain(&script));
        let b = project_structure(&chain(&script));
        let cmp = compare_structure(&a, &b);
        assert!(cmp.matches);
        assert!(cmp.mismatches.is_empty());
    }

    #[test]
    fn changed_gate_verdict_is_a_mismatch() {
        let ok = project_structure(&chain(&[start("a", 0), pass("a", "g0")]));
        let regressed = project_structure(&chain(&[start("a", 0), fail("a", "g0", "GATE_REJECT")]));
        let cmp = compare_structure(&ok, &regressed);
        assert!(!cmp.matches, "a pass → fail flip is structural (S8)");
        assert!(matches!(
            cmp.mismatches[0],
            StructureMismatch::GateStep { index: 0, .. }
        ));
    }

    #[test]
    fn extra_node_and_reordered_path_are_mismatches() {
        let base = chain(&[start("a", 0), start("b", 1)]);
        let grown = chain(&[start("a", 0), start("x", 0), start("b", 1)]);
        let cmp = compare_structure(&project_structure(&base), &project_structure(&grown));
        assert!(!cmp.matches);
        assert!(cmp.mismatches.iter().any(|m| matches!(
            m,
            StructureMismatch::NodeCount {
                expected: 2,
                actual: 3
            }
        )));

        // Same steps in swapped order: counts agree, elementwise catches it.
        let swapped = chain(&[start("b", 1), start("a", 0)]);
        let cmp = compare_structure(&project_structure(&base), &project_structure(&swapped));
        assert!(!cmp.matches);
        assert!(matches!(
            cmp.mismatches[0],
            StructureMismatch::NodeStep { index: 0, .. }
        ));
    }

    #[test]
    fn mismatch_listing_is_capped() {
        // Owned gate names first: the tuple helpers borrow &str.
        let names_a: Vec<String> = (0..50).map(|i| format!("g{i}")).collect();
        let names_b: Vec<String> = (0..50).map(|i| format!("gate-{i}")).collect();
        let long: Vec<_> = names_a.iter().map(|g| pass("a", g)).collect();
        let mut other = long.clone();
        other[10] = fail("a", "g10", "GATE_REJECT");
        // Two chains differing at EVERY gate: the diff list caps at the constant.
        let a: Vec<_> = names_b.iter().map(|g| pass("a", g)).collect();
        let cmp = compare_structure(
            &project_structure(&chain(&a)),
            &project_structure(&chain(&other)),
        );
        assert!(!cmp.matches);
        assert_eq!(cmp.mismatches.len(), MAX_REPORTED_MISMATCHES);
    }

    /// The golden sample roundtrips: bless writes this serialization, eval reads it
    /// back — an unstable format would flag honest refactors as regressions.
    #[test]
    fn structure_serialization_roundtrips() {
        let s = project_structure(&chain(&[start("a", 0), fail("a", "g", "GATE_REJECT")]));
        let yaml = serde_yaml_ng::to_string(&s).expect("serialize");
        let back: SessionStructure = serde_yaml_ng::from_str(&yaml).expect("deserialize");
        assert_eq!(s, back);
    }
}
