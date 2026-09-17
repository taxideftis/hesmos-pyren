//! TRAIT-3 Gate framework — `check(GateCtx) -> GateVerdict` plus the one eventing rule
//! that makes judgments auditable: EVERY check leaves a gate.pass or gate.fail event
//! (SS-09 rule 3 — a judgment path without an event is forbidden, US-11 AC2).
//!
//! The trait's `check` is the contract signature verbatim. One extra method, `id`,
//! exists because gate.fail/pass events REQUIRE a `gate_id` attribute (US-11 AC1): the
//! id is inherent gate identity, not judgment input, so it lives beside `check` rather
//! than inside the ctx. [`run_checked`] is the ONLY sanctioned way to run a gate — it is
//! what turns a verdict into events, and skipping it would produce the judgmentless
//! progress the invariants forbid.
//!
//! Verdict→event mapping (TYPE-7): Pass → gate.pass; Retry → gate.fail WITH
//! retry-announcing attrs (attempt/attempts_left) — a retry is a recorded failure that
//! announces its bounded continuation, never silent progress; Reject → gate.fail.

use hesmos_core::{
    BudgetState, EventAttrs, EventKind, EventSink, GateVerdict, NodeId, PendingEvent, PolicySet,
    ReasonCode, SessionHandle,
};

use crate::lease::LeaseRegistry;

/// Gate phase (TRAIT-3): Pre = before execution, Post = after, G0 = the transfer check
/// on the handoff payload itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatePhase {
    Pre,
    Post,
    G0,
}

/// The judgment context. Field set is the TRAIT-3 `GateCtx` contract; the runner builds
/// one per boundary — gates never reach into the ledger or the engine (code-structure
/// §2: the runner conducts, gates judge).
pub struct GateCtx<'a> {
    pub session: &'a SessionHandle,
    pub node: NodeId,
    pub phase: GatePhase,
    pub envelope_in: Option<&'a hesmos_core::Envelope>,
    pub envelope_out: Option<&'a hesmos_core::Envelope>,
    pub contract: Option<&'a hesmos_core::HandoffContract>,
    pub policy: &'a PolicySet,
    /// Immutable spend snapshot taken by the runner just before the gate runs.
    pub budget_state: BudgetState,
    /// Retries already consumed on this node (0 = first attempt); feeds attempts_left.
    pub attempt: u8,
    pub lease: &'a LeaseRegistry,
}

impl<'a> GateCtx<'a> {
    /// The common skeleton — envelope/contract fields start absent and are set by the
    /// caller (struct update syntax) for the phase in question.
    pub fn base(
        session: &'a SessionHandle,
        node: NodeId,
        phase: GatePhase,
        policy: &'a PolicySet,
        budget_state: BudgetState,
        lease: &'a LeaseRegistry,
    ) -> Self {
        Self {
            session,
            node,
            phase,
            envelope_in: None,
            envelope_out: None,
            contract: None,
            policy,
            budget_state,
            attempt: 0,
            lease,
        }
    }

    /// Remaining retries under the policy budget — the value a Retry verdict announces.
    pub fn attempts_left(&self) -> u8 {
        self.policy.bounded_retry.saturating_sub(self.attempt)
    }
}

/// TRAIT-3 — a gate judges; it never executes anything and never writes events itself.
/// (`check` is the contract signature; `id` is the event-attribute identity — see the
/// module doc for why it exists.)
pub trait Gate {
    /// The built-in id from the TRAIT-3 `built_in_gates` table — emitted verbatim as the
    /// `gate_id` attribute.
    fn id(&self) -> &'static str;
    fn check(&self, ctx: &GateCtx) -> GateVerdict;
}

/// Runs a gate and emits its judgment event through PORT-1. The single sanctioned
/// execution path: calling `check` directly is how a judgmentless boundary would happen
/// by accident.
///
/// Panics never: the emitted attrs always satisfy the kind's required set, so
/// `PendingEvent::new` cannot fail here — its `Err` is a bug abort, not a runtime path.
pub fn run_checked(gate: &dyn Gate, ctx: &GateCtx, sink: &dyn EventSink) -> GateVerdict {
    run_checked_with(gate, ctx, sink, &[])
}

