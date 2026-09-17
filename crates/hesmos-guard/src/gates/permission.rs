//! permission — the Pre gate's SS-19 enforcement (WP-P2a; the WP-P1c seat realized).
//!
//! Three rules, all judged against the GRANTOR — the from-node whose boundary emits
//! the handoff contract:
//! 1. **Profile required** (SS-19 rule 1): a boundary with no grantor profile is the
//!    profile-less agent the gate exists to refuse — Reject (AC1's second defense
//!    line; CE-06 at compile is the first).
//! 2. **Cap ⊆ grantor authority** (SS-19 rule 2 / ERD 불변식4): the contract's
//!    `permission_cap` may only RESTRICT what the grantor already holds. A cap naming
//!    a tool or network target the grantor lacks is privilege amplification
//!    (confused deputy) — Reject. Union across a chain is unreachable by
//!    construction: every grant is checked against a subset of its grantor.
//! 3. **Allowlist-only** (SS-19 rule 3 / 표3 P7): [`tool_admitted`] and
//!    [`network_admitted`] are the judgments the PY-4 tool executor consults BEFORE
//!    running a call ("허용된 호출만 받는다") — this gate is that judgment's pre-gate
//!    author. Denylists are not representable: profiles carry `BTreeSet` allowlists.
//!
//! First boundary = bathos hooks + OS (SS-19 rule 4): NO in-process sandboxing lives
//! here or anywhere in the guard — this module judges, it never contains.

use std::collections::BTreeSet;

use hesmos_core::{AgentProfile, GateVerdict, PermissionCap, ReasonCode, ToolName};

use crate::gate::{Gate, GateCtx};

/// The grantor authority a gate instance judges against — the from-node profile's
/// two permission-bearing surfaces, cloned at instantiation (gates are `'static`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantorAuthority {
    pub tools: BTreeSet<ToolName>,
    pub network_allow: BTreeSet<String>,
}

impl GrantorAuthority {
    pub fn of(profile: &AgentProfile) -> Self {
        Self {
            tools: profile.tools.clone(),
            network_allow: profile.network.allow.clone(),
        }
    }
}

pub struct PermissionGate {
    /// `None` = the boundary has no profile — the state this gate rejects (AC1).
    grantor: Option<GrantorAuthority>,
}

impl PermissionGate {
    /// The profiled gate — the only form a compiled plan can reach through
    /// [`crate::instantiate`] with a present profile.
    pub fn new(grantor: GrantorAuthority) -> Self {
        Self {
            grantor: Some(grantor),
        }
    }

    /// The profile-less gate. Reachable only by a composition that bypasses compile
    /// (CE-06) — exactly the double-defense case: the boundary refuses to pass an
    /// agent whose authority is unknown, instead of assuming the safest-looking set.
    pub fn unprofiled() -> Self {
        Self { grantor: None }
    }
}

impl Gate for PermissionGate {
    fn id(&self) -> &'static str {
        "permission"
    }

    fn check(&self, ctx: &GateCtx) -> GateVerdict {
        let Some(grantor) = &self.grantor else {
            return reject();
        };
        // No contract = no cap evidence to judge. A permission boundary without the
        // contract is a composition bug — never pass unjudged.
        let Some(contract) = ctx.contract else {
            return reject();
        };
        if cap_within_grantor(grantor, &contract.permission_cap) {
            GateVerdict::Pass { score: 1.0 }
        } else {
            reject()
        }
    }
}

/// SS-19 rule 2: every granted surface must already belong to the grantor. Subset
/// checks only — there is deliberately no "merge authorities" helper to misuse.
pub fn cap_within_grantor(grantor: &GrantorAuthority, cap: &PermissionCap) -> bool {
    cap.tools.is_subset(&grantor.tools) && cap.network_allow.is_subset(&grantor.network_allow)
}

/// PY-4's pre-call judgment (SS-19 rule 3): the tool executor receives only calls
/// this answers true for. Allowlist membership, nothing else.
pub fn tool_admitted(profile: &AgentProfile, tool: &ToolName) -> bool {
    profile.tools.contains(tool)
}

/// PY-4's network judgment — allowlist-only by construction (표3 P7: a denylist
/// shell would be the violation, and no denylist type exists to reach for).
pub fn network_admitted(profile: &AgentProfile, target: &str) -> bool {
    profile.network.allow.contains(target)
}

