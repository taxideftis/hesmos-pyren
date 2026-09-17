//! TYPE-4 HandoffContract — the contract object; no node may start without one (P3).
//!
//! Body: exactly the 8 fields of 표 6. Envelope: routing metadata. The two are kept in
//! one struct because they travel together, but [`HandoffContract::body`] restricts the
//! hash input to the 8 body fields: `contract_hash = sha256(canonical(body))` (TYPE-4
//! invariant 4). Field-absence judgments (T4 oracle) are the guard's job (WP-P1b); this
//! schema makes absence representable exactly where the matrix needs it (`Option` fields)
//! and impossible everywhere else.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::envelope::Payload;
use crate::error::SchemaError;
use crate::ids::{NodeId, Sha256Hex, ToolName};

/// Machine-readable completion criteria. Absence at the wire level is `reject_no_retry`
/// (표 6); an empty list is judged the same way by the validator (WP-P1b).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoneCriteria {
    pub items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub path: String,
    pub sha256: Sha256Hex,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedApproach {
    pub approach: String,
    pub failed_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assumption {
    pub text: String,
    /// `false` = provisional — must keep its "provisional" marking downstream; it may
    /// never propagate as an established fact (US-10 AC3).
    pub confirmed: bool,
}

/// Confidence in 0.0..=1.0. NaN is rejected — an unordered score would silently pass any
/// threshold comparison and corrupt bounded-retry decisions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Confidence(f32);

impl Confidence {
    pub fn new(value: f32) -> Result<Self, SchemaError> {
        if (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(SchemaError::ConfidenceOutOfRange(value))
        }
    }

    pub fn get(&self) -> f32 {
        self.0
    }
}

/// Node-scoped permission ceiling shipped with the contract (SS-19 rule 2). Enforcement
/// (cap ⊆ from-node profile) is WP-P2a; the schema just fixes the shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionCap {
    pub tools: BTreeSet<ToolName>,
    /// Network allowlist — empty means no network. Allowlist-only; denylists are a
    /// P7 violation (표 3).
    pub network_allow: BTreeSet<String>,
}

/// One knowledge entry passed inside the snapshot (permission-filtered upstream, §5.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeEntry {
    pub key: String,
    pub content: String,
}

/// The immutable snapshot + slice a receiver is allowed to see (PT-3). Exactly these 4
/// fields — full transcripts / whole SharedContext re-injection is forbidden (SS-07 rule 2),
/// and `deny_unknown_fields` turns an attempt into a rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotSlice {
    pub goal_original: String,
    pub prev_contract_summary: String,
    pub node_input: Payload,
    pub shared_knowledge: Vec<KnowledgeEntry>,
}

/// Hash-input view of the 8 body fields, in declaration order. Borrowing keeps this a
/// zero-copy projection; serializing this struct (and only this struct) is what
/// `contract_hash` means — routing metadata must never move the hash.
#[derive(Serialize)]
pub struct ContractBody<'a> {
    pub goal_original: &'a String,
    pub goal_current: &'a String,
    pub invariants: &'a Vec<String>,
    pub done_criteria: &'a Option<DoneCriteria>,
    pub artifacts: &'a Option<Vec<ArtifactRef>>,
    pub failed_approaches: &'a Option<Vec<FailedApproach>>,
    pub assumptions: &'a Option<Vec<Assumption>>,
    pub confidence: &'a Option<Confidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffContract {
    // —— 표 6 body: 8 fields ——
    /// Immutable, byte-identical to the session task text; compiler-injected.
    pub goal_original: String,
    /// Absence → reject (matrix row 2).
    pub goal_current: String,
    /// Non-empty required; empty array is also reject (matrix row 3).
    pub invariants: Vec<String>,
    /// Absence → reject, no retry (matrix row 4).
    pub done_criteria: Option<DoneCriteria>,
    /// Absence → post-gate failure handling (matrix row 5).
    pub artifacts: Option<Vec<ArtifactRef>>,
    /// Recommended; absence → proceed with warning (matrix row 6).
    pub failed_approaches: Option<Vec<FailedApproach>>,
    /// Absence → reject (matrix row 7).
    pub assumptions: Option<Vec<Assumption>>,
    /// Below threshold → bounded retry via the shared path (matrix row 8). Absence is a
    /// required-field reject — the matrix only defines the below-threshold branch.
    pub confidence: Option<Confidence>,
    // —— routing envelope ——
    pub from_node: NodeId,
    /// Decided by a router node; None without router passage is not allowed (TYPE-4).
    pub to_node: Option<NodeId>,
    pub permission_cap: PermissionCap,
    pub snapshot: SnapshotSlice,
}