/// [`run_checked`] with extra event attributes — the escalation record (WP-P1e): the
/// 13-kind taxonomy has no dedicated escalation kind, so a bounded-retry exhaustion is
/// recorded as `escalated_to`/`escalation_reason` attributes on the exhausting
/// gate.fail (retry.rs: "the exhausting attempt IS the event").
pub fn run_checked_with(
    gate: &dyn Gate,
    ctx: &GateCtx,
    sink: &dyn EventSink,
    extra: &[(&str, serde_json::Value)],
) -> GateVerdict {
    let verdict = gate.check(ctx);
    let mut attrs = match &verdict {
        GateVerdict::Pass { score } => EventAttrs::new()
            .set("gate_id", gate.id())
            .set("score", *score),
        GateVerdict::Reject { reason_code, score } => fail_attrs(gate.id(), *reason_code, *score),
        GateVerdict::Retry {
            reason_code,
            score,
            attempts_left,
        } => fail_attrs(gate.id(), *reason_code, *score)
            .set("attempt", ctx.attempt)
            .set("attempts_left", *attempts_left),
    };
    for (key, value) in extra {
        attrs = attrs.set(*key, value.clone());
    }
    let kind = match verdict {
        GateVerdict::Pass { .. } => EventKind::GatePass,
        GateVerdict::Reject { .. } | GateVerdict::Retry { .. } => EventKind::GateFail,
    };
    let pending = PendingEvent::new(kind, Some(ctx.node.clone()), attrs)
        .expect("gate event attrs are complete by construction");
    sink.emit(pending);
    verdict
}

/// The gate.fail attribute triple (gate_id·reason_code·score — US-11 AC1).
fn fail_attrs(gate_id: &str, reason_code: ReasonCode, score: f32) -> EventAttrs {
    EventAttrs::new()
        .set("gate_id", gate_id)
        .set("reason_code", hesmos_core::code_to_static(reason_code))
        .set("score", score)
}

/// SS-09 rule 2 — chain trustworthiness is min(gate_score); averaging or summing is
/// forbidden. `None` = no gate scores recorded yet (no claim to make).
pub fn chain_min_score(scores: &[f32]) -> Option<f32> {
    scores.iter().copied().reduce(f32::min)
}

/// SS-12 rule 1 — "we are done" may be declared ONLY by a terminal node. The runner
/// consults this at a node's completion claim (StageSpec.is_terminal is its input); a
/// non-terminal declaration is a blocked violation, emitted as gate.fail by the caller.
pub fn terminal_declaration_verdict(is_terminal: bool, declares_done: bool) -> GateVerdict {
    if declares_done && !is_terminal {
        GateVerdict::Reject {
            reason_code: ReasonCode::GATE_REJECT,
            score: 0.0,
        }
    } else {
        GateVerdict::Pass { score: 1.0 }
    }
}

/// W3-2 (확정 각인) — the boundary-level verdict vocabulary, DERIVED, never stored as an
/// event attribute. A node/gate boundary is CONCERNS exactly when at least one gate
/// Retry occurred there and the boundary ultimately Passed; FAIL when it ultimately
/// Rejected (or the session Halted/Suspended — mapped by the summary consumer); PASS
/// otherwise. There is deliberately no `level=concern` attribute anywhere in the
/// taxonomy — a test in this module pins that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryVerdict {
    Pass,
    Concerns,
    Fail,
}

/// Derives one boundary's verdict: the FINAL verdict at the boundary plus whether any
/// Retry happened along the way. Counting is per boundary — a later node's clean Pass
/// never absorbs an earlier boundary's CONCERNS (the W3-2 scenario ④ boundary bug).
pub fn derive_boundary_verdict(retried: bool, final_verdict: &GateVerdict) -> BoundaryVerdict {
    match final_verdict {
        GateVerdict::Reject { .. } => BoundaryVerdict::Fail,
        GateVerdict::Pass { .. } if retried => BoundaryVerdict::Concerns,
        GateVerdict::Pass { .. } => BoundaryVerdict::Pass,
        // A Retry is never final at a boundary — the retry loop ends in Pass or Reject.
        // Reaching this arm means the caller derived a boundary with the loop still
        // open; it is not a verdict the vocabulary can express, so it maps to Fail
        // (conservative: an open loop has NOT passed).
        GateVerdict::Retry { .. } => BoundaryVerdict::Fail,
    }
}

