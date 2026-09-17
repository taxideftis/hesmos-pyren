//! The 8 built-in gates (TRAIT-3 `built_in_gates`). Each module owns ONE gate; every
//! judgment is mechanical and deterministic (P1) — the rubric's semantic judging arrives
//! with the WP-P3b LLM judge, never before.
//!
//! Gate ids are the contract table's spellings ("G0", "permission", …) and are emitted
//! verbatim as the `gate_id` event attribute. Plans reference gates as `"<id>.<ver>"`
//! (e.g. `rubric.v1`); [`instantiate`] maps a reference to its built-in by the segment
//! before the first dot, case-insensitively, so plan spelling and event vocabulary stay
//! decoupled but total.
//!
//! Score convention: mechanical gates score 1.0 on pass and 0.0 on fail; only the rubric
//! produces intermediate scores (0.0..=1.0), because a threshold judgment with an
//! invented partial number is noise, not information.

pub mod budget;
pub mod contract;
pub mod done_criteria;
pub mod g0;
pub mod over_delegation;
pub mod permission;
pub mod rubric;
pub mod schema;

pub use budget::BudgetGate;
pub use contract::ContractGate;
pub use done_criteria::DoneCriteriaGate;
pub use g0::G0Gate;
pub use over_delegation::OverDelegationGate;
pub use permission::{
    GrantorAuthority, PermissionGate, cap_within_grantor, network_admitted, tool_admitted,
};
pub use rubric::RubricGate;
pub use schema::SchemaGate;

use crate::gate::Gate;

use hesmos_core::AgentProfile;

/// Runner-owned context the built-ins may need at instantiation (WP-P2a). Grows per
/// gate need; every built-in reads only what it judges and ignores the rest.
///
/// `grantor_profile` is the SS-19 grantor — the FROM node's AgentProfile whose
/// authority bounds the handoff contract's `permission_cap`. `None` at a permission
/// boundary is the profile-less composition CE-06 forbids; the gate exists to reject
/// it as the second defense line (AC1), so it is an Option, not a panic.
#[derive(Clone, Copy)]
pub struct GateDeps<'a> {
    /// The session task — the `contract` gate instantiation needs it (goal_original
    /// check).
    pub session_task: &'a str,
    /// The stage's over-delegation floor — the `over_delegation` gate instantiation.
    pub spawn_token_floor: u64,
    /// The boundary's grantor profile — the `permission` gate instantiation.
    pub grantor_profile: Option<&'a AgentProfile>,
}

/// Builds a built-in gate from a plan gate reference (`"rubric.v1"` → rubric).
///
/// Gate ids are the contract table's spellings; the segment before the first dot,
/// case-insensitively, maps to the built-in so plan spelling and event vocabulary stay
/// decoupled but total.
pub fn instantiate(plan_ref: &str, deps: &GateDeps<'_>) -> Option<Box<dyn Gate>> {
    let builtin = plan_ref
        .split('.')
        .next()
        .unwrap_or(plan_ref)
        .to_ascii_lowercase();
    match builtin.as_str() {
        "g0" => Some(Box::new(G0Gate)),
        "permission" => Some(Box::new(match deps.grantor_profile {
            Some(profile) => PermissionGate::new(GrantorAuthority::of(profile)),
            None => PermissionGate::unprofiled(),
        })),
        "budget" => Some(Box::new(BudgetGate)),
        "schema" => Some(Box::new(SchemaGate)),
        "contract" => Some(Box::new(ContractGate::new(deps.session_task))),
        "done_criteria" => Some(Box::new(DoneCriteriaGate)),
        "rubric" => Some(Box::new(RubricGate)),
        "over_delegation" => Some(Box::new(OverDelegationGate::new(deps.spawn_token_floor))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps<'a>() -> GateDeps<'a> {
        GateDeps {
            session_task: "task",
            spawn_token_floor: 1000,
            grantor_profile: None,
        }
    }

    /// The full built-in table resolves; an unknown reference does not (a plan naming a
    /// nonexistent gate is a composition error, not a silent pass-through).
    #[test]
    fn all_eight_builtins_instantiate_and_unknown_ref_is_none() {
        for r in [
            "G0",
            "g0.v1",
            "permission.v1",
            "budget.v1",
            "schema.v1",
            "contract.v1",
            "done_criteria.v1",
            "rubric.v1",
            "over_delegation.v1",
        ] {
            assert!(instantiate(r, &deps()).is_some(), "ref {r}");
        }
        assert!(instantiate("nonexistent.v1", &deps()).is_none());
    }

    /// SS-19 double defense: instantiating permission WITHOUT a grantor profile gives
    /// the unprofiled gate (reject), never a silently-passing seat.
    #[test]
    fn permission_without_profile_instantiates_the_rejecting_gate() {
        use crate::gate::GatePhase;
        use crate::test_support::Fixture;
        use hesmos_core::NodeId;

        let gate = instantiate("permission.v1", &deps()).expect("builtin");
        let fx = Fixture::new();
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Pre);
        assert!(matches!(
            gate.check(&ctx),
            hesmos_core::GateVerdict::Reject { .. }
        ));
    }
}