impl HandoffContract {
    /// Borrowed view of the 8 body fields for hashing.
    pub fn body(&self) -> ContractBody<'_> {
        ContractBody {
            goal_original: &self.goal_original,
            goal_current: &self.goal_current,
            invariants: &self.invariants,
            done_criteria: &self.done_criteria,
            artifacts: &self.artifacts,
            failed_approaches: &self.failed_approaches,
            assumptions: &self.assumptions,
            confidence: &self.confidence,
        }
    }

    /// `sha256(canonical(본문 8필드))` — the contract's integrity proof, recorded on
    /// handoff.request / handoff.accept events (표 7).
    pub fn contract_hash(&self) -> Sha256Hex {
        crate::canonical_sha256(&self.body())
    }

    /// Provisional assumptions keep their marking when rendered into a receiving prompt
    /// (US-10 AC3). This is the single sanctioned renderer so the "provisional" prefix
    /// cannot be forgotten at a call site.
    pub fn render_assumptions(&self) -> Vec<String> {
        self.assumptions
            .as_ref()
            .map(|list| {
                list.iter()
                    .map(|a| {
                        if a.confirmed {
                            a.text.clone()
                        } else {
                            format!("[PROVISIONAL] {}", a.text)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The field-absence matrix (표 6) judgments as a closed enum — variant names mirror the
/// matrix spellings (`snake_case` serde renames keep event/audit strings identical to the
/// contract doc).
///
/// Lives in core, not in hesmos-guard, because two D-2-separated components share it:
/// the validator (guard) PRODUCES it and the handoff router (orchestrator) CONSUMES it
/// (TRAIT-4 step 1 delegates to the validator). Core is the only type both may name.
/// The judgment LOGIC stays in hesmos-guard (`contract_check::validate`); this is data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractJudgment {
    /// All 8 body fields present and consistent — the next node may start (the router
    /// still applies its own loop guards afterwards, TRAIT-4 step 2).
    Accept,
    /// Matrix `reject` — contract returns to its sender; the next node never starts
    /// (SS-06 rule 6). `field` names the offending matrix row.
    Reject { field: &'static str, detail: String },
    /// Matrix `reject_no_retry` — done_criteria absent/empty. There is nothing to
    /// re-check against, so the retry path cannot exist (ADR-0002 강제조항).
    RejectNoRetry { field: &'static str },
    /// Matrix `post_gate_fail` — artifacts absent. The stage may run, but its output is
    /// handled as a post-gate failure instead of a normal completion.
    PostGateFail { field: &'static str },
    /// Matrix `warn_proceed` — failed_approaches absent (recommended field; forcing it
    /// would make first-ever tasks unrunnable, §5.3).
    WarnProceed { field: &'static str },
    /// Matrix `bounded_retry` — confidence below the floor; reuses the shared
    /// bounded-retry path, no separate loop (SS-10 rule 3).
    BoundedRetry,
}

impl ContractJudgment {
    /// `true` when the contract did NOT pass validation (no next-node start).
    pub fn is_rejection(&self) -> bool {
        matches!(
            self,
            ContractJudgment::Reject { .. }
                | ContractJudgment::RejectNoRetry { .. }
                | ContractJudgment::BoundedRetry
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Payload, Taint};

    fn payload() -> Payload {
        Payload {
            schema_id: crate::ids::SchemaId::new("node_input.v1"),
            json: serde_json::json!({ "q": "draft" }),
        }
    }

    fn complete_contract() -> HandoffContract {
        HandoffContract {
            goal_original: "Write a report".into(),
            goal_current: "Draft section 2".into(),
            invariants: vec!["cite sources".into()],
            done_criteria: Some(DoneCriteria {
                items: vec!["section 2 exists".into()],
            }),
            artifacts: Some(vec![ArtifactRef {
                path: "out/section2.md".into(),
                sha256: Sha256Hex::parse("a".repeat(64)).expect("hex"),
            }]),
            failed_approaches: Some(vec![FailedApproach {
                approach: "bullet points".into(),
                failed_reason: "too terse".into(),
            }]),
            assumptions: Some(vec![Assumption {
                text: "format is markdown".into(),
                confirmed: false,
            }]),
            confidence: Some(Confidence::new(0.7).expect("in range")),
            from_node: NodeId::new("draft"),
            to_node: Some(NodeId::new("verify")),
            permission_cap: PermissionCap {
                tools: BTreeSet::new(),
                network_allow: BTreeSet::new(),
            },
            snapshot: SnapshotSlice {
                goal_original: "Write a report".into(),
                prev_contract_summary: "n/a".into(),
                node_input: payload(),
                shared_knowledge: vec![],
            },
        }
    }

    /// Body-hash excludes routing metadata: flipping to_node must not move contract_hash.
    #[test]
    fn contract_hash_excludes_routing_envelope() {
        let mut a = complete_contract();
        let mut b = complete_contract();
        a.to_node = Some(NodeId::new("x"));
        b.to_node = Some(NodeId::new("y"));
        assert_eq!(a.contract_hash(), b.contract_hash());

        b.goal_current = "changed".into();
        assert_ne!(a.contract_hash(), b.contract_hash());
    }

    /// Extra field injection into the snapshot is rejected (US-10 AC1: the receiver gets
    /// exactly the 4 slice fields, nothing else).
    #[test]
    fn snapshot_extra_field_rejected() {
        let mut value = serde_json::to_value(complete_contract()).expect("json");
        let snapshot = value
            .get_mut("snapshot")
            .and_then(|s| s.as_object_mut())
            .expect("obj");
        snapshot.insert("full_transcript".into(), serde_json::json!("sneaky"));
        assert!(serde_json::from_value::<HandoffContract>(value).is_err());
    }

    /// Provisional assumptions keep their marking through the sanctioned renderer (US-10 AC3).
    #[test]
    fn provisional_assumptions_stay_marked() {
        let rendered = complete_contract().render_assumptions();
        assert_eq!(rendered, vec!["[PROVISIONAL] format is markdown"]);
    }

    /// goal_original byte-identity is the validator's check, but the parse surface still
    /// round-trips the full contract byte-identically.
    #[test]
    fn contract_roundtrip_bytes_identical() {
        let bytes = crate::canonical_bytes(&complete_contract());
        let back: HandoffContract = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(crate::canonical_bytes(&back), bytes);
    }

    #[test]
    fn confidence_bounds_enforced() {
        assert!(Confidence::new(1.0).is_ok());
        assert!(Confidence::new(0.0).is_ok());
        assert!(Confidence::new(1.1).is_err());
        assert!(Confidence::new(-0.01).is_err());
        assert!(Confidence::new(f32::NAN).is_err());
        let _ = Taint::Clean; // schema types coexist; keeps the import honest
    }
}