/// W3-2 session-level mapping — the final `bathos gate verdict` record: FAIL is forced
/// by any FAIL boundary (critical Reject 존재 → 강제), else CONCERNS if any boundary
/// ended with a Retry-then-Pass, else PASS.
pub fn session_verdict(boundaries: &[BoundaryVerdict]) -> BoundaryVerdict {
    let mut level = BoundaryVerdict::Pass;
    for b in boundaries {
        match b {
            BoundaryVerdict::Fail => return BoundaryVerdict::Fail,
            BoundaryVerdict::Concerns => level = BoundaryVerdict::Concerns,
            BoundaryVerdict::Pass => {}
        }
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::LeaseRegistry;
    use hesmos_core::{BudgetEnvelope, BudgetState, PolicySet, SessionId, SessionState, Sha256Hex};
    use std::collections::BTreeMap;

    struct RecordingSink {
        events: std::sync::Mutex<Vec<(EventKind, EventAttrs)>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn kinds(&self) -> Vec<EventKind> {
            self.events
                .lock()
                .expect("lock")
                .iter()
                .map(|(k, _)| *k)
                .collect()
        }
        fn last_attrs(&self) -> EventAttrs {
            self.events
                .lock()
                .expect("lock")
                .last()
                .expect("event")
                .1
                .clone()
        }
    }
    impl EventSink for RecordingSink {
        fn emit(&self, e: PendingEvent) -> hesmos_core::TraceEvent {
            self.events
                .lock()
                .expect("lock")
                .push((e.kind, e.attrs.clone()));
            hesmos_core::TraceEvent {
                seq: 0,
                kind: e.kind,
                node: e.node,
                attrs: e.attrs,
                prev_hash: Sha256Hex::parse("0".repeat(64)).expect("hex"),
                hash: Sha256Hex::parse("0".repeat(64)).expect("hex"),
                ts: 0,
            }
        }
    }

    struct Always(GateVerdict);
    impl Gate for Always {
        fn id(&self) -> &'static str {
            "rubric"
        }
        fn check(&self, _ctx: &GateCtx) -> GateVerdict {
            self.0.clone()
        }
    }

    fn ctx() -> (SessionHandle, PolicySet, LeaseRegistry, BudgetState) {
        let session = SessionHandle {
            session_id: SessionId::from_u128(1),
            run_id: hesmos_core::RunId::from_u128(2),
            seed: 7,
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
        };
        (
            session,
            PolicySet::default(),
            LeaseRegistry::new(),
            BudgetState {
                session_spent: 0,
                session_warn_limit: None,
                session_suspend_limit: None,
                team_spent: 0,
                team_suspend_limit: None,
                agent_spent: 0,
                agent_suspend_limit: None,
            },
        )
    }

    fn gate_ctx<'a>(
        session: &'a SessionHandle,
        policy: &'a PolicySet,
        lease: &'a LeaseRegistry,
        bs: BudgetState,
        attempt: u8,
    ) -> GateCtx<'a> {
        let mut c = GateCtx::base(
            session,
            NodeId::new("n"),
            GatePhase::Post,
            policy,
            bs,
            lease,
        );
        c.attempt = attempt;
        c
    }

    /// AC1 — every verdict flavor produces an event, and gate.fail carries all three
    /// mandatory attrs (gate_id·reason_code·score). Pass omits reason_code.
    #[test]
    fn every_check_leaves_an_event_and_fail_carries_three_attrs() {
        let (session, policy, lease, bs) = ctx();
        let sink = RecordingSink::new();

        let v = run_checked(
            &Always(GateVerdict::Pass { score: 1.0 }),
            &gate_ctx(&session, &policy, &lease, bs, 0),
            &sink,
        );
        assert!(matches!(v, GateVerdict::Pass { .. }));
        assert_eq!(sink.kinds(), vec![EventKind::GatePass]);
        let a = sink.last_attrs();
        assert_eq!(a.get_str("gate_id"), Some("rubric"));
        assert!(
            a.get("reason_code").is_none(),
            "pass does not carry a reason"
        );

        run_checked(
            &Always(GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0,
            }),
            &gate_ctx(&session, &policy, &lease, bs, 0),
            &sink,
        );
        let a = sink.last_attrs();
        assert_eq!(sink.kinds(), vec![EventKind::GatePass, EventKind::GateFail]);
        assert_eq!(a.get_str("gate_id"), Some("rubric"));
        assert_eq!(a.get_str("reason_code"), Some("GATE_REJECT"));
        assert_eq!(a.get_f32("score"), Some(0.0));
    }

    /// TYPE-7 invariant — a Retry is recorded as gate.fail WITH retry-announcing attrs.
    #[test]
    fn retry_is_a_recorded_gate_fail_announcing_the_retry() {
        let (session, policy, lease, bs) = ctx();
        let sink = RecordingSink::new();
        let v = run_checked(
            &Always(GateVerdict::Retry {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.4,
                attempts_left: 1,
            }),
            &gate_ctx(&session, &policy, &lease, bs, 1),
            &sink,
        );
        assert!(matches!(
            v,
            GateVerdict::Retry {
                attempts_left: 1,
                ..
            }
        ));
        assert_eq!(sink.kinds(), vec![EventKind::GateFail]);
        let a = sink.last_attrs();
        assert_eq!(a.get_str("reason_code"), Some("GATE_REJECT"));
        assert_eq!(a.get_f32("score"), Some(0.4));
        assert_eq!(a.get_u64("attempt"), Some(1));
        assert_eq!(a.get_u64("attempts_left"), Some(1));
    }

    /// SS-09 rule 2 — chain validity is the minimum, never an average.
    #[test]
    fn chain_validity_is_min_gate_score() {
        assert_eq!(chain_min_score(&[0.9, 0.4, 1.0]), Some(0.4));
        assert_eq!(chain_min_score(&[1.0]), Some(1.0));
        assert_eq!(chain_min_score(&[]), None);
        assert_eq!(chain_min_score(&[0.5, 0.5]), Some(0.5));
    }

    /// SS-12 rule 1 — only terminal nodes may declare completion.
    #[test]
    fn terminal_declaration_blocked_off_terminal_nodes() {
        let ok = terminal_declaration_verdict(true, true);
        assert!(matches!(ok, GateVerdict::Pass { .. }));
        let blocked = terminal_declaration_verdict(false, true);
        assert!(matches!(
            blocked,
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0
            }
        ));
        // Not declaring is unproblematic on any node.
        assert!(matches!(
            terminal_declaration_verdict(false, false),
            GateVerdict::Pass { .. }
        ));
    }

    /// W3-2 scenarios ①②③④ — the CONCERNS derivation and its boundary accounting.
    #[test]
    fn w3_2_concerns_derivation_is_boundary_exact() {
        // ① Retry 발생 후 최종 Pass → CONCERNS.
        assert_eq!(
            derive_boundary_verdict(true, &GateVerdict::Pass { score: 1.0 }),
            BoundaryVerdict::Concerns
        );
        // ③ 무Retry 전 Pass → PASS.
        assert_eq!(
            derive_boundary_verdict(false, &GateVerdict::Pass { score: 1.0 }),
            BoundaryVerdict::Pass
        );
        // ② critical Reject 존재 → FAIL (session level forces it too).
        assert_eq!(
            derive_boundary_verdict(
                false,
                &GateVerdict::Reject {
                    reason_code: ReasonCode::GATE_REJECT,
                    score: 0.0
                }
            ),
            BoundaryVerdict::Fail
        );

        // ④ boundary exactness: a Retry-then-Pass boundary is counted at ITS boundary
        // only — a later clean node is a separate PASS boundary and the two do not merge.
        let boundaries = [
            derive_boundary_verdict(true, &GateVerdict::Pass { score: 1.0 }), // node A: CONCERNS
            derive_boundary_verdict(false, &GateVerdict::Pass { score: 1.0 }), // node B: PASS
        ];
        assert_eq!(boundaries[0], BoundaryVerdict::Concerns);
        assert_eq!(boundaries[1], BoundaryVerdict::Pass);
        assert_eq!(session_verdict(&boundaries), BoundaryVerdict::Concerns);

        // One FAIL anywhere forces the session FAIL — concerns cannot dilute it.
        let with_fail = [BoundaryVerdict::Concerns, BoundaryVerdict::Fail];
        assert_eq!(session_verdict(&with_fail), BoundaryVerdict::Fail);
        assert_eq!(session_verdict(&[]), BoundaryVerdict::Pass);
    }

    /// W3-2 각인 — no `level` attribute may exist on gate.pass (CONCERNS is derived, not
    /// stored; the P-N8 provisional value is dead). If someone re-introduces it, this
    /// fails before the drift reaches a trace.
    #[test]
    fn no_concern_level_attribute_exists_on_gate_pass() {
        for (kind, optional) in hesmos_core::OPTIONAL_ATTR_KEYS {
            if *kind == EventKind::GatePass {
                assert!(
                    !optional.contains(&"level"),
                    "gate.pass must never carry a level attr"
                );
            }
        }
        // And the required set does not smuggle it in either.
        for (kind, required) in hesmos_core::REQUIRED_ATTRS {
            if *kind == EventKind::GatePass {
                assert!(!required.contains(&"level"));
            }
        }
    }
}