fn reject() -> GateVerdict {
    GateVerdict::Reject {
        reason_code: ReasonCode::GATE_REJECT,
        score: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GatePhase;
    use crate::test_support::{Fixture, fixture_contract};
    use hesmos_core::NodeId;

    fn profile(tools: &[&str], network: &[&str]) -> AgentProfile {
        AgentProfile {
            role: hesmos_core::AgentRole::new("researcher"),
            model: hesmos_core::ModelRef::new("glm-5.3-flash"),
            temperature: None,
            tools: tools.iter().copied().map(ToolName::new).collect(),
            resource_limits: Default::default(),
            network: hesmos_core::NetworkScope {
                allow: network.iter().map(|s| s.to_string()).collect(),
            },
            spawn_token_floor: 0,
        }
    }

    fn cap(tools: &[&str], network: &[&str]) -> PermissionCap {
        PermissionCap {
            tools: tools.iter().copied().map(ToolName::new).collect(),
            network_allow: network.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A Pre boundary whose contract carries `c` — the shape the runner builds at
    /// every handoff. The contract local must outlive the ctx (the ctx borrows it).
    fn violating_ctx<'a>(
        fx: &'a Fixture,
        c: PermissionCap,
    ) -> (hesmos_core::HandoffContract, GateCtx<'a>) {
        let mut contract = fixture_contract();
        contract.permission_cap = c;
        let ctx = fx.ctx(NodeId::new("to"), GatePhase::Pre);
        (contract, ctx)
    }

    fn assert_reject(v: GateVerdict) {
        assert!(matches!(
            v,
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0
            }
        ));
    }

    /// AC3 (규칙2): a cap within the grantor's own authority passes.
    #[test]
    fn cap_within_grantor_passes() {
        let grantor = GrantorAuthority::of(&profile(&["web.search"], &["api.example.com"]));
        assert!(cap_within_grantor(&grantor, &cap(&["web.search"], &[])));
        assert!(cap_within_grantor(
            &grantor,
            &cap(&[], &["api.example.com"])
        ));
        assert!(cap_within_grantor(&grantor, &cap(&[], &[])));
    }

    /// AC3 (규칙2): a cap naming ANY surface the grantor lacks is privilege
    /// amplification — the confused-deputy path this gate exists to close.
    #[test]
    fn cap_exceeding_grantor_is_rejected() {
        let grantor = GrantorAuthority::of(&profile(&["web.search"], &[]));
        assert!(
            !cap_within_grantor(&grantor, &cap(&["write.file"], &[])),
            "tool beyond the grantor = 권한 상승"
        );
        assert!(
            !cap_within_grantor(&grantor, &cap(&[], &["internal.db"])),
            "network target beyond the grantor = 권한 상승"
        );
    }

    /// AC2 (규칙3): the PY-4 judgment — allowlist membership only.
    #[test]
    fn allowlist_judgments_admit_only_listed_surfaces() {
        let p = profile(&["web.search"], &["api.example.com"]);
        assert!(tool_admitted(&p, &ToolName::new("web.search")));
        assert!(!tool_admitted(&p, &ToolName::new("write.file")));
        assert!(network_admitted(&p, "api.example.com"));
        assert!(!network_admitted(&p, "evil.example.net"));
    }

    /// AC1 second line: a profile-less boundary never passes — the pre-gate defense
    /// behind CE-06's compile-time rejection. Even a compliant cap changes nothing:
    /// unknown authority is refused, not assumed minimal.
    #[test]
    fn unprofiled_gate_rejects() {
        let fx = Fixture::new();
        let (contract, ctx) = violating_ctx(&fx, cap(&["web.search"], &[]));
        let gate = PermissionGate::unprofiled();
        assert_reject(gate.check(&GateCtx {
            contract: Some(&contract),
            ..ctx
        }));
    }

    /// A contract absent at a permission boundary is unjudgable evidence — Reject,
    /// never a silent pass (the seat must not become a judgmentless boundary).
    #[test]
    fn missing_contract_is_rejected_not_passed() {
        let fx = Fixture::new();
        let gate = PermissionGate::new(GrantorAuthority::of(&profile(&["web.search"], &[])));
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Pre);
        assert_reject(gate.check(&ctx));
    }

    /// The evented pair (AC2/AC3): run_checked on this gate turns the violating cap
    /// into a gate.fail(reason_code) event — the recorded denial US-23 AC2 requires.
    #[test]
    fn violating_boundary_records_gate_fail_through_run_checked() {
        use crate::gate::run_checked;
        use hesmos_core::{EventAttrs, EventKind, EventSink, PendingEvent, Sha256Hex};

        struct RecordingSink(std::sync::Mutex<Vec<EventKind>>);
        impl EventSink for RecordingSink {
            fn emit(&self, e: PendingEvent) -> hesmos_core::TraceEvent {
                self.0.lock().expect("lock").push(e.kind);
                hesmos_core::TraceEvent {
                    seq: 0,
                    kind: e.kind,
                    node: e.node,
                    attrs: EventAttrs::new(),
                    prev_hash: Sha256Hex::parse("0".repeat(64)).expect("hex"),
                    hash: Sha256Hex::parse("0".repeat(64)).expect("hex"),
                    ts: 0,
                }
            }
        }

        let fx = Fixture::new();
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));
        let gate = PermissionGate::new(GrantorAuthority::of(&profile(&["web.search"], &[])));

        // Compliant cap → gate.pass.
        let (contract, ctx) = violating_ctx(&fx, cap(&["web.search"], &[]));
        let v = run_checked(
            &gate,
            &GateCtx {
                contract: Some(&contract),
                ..ctx
            },
            &sink,
        );
        assert!(matches!(v, GateVerdict::Pass { .. }));

        // write.file beyond the allowlist → gate.fail(GATE_REJECT) on the record.
        let (contract, ctx) = violating_ctx(&fx, cap(&["write.file"], &[]));
        let v = run_checked(
            &gate,
            &GateCtx {
                contract: Some(&contract),
                ..ctx
            },
            &sink,
        );
        assert_reject(v);
        assert_eq!(
            sink.0.into_inner().expect("kinds"),
            vec![EventKind::GatePass, EventKind::GateFail]
        );
    }
}
