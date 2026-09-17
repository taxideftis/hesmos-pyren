//! Shared unit-test fixtures for gate modules (compiled only under `cfg(test)`).
//! Keeps every gate test's context construction identical, so a GateCtx field change
//! surfaces as one compile error here instead of eight divergent fixture edits.

use std::collections::BTreeMap;

use hesmos_core::{
    BudgetEnvelope, BudgetState, PolicySet, RunId, SessionHandle, SessionId, SessionState,
    Sha256Hex,
};

use crate::gate::{GateCtx, GatePhase};
use crate::lease::LeaseRegistry;

/// The always-available session/policy/lease/budget quadruple.
pub struct Fixture {
    pub session: SessionHandle,
    pub policy: PolicySet,
    pub lease: LeaseRegistry,
    pub budget: BudgetState,
}

impl Fixture {
    /// A GateCtx borrowing this fixture, for the given node/phase. Envelope and
    /// contract fields start absent — tests set the ones the gate under test reads.
    pub fn ctx<'a>(&'a self, node: hesmos_core::NodeId, phase: GatePhase) -> GateCtx<'a> {
        GateCtx::base(
            &self.session,
            node,
            phase,
            &self.policy,
            self.budget,
            &self.lease,
        )
    }
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            session: SessionHandle {
                session_id: SessionId::from_u128(1),
                run_id: RunId::from_u128(2),
                seed: 42,
                plan_hash: Sha256Hex::parse("a".repeat(64)).expect("hex"),
                budget: BudgetEnvelope {
                    session_max_tokens: None,
                    team_max_tokens: None,
                    agent_max_tokens: BTreeMap::new(),
                    warn_pct: 80,
                    suspend_pct: 100,
                },
                state: SessionState::Running,
                team_id: None,
                fork_of: None,
                chain_head: None,
            },
            policy: PolicySet::default(),
            lease: LeaseRegistry::new(),
            budget: BudgetState {
                session_spent: 0,
                session_warn_limit: None,
                session_suspend_limit: None,
                team_spent: 0,
                team_suspend_limit: None,
                agent_spent: 0,
                agent_suspend_limit: None,
            },
        }
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// A fully-valid [`HandoffContract`] (all 8 body fields, correct in every matrix row) —
/// the happy-path fixture; tests mutate a clone to reach defect rows.
pub fn fixture_contract() -> hesmos_core::HandoffContract {
    use hesmos_core::{
        ArtifactRef, Assumption, Confidence, DoneCriteria, FailedApproach, NodeId, Payload,
        PermissionCap, SchemaId, SnapshotSlice,
    };
    use std::collections::BTreeSet;

    const TASK: &str = "fixture task";
    hesmos_core::HandoffContract {
        goal_original: TASK.into(),
        goal_current: "current step".into(),
        invariants: vec!["inv".into()],
        done_criteria: Some(DoneCriteria {
            items: vec!["criterion".into()],
        }),
        artifacts: Some(vec![ArtifactRef {
            path: "out/artifact".into(),
            sha256: Sha256Hex::parse("a".repeat(64)).expect("hex"),
        }]),
        failed_approaches: Some(vec![FailedApproach {
            approach: "approach".into(),
            failed_reason: "reason".into(),
        }]),
        assumptions: Some(vec![Assumption {
            text: "assumption".into(),
            confirmed: true,
        }]),
        confidence: Some(Confidence::new(0.9).expect("in range")),
        from_node: NodeId::new("from"),
        to_node: Some(NodeId::new("to")),
        permission_cap: PermissionCap {
            tools: BTreeSet::new(),
            network_allow: BTreeSet::new(),
        },
        snapshot: SnapshotSlice {
            goal_original: TASK.into(),
            prev_contract_summary: "summary".into(),
            node_input: Payload {
                schema_id: SchemaId::new("fixture.in.v1"),
                json: serde_json::json!({}),
            },
            shared_knowledge: vec![],
        },
    }
}
