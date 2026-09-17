//! contract — the Pre HandoffContract gate (SS-06).
//!
//! The gate DELEGATES the matrix judgment to `contract_check::validate` — one judging
//! function feeds both this gate and the router's TRAIT-4 step 1, so the two can never
//! disagree about a contract. This module only translates a [`ContractJudgment`] into
//! the gate verdict vocabulary:
//!
//! - Accept / WarnProceed / PostGateFail → Pass (soft rows pass by definition; their
//!   consequences are realized downstream — post-gate failure handling and the route
//!   eventing respectively)
//! - Reject / RejectNoRetry → Reject (GATE_REJECT — the next node never starts)
//! - BoundedRetry → Retry on the shared bounded-retry path (SS-10 rule 3; score IS the
//!   measured confidence — the one fail flavor where a real number exists)

use hesmos_core::{ContractJudgment, GateVerdict, ReasonCode};

use crate::contract_check;
use crate::gate::{Gate, GateCtx};

pub struct ContractGate {
    /// The session's original task text — the validator's row-1 reference. Runner-owned
    /// (it comes from the plan), injected at construction; `GateCtx` has no field for it.
    session_task: String,
}

impl ContractGate {
    pub fn new(session_task: &str) -> Self {
        Self {
            session_task: session_task.to_string(),
        }
    }
}

impl Gate for ContractGate {
    fn id(&self) -> &'static str {
        "contract"
    }

    fn check(&self, ctx: &GateCtx) -> GateVerdict {
        let Some(contract) = ctx.contract else {
            // A Pre contract gate with no contract is a malformed boundary.
            return GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0,
            };
        };
        match contract_check::validate(contract, &self.session_task, ctx.policy.min_confidence) {
            ContractJudgment::Accept
            | ContractJudgment::WarnProceed { .. }
            | ContractJudgment::PostGateFail { .. } => GateVerdict::Pass { score: 1.0 },
            ContractJudgment::Reject { .. } | ContractJudgment::RejectNoRetry { .. } => {
                GateVerdict::Reject {
                    reason_code: ReasonCode::GATE_REJECT,
                    score: 0.0,
                }
            }
            ContractJudgment::BoundedRetry => {
                let confidence = contract.confidence.as_ref().map(|c| c.get()).unwrap_or(0.0);
                GateVerdict::Retry {
                    reason_code: ReasonCode::GATE_REJECT,
                    score: confidence,
                    attempts_left: ctx.attempts_left(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GatePhase;
    use crate::test_support::Fixture;
    use hesmos_core::{
        ArtifactRef, Assumption, Confidence, DoneCriteria, FailedApproach, NodeId, Payload,
        PermissionCap, SchemaId, Sha256Hex, SnapshotSlice,
    };
    use std::collections::BTreeSet;

    const TASK: &str = "ship the ledger migration";

    fn contract() -> hesmos_core::HandoffContract {
        hesmos_core::HandoffContract {
            goal_original: TASK.into(),
            goal_current: "migrate schema".into(),
            invariants: vec!["no data loss".into()],
            done_criteria: Some(DoneCriteria {
                items: vec!["migration applied".into()],
            }),
            artifacts: Some(vec![ArtifactRef {
                path: "out/migrated.sql".into(),
                sha256: Sha256Hex::parse("a".repeat(64)).expect("hex"),
            }]),
            failed_approaches: Some(vec![FailedApproach {
                approach: "in-place alter".into(),
                failed_reason: "locks".into(),
            }]),
            assumptions: Some(vec![Assumption {
                text: "pg 16".into(),
                confirmed: true,
            }]),
            confidence: Some(Confidence::new(0.9).expect("in range")),
            from_node: NodeId::new("migrate"),
            to_node: Some(NodeId::new("verify")),
            permission_cap: PermissionCap {
                tools: BTreeSet::new(),
                network_allow: BTreeSet::new(),
            },
            snapshot: SnapshotSlice {
                goal_original: TASK.into(),
                prev_contract_summary: "ok".into(),
                node_input: Payload {
                    schema_id: SchemaId::new("migrate.in.v1"),
                    json: serde_json::json!({}),
                },
                shared_knowledge: vec![],
            },
        }
    }

    #[test]
    fn accept_passes_and_reject_rejects() {
        let fx = Fixture::new();
        let gate = ContractGate::new(TASK);

        let good = contract();
        let mut ctx = fx.ctx(NodeId::new("verify"), GatePhase::Pre);
        ctx.contract = Some(&good);
        assert!(matches!(gate.check(&ctx), GateVerdict::Pass { score: 1.0 }));

        // Missing assumptions (matrix row 7) is a hard reject through the gate.
        let mut bad = contract();
        bad.assumptions = None;
        let mut ctx = fx.ctx(NodeId::new("verify"), GatePhase::Pre);
        ctx.contract = Some(&bad);
        assert!(matches!(
            gate.check(&ctx),
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0
            }
        ));
    }

    /// SS-10 rule 3 — below-floor confidence reuses the SAME bounded-retry path; the
    /// verdict announces the remaining budget computed from ctx.attempt + policy.
    #[test]
    fn below_floor_confidence_retries_via_shared_path() {
        let mut fx = Fixture::new();
        fx.policy.min_confidence = 0.6;
        let gate = ContractGate::new(TASK);

        let mut low = contract();
        low.confidence = Some(Confidence::new(0.59).expect("in range"));
        let mut ctx = fx.ctx(NodeId::new("verify"), GatePhase::Pre);
        ctx.attempt = 1; // one retry already spent
        ctx.contract = Some(&low);
        match gate.check(&ctx) {
            GateVerdict::Retry {
                reason_code: ReasonCode::GATE_REJECT,
                score,
                attempts_left,
            } => {
                assert_eq!(score, 0.59, "score is the measured confidence");
                assert_eq!(attempts_left, 1, "bounded_retry(2) - attempt(1)");
            }
            other => panic!("expected Retry, got {other:?}"),
        }

        // At the floor exactly: NOT below — passes (boundary).
        let mut at_floor = contract();
        at_floor.confidence = Some(Confidence::new(0.6).expect("in range"));
        let mut ctx = fx.ctx(NodeId::new("verify"), GatePhase::Pre);
        ctx.contract = Some(&at_floor);
        assert!(matches!(gate.check(&ctx), GateVerdict::Pass { score: 1.0 }));
    }

    #[test]
    fn missing_contract_is_rejected() {
        let fx = Fixture::new();
        let gate = ContractGate::new(TASK);
        let ctx = fx.ctx(NodeId::new("verify"), GatePhase::Pre);
        assert!(matches!(gate.check(&ctx), GateVerdict::Reject { .. }));
    }
}
