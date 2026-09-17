//! hesmos-guard — judgment only (code-structure §1/§3).
//!
//! Owns gates and their verdicts, the TYPE-4 contract validator (field-absence matrix),
//! bounded retry, node leases, permission enforcement (SS-19) and the prompt-cache
//! invariant verifier (SS-18 core half). Forbidden here: LLM calls and direct event-file
//! writes — every verdict leaves through the PORT-1 [`hesmos_core::EventSink`] (P6).
//!
//! Modules: `contract_check` (WP-P1b) · `policy_set`/`gate`/`gates`/`retry`/`lease`
//! (WP-P1c) · `gates::permission` + `cache` (WP-P2a).

mod cache;
mod contract_check;
mod gate;
mod gates;
mod lease;
mod policy_set;
mod retry;

#[cfg(test)]
mod test_support;

// D-7: the public surface is the re-export list below; the modules themselves are private.
pub use cache::{CACHE_GATE_ID, CacheJudgment, PromptInvariant, verify_turn};
pub use contract_check::{
    SNAPSHOT_TOKEN_LIMIT, provisional_assumptions, snapshot_token_count, validate,
};
pub use gate::{
    BoundaryVerdict, Gate, GateCtx, GatePhase, chain_min_score, derive_boundary_verdict,
    run_checked, run_checked_with, session_verdict, terminal_declaration_verdict,
};
pub use gates::{
    BudgetGate, ContractGate, DoneCriteriaGate, G0Gate, GateDeps, GrantorAuthority,
    OverDelegationGate, PermissionGate, RubricGate, SchemaGate, cap_within_grantor, instantiate,
    network_admitted, tool_admitted,
};
/// The matrix judgment type lives in core (shared with the orchestrator's router via
/// the D-2 seam); re-exported here so P1b consumers keep their import path.
pub use hesmos_core::ContractJudgment;
pub use lease::{LeaseError, LeaseRegistry};
pub use policy_set::{PolicyError, check_consistency, effective, load, parse_str};
pub use retry::{RetryOutcome, RetryTracker, escalation_attrs};

/// Compile-time proof of the D-2 dependency direction: the crate reaches the core
/// verdict vocabulary it will emit as gate.pass/gate.fail from WP-P1c on.
#[cfg(test)]
mod p0a_wiring {
    #[test]
    fn verdict_vocabulary_is_reachable() {
        let v = hesmos_core::GateVerdict::Retry {
            reason_code: hesmos_core::ReasonCode::GATE_REJECT,
            score: 0.4,
            attempts_left: 2,
        };
        assert!(matches!(
            v,
            hesmos_core::GateVerdict::Retry {
                attempts_left: 2,
                ..
            }
        ));
    }
}

#[cfg(test)]
mod p1b_wiring {
    /// The validator surface is reachable through the crate root (consumer smoke).
    #[test]
    fn contract_validator_is_exported() {
        use hesmos_core::{ArtifactRef, DoneCriteria, NodeId, Payload, PermissionCap, SchemaId};
        use std::collections::BTreeSet;

        let c = hesmos_core::HandoffContract {
            goal_original: "t".into(),
            goal_current: "now".into(),
            invariants: vec!["i".into()],
            done_criteria: Some(DoneCriteria {
                items: vec!["d".into()],
            }),
            artifacts: Some(vec![ArtifactRef {
                path: "p".into(),
                sha256: hesmos_core::Sha256Hex::parse("b".repeat(64)).expect("hex"),
            }]),
            failed_approaches: None,
            assumptions: Some(vec![]),
            confidence: Some(hesmos_core::Confidence::new(1.0).expect("in range")),
            from_node: NodeId::new("a"),
            to_node: Some(NodeId::new("b")),
            permission_cap: PermissionCap {
                tools: BTreeSet::new(),
                network_allow: BTreeSet::new(),
            },
            snapshot: hesmos_core::SnapshotSlice {
                goal_original: "t".into(),
                prev_contract_summary: String::new(),
                node_input: Payload {
                    schema_id: SchemaId::new("s.v1"),
                    json: serde_json::json!({}),
                },
                shared_knowledge: vec![],
            },
        };
        // failed_approaches is None here on purpose: the smoke asserts the validator
        // surfaces through the crate root AND that the recommended-field row works.
        assert!(matches!(
            crate::validate(&c, "t", 0.5),
            crate::ContractJudgment::WarnProceed {
                field: "failed_approaches"
            }
        ));
    }
}
