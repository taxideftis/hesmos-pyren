//! WP-P1c integration seam — T5 (gate judgments leave events) and S4 (loop guards
//! halt, no infinite loops) driven through the PUBLIC surfaces of three crates at once:
//! hesmos-guard (gates/lease/retry), hesmos-orchestrator (router + loop-guard state),
//! hesmos-core (events, RouteDecision).
//!
//! The `GuardValidator` adapter at the top is the exact shape the WP-P1e runner wires:
//! guard's validator dressed as the orchestrator's core-typed `ContractValidator` seam
//! (D-2: neither crate may depend on the other, so the composition lives with the
//! consumer — "runner가 지휘" made literal).
//!
//! Event recording here is the runner's job too: every route/gate decision is turned
//! into REAL `PendingEvent`s (taxonomy-validated) before touching the sink, so a
//! decision that couldn't be evented fails this file loudly instead of silently
//! breaking TRAIT-4's "판정 입력·출력은 전부 이벤트" invariant in production.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use hesmos_core::{
    BudgetEnvelope, BudgetState, Confidence, ContractJudgment, DoneCriteria, EventAttrs, EventKind,
    EventSink, GateVerdict, HandoffContract, NodeId, PendingEvent, PolicySet, ReasonCode,
    RouteDecision, RunId, SessionHandle, SessionId, SessionState, Sha256Hex, TraceEvent,
};
use hesmos_guard::{
    BoundaryVerdict, Gate, GateCtx, GatePhase, LeaseRegistry, OverDelegationGate, RetryOutcome,
    RetryTracker, RubricGate, derive_boundary_verdict, escalation_attrs, run_checked,
    session_verdict, terminal_declaration_verdict,
};
use hesmos_orchestrator::{ContractValidator, HandoffRouter, LoopGuardRouter};

/// The D-2 seam adapter (runner-side; see module doc).
struct GuardValidator;

impl ContractValidator for GuardValidator {
    fn validate(
        &self,
        contract: &HandoffContract,
        session_task: &str,
        confidence_floor: f32,
    ) -> ContractJudgment {
        hesmos_guard::validate(contract, session_task, confidence_floor)
    }
}

/// Local GateCtx fixture — the runner's per-session composition, minimized.
struct Fixture {
    session: SessionHandle,
    policy: PolicySet,
    lease: LeaseRegistry,
    budget: BudgetState,
}

impl Fixture {
    fn new() -> Self {
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

    fn ctx<'a>(&'a self, node: NodeId, phase: GatePhase) -> GateCtx<'a> {
        GateCtx::base(
            &self.session,
            node,
            phase,
            &self.policy,
            self.budget,
            &self.lease,
        )
    }

    fn result_envelope(json: serde_json::Value) -> hesmos_core::Envelope {
        use hesmos_core::{
            CorrelationId, Envelope, EnvelopeId, EnvelopeKind, Payload, SchemaId, Taint,
        };
        Envelope {
            id: EnvelopeId::from_u128(1),
            from: NodeId::new("a"),
            to: NodeId::new("b"),
            kind: EnvelopeKind::Result,
            payload: Payload {
                schema_id: SchemaId::new("s.v1"),
                json,
            },
            correlation_id: CorrelationId::from_u128(2),
            taint: Taint::Clean,
        }
    }

    fn control_envelope(expected_tokens: u64) -> hesmos_core::Envelope {
        use hesmos_core::{
            CorrelationId, Envelope, EnvelopeId, EnvelopeKind, Payload, SchemaId, Taint,
        };
        Envelope {
            id: EnvelopeId::from_u128(3),
            from: NodeId::new("router"),
            to: NodeId::new("worker"),
            kind: EnvelopeKind::Control,
            payload: Payload {
                schema_id: SchemaId::new("control.v1"),
                json: serde_json::json!({ "action": "spawn", "expected_tokens": expected_tokens }),
            },
            correlation_id: CorrelationId::from_u128(2),
            taint: Taint::Clean,
        }
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// A valid-in-every-matrix-row contract (T4-proven shape) with a swappable envelope.
fn valid_contract(from: &str, to: &str) -> HandoffContract {
    const TASK: &str = "s4 task";
    use hesmos_core::{
        ArtifactRef, Assumption, FailedApproach, Payload, PermissionCap, SchemaId, SnapshotSlice,
    };
    HandoffContract {
        goal_original: TASK.into(),
        goal_current: "current".into(),
        invariants: vec!["i".into()],
        done_criteria: Some(DoneCriteria {
            items: vec!["d".into()],
        }),
        artifacts: Some(vec![ArtifactRef {
            path: "p".into(),
            sha256: Sha256Hex::parse("a".repeat(64)).expect("hex"),
        }]),
        failed_approaches: Some(vec![FailedApproach {
            approach: "a".into(),
            failed_reason: "r".into(),
        }]),
        assumptions: Some(vec![Assumption {
            text: "x".into(),
            confirmed: true,
        }]),
        confidence: Some(Confidence::new(0.9).expect("in range")),
        from_node: NodeId::new(from),
        to_node: Some(NodeId::new(to)),
        permission_cap: PermissionCap {
            tools: BTreeSet::new(),
            network_allow: BTreeSet::new(),
        },
        snapshot: SnapshotSlice {
            goal_original: TASK.into(),
            prev_contract_summary: String::new(),
            node_input: Payload {
                schema_id: SchemaId::new("s.v1"),
                json: serde_json::json!({}),
            },
            shared_knowledge: vec![],
        },
    }
}

/// A sink that keeps every event — good enough for assertions; storage is WP-P0b's.
/// `EventSink: Send + Sync`, so the buffer is a Mutex, not a RefCell.
#[derive(Default)]
struct VecSink {
    events: Mutex<Vec<TraceEvent>>,
}

impl VecSink {
    fn new() -> Self {
        Self::default()
    }
    fn kinds(&self) -> Vec<EventKind> {
        self.lock().iter().map(|e| e.kind).collect()
    }
    fn last(&self) -> TraceEvent {
        self.lock().last().expect("an event exists").clone()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<TraceEvent>> {
        self.events.lock().expect("unpoisoned")
    }
}

impl EventSink for VecSink {
    fn emit(&self, e: PendingEvent) -> TraceEvent {
        let mut events = self.lock();
        let seq = events.len() as u64;
        let prev = events
            .last()
            .map(|e| e.hash.clone())
            .unwrap_or_else(|| Sha256Hex::parse("0".repeat(64)).expect("genesis hex"));
        // Real chain-field computation — the same formula storage will verify later.
        let hash = hesmos_core::chain_hash(&prev, seq, e.kind, e.node.as_ref(), &e.attrs);
        let event = TraceEvent {
            seq,
            kind: e.kind,
            node: e.node,
            attrs: e.attrs,
            prev_hash: prev,
            hash,
            ts: 0,
        };
        events.push(event.clone());
        event
    }
}

/// The runner's route→event mapping (TRAIT-4 invariant): input always events as
/// handoff.request; Accept → handoff.accept; the other three decisions are expressible
/// as gate.fail with a router gate id. Asserts the mapping is taxonomy-total.
#[test]
fn s4_route_io_is_fully_eventable_for_every_decision() {
    let router = LoopGuardRouter::new("s4 task", GuardValidator, PolicySet::default());
    let sink = VecSink::new();

    // Input event (handoff.request: contract_hash + from) for the first route.
    let c = valid_contract("a", "b");
    let hash = c.contract_hash();
    let request = PendingEvent::new(
        EventKind::HandoffRequest,
        Some(c.from_node.clone()),
        EventAttrs::new()
            .set("contract_hash", hash.to_string())
            .set("from", c.from_node.to_string()),
    )
    .expect("handoff.request attrs complete");
    sink.emit(request);

    match HandoffRouter::route(&router, c) {
        RouteDecision::Accept { to, contract_hash } => {
            assert_eq!(to, NodeId::new("b"));
            assert_eq!(contract_hash, hash);
            let accept = PendingEvent::new(
                EventKind::HandoffAccept,
                Some(NodeId::new("a")),
                EventAttrs::new()
                    .set("contract_hash", contract_hash.to_string())
                    .set("from", "a")
                    .set("to", to.to_string()),
            )
            .expect("handoff.accept attrs complete");
            sink.emit(accept);
        }
        other => panic!("first valid route must accept, got {other:?}"),
    }
    assert_eq!(
        sink.kinds(),
        vec![EventKind::HandoffRequest, EventKind::HandoffAccept]
    );

    // Non-accept decisions map to gate.fail — verify each flavor survives the taxonomy.
    for reason in [
        ReasonCode::GATE_REJECT,
        ReasonCode::MAX_HANDOFFS,
        ReasonCode::REPETITIVE_HANDOFF,
    ] {
        PendingEvent::new(
            EventKind::GateFail,
            Some(NodeId::new("a")),
            EventAttrs::new()
                .set("gate_id", "router.v1")
                .set("reason_code", hesmos_core::code_to_static(reason))
                .set("score", 0.0f32),
        )
        .expect("router gate.fail is expressible in the taxonomy");
    }
}

/// S4 ① — ping-pong (A→B→A) inside window 8 halts with REPETITIVE_HANDOFF, and after
/// the halt NO new node starts: every subsequent route — even a fresh, legitimate pair —
/// answers Halt. Zero infinite loops, by latch.
#[test]
fn s4_ping_pong_halts_and_nothing_routes_after() {
    let router = LoopGuardRouter::new("s4 task", GuardValidator, PolicySet::default());

    assert!(matches!(
        HandoffRouter::route(&router, valid_contract("a", "b")),
        RouteDecision::Accept { .. }
    ));
    // The bounce B→A: pair {A,B} already in the window (either direction counts).
    match HandoffRouter::route(&router, valid_contract("b", "a")) {
        RouteDecision::Halt {
            reason_code: ReasonCode::REPETITIVE_HANDOFF,
        } => {}
        other => panic!("expected REPETITIVE_HANDOFF halt, got {other:?}"),
    }

    // halt 후 신규 노드 시작·handoff 0건 — the latch is absolute.
    for (from, to) in [("a", "c"), ("c", "d"), ("d", "e")] {
        assert!(matches!(
            HandoffRouter::route(&router, valid_contract(from, to)),
            RouteDecision::Halt { .. }
        ));
    }
    assert_eq!(
        router.loop_guards().halt_reason(),
        Some(ReasonCode::REPETITIVE_HANDOFF)
    );
}

/// S4 ② — the cumulative cap: handoff #20 latches MAX_HANDOFFS; the 21st is refused.
#[test]
fn s4_cumulative_twenty_halts() {
    let router = LoopGuardRouter::new("s4 task", GuardValidator, PolicySet::default());

    // 20 accepted handoffs over a linear chain of distinct pairs (never a repeat).
    for i in 0..20u32 {
        let (from, to) = (format!("n{i}"), format!("n{}", i + 1));
        match HandoffRouter::route(&router, valid_contract(&from, &to)) {
            RouteDecision::Accept { .. } => {}
            other => panic!("handoff {i} must accept, got {other:?}"),
        }
    }
    assert_eq!(
        router.loop_guards().halt_reason(),
        Some(ReasonCode::MAX_HANDOFFS),
        "reaching 20 latches the halt"
    );
    // 21st handoff: refused — and everything after it, forever.
    assert!(matches!(
        HandoffRouter::route(&router, valid_contract("n20", "n21")),
        RouteDecision::Halt {
            reason_code: ReasonCode::MAX_HANDOFFS
        }
    ));
    assert!(matches!(
        HandoffRouter::route(&router, valid_contract("x", "y")),
        RouteDecision::Halt { .. }
    ));
}

/// S4 ③ — leases block: an un-licensed access is judged Reject and its block is
/// RECORDED (gate.fail). The access gate shown here is the enforcement shape the P2a
/// permission gate will formalize; the registry semantics are already guard's.
#[test]
fn s4_lease_blocks_unlicensed_access_with_event() {
    let mut lease = LeaseRegistry::new();
    let writer = NodeId::new("writer");
    let reviewer = NodeId::new("reviewer");
    lease.acquire("data/report.csv", &writer).expect("grant");

    struct LeaseAccessGate {
        resource: String,
    }
    impl Gate for LeaseAccessGate {
        fn id(&self) -> &'static str {
            "permission"
        }
        fn check(&self, ctx: &GateCtx) -> GateVerdict {
            if ctx.lease.check(&self.resource, &ctx.node) {
                GateVerdict::Pass { score: 1.0 }
            } else {
                GateVerdict::Reject {
                    reason_code: ReasonCode::GATE_REJECT,
                    score: 0.0,
                }
            }
        }
    }

    let gate = LeaseAccessGate {
        resource: "data/report.csv".into(),
    };
    let fx = Fixture::new();
    let sink = VecSink::new();

    // The writer holds the lease: its check passes through the full ctx path. The
    // populated registry replaces the fixture's empty one (the runner wires the real
    // registry into every ctx it builds).
    let mut ctx = fx.ctx(writer.clone(), GatePhase::Pre);
    ctx.lease = &lease;
    assert!(matches!(
        run_checked(&gate, &ctx, &sink),
        GateVerdict::Pass { score: 1.0 }
    ));

    // The reviewer does not: blocked AND evented with all three attrs (AC6 차단 이벤트).
    let mut ctx = fx.ctx(reviewer.clone(), GatePhase::Pre);
    ctx.lease = &lease;
    let verdict = run_checked(&gate, &ctx, &sink);
    assert!(matches!(
        verdict,
        GateVerdict::Reject {
            reason_code: ReasonCode::GATE_REJECT,
            score: 0.0
        }
    ));
    let event = sink.last();
    assert_eq!(event.kind, EventKind::GateFail);
    assert_eq!(event.attrs.get_str("gate_id"), Some("permission"));
    assert_eq!(event.attrs.get_str("reason_code"), Some("GATE_REJECT"));
    assert_eq!(event.attrs.get_f32("score"), Some(0.0));
    assert_eq!(event.node.as_ref(), Some(&reviewer));
}

/// S4 ④ — over-delegation: a Control spawn under the node's floor is blocked with a
/// recorded gate.fail; a legal spawn passes (gate.pass).
#[test]
fn s4_over_delegation_blocked_with_event() {
    let fx = Fixture::new();
    let gate = OverDelegationGate::new(1_000);
    let sink = VecSink::new();

    let spawn = Fixture::control_envelope(400);
    let mut ctx = fx.ctx(NodeId::new("worker"), GatePhase::Pre);
    ctx.envelope_in = Some(&spawn);
    let verdict = run_checked(&gate, &ctx, &sink);
    assert!(matches!(verdict, GateVerdict::Reject { .. }));
    assert_eq!(sink.last().kind, EventKind::GateFail);
    assert_eq!(
        sink.last().attrs.get_str("gate_id"),
        Some("over_delegation")
    );

    let legal = Fixture::control_envelope(50_000);
    let mut ctx = fx.ctx(NodeId::new("worker"), GatePhase::Pre);
    ctx.envelope_in = Some(&legal);
    assert!(matches!(
        run_checked(&gate, &ctx, &sink),
        GateVerdict::Pass { score: 1.0 }
    ));
    assert_eq!(sink.last().kind, EventKind::GatePass);
}

/// S4 ⑤ — a non-terminal node's completion declaration is refused, with the refusal
/// recorded (gate.fail); the terminal node's declaration passes.
#[test]
fn s4_terminal_declaration_refused_off_terminal_nodes() {
    let fx = Fixture::new();
    let sink = VecSink::new();

    let ctx = fx.ctx(NodeId::new("mid_chain"), GatePhase::Post);
    let verdict = run_checked(
        &DeclarationGate {
            verdict: terminal_declaration_verdict(false, true),
        },
        &ctx,
        &sink,
    );
    assert!(matches!(verdict, GateVerdict::Reject { .. }));
    assert_eq!(sink.last().kind, EventKind::GateFail);
    assert_eq!(
        sink.last().attrs.get_str("reason_code"),
        Some("GATE_REJECT")
    );

    let ctx = fx.ctx(NodeId::new("final_node"), GatePhase::Post);
    let verdict = run_checked(
        &DeclarationGate {
            verdict: terminal_declaration_verdict(true, true),
        },
        &ctx,
        &sink,
    );
    assert!(matches!(verdict, GateVerdict::Pass { score: 1.0 }));
    assert_eq!(sink.last().kind, EventKind::GatePass);
}

/// Wraps a precomputed verdict so the refusal rides the standard eventing path —
/// the runner will construct it from StageSpec.is_terminal in WP-P1e.
struct DeclarationGate {
    verdict: GateVerdict,
}
impl Gate for DeclarationGate {
    fn id(&self) -> &'static str {
        "done_criteria"
    }
    fn check(&self, _ctx: &GateCtx) -> GateVerdict {
        self.verdict.clone()
    }
}

/// T5 — judgment eventing on the real rubric path: a Retry is a recorded gate.fail with
/// retry-announcing attrs, the retry budget is bounded, a later Pass makes the boundary
/// CONCERNS (W3-2 ①), and pass events carry no reason code (US-11 AC1's split).
#[test]
fn t5_retry_is_evented_bounded_and_derives_concerns() {
    let fx = Fixture::new();
    let sink = VecSink::new();
    let node = NodeId::new("n");
    let contract = valid_contract("a", "b");

    // Attempt 1: thin output → rubric 0.8 → Retry, recorded as gate.fail with attrs.
    let thin = Fixture::result_envelope(serde_json::json!({}));
    let mut ctx = fx.ctx(node.clone(), GatePhase::Post);
    ctx.envelope_out = Some(&thin);
    ctx.contract = Some(&contract);

    let mut tracker = RetryTracker::new(fx.policy.bounded_retry);
    let first = run_checked(&RubricGate, &ctx, &sink);
    let retried = match first {
        GateVerdict::Retry { attempts_left, .. } => {
            // First retry under budget 2: two retries remain, announced by the verdict.
            assert_eq!(attempts_left, 2);
            // Spending one leaves one.
            assert!(matches!(
                tracker.record(&node),
                RetryOutcome::Granted { attempts_left: 1 }
            ));
            // Escalation attrs exist for the exhaustion path — pin the shape.
            let attrs = escalation_attrs(&NodeId::new("parent"));
            assert_eq!(attrs[1].1, "bounded_retry_exhausted");
            true
        }
        other => panic!("expected Retry, got {other:?}"),
    };

    // Attempt 2: complete output → 1.0 → Pass.
    let full = Fixture::result_envelope(serde_json::json!({ "summary": "complete", "refs": [1] }));
    let mut ctx = fx.ctx(node.clone(), GatePhase::Post);
    ctx.envelope_out = Some(&full);
    ctx.contract = Some(&contract);
    let final_verdict = run_checked(&RubricGate, &ctx, &sink);
    assert!(matches!(final_verdict, GateVerdict::Pass { score: 1.0 }));

    // Boundary + session derivation (W3-2): Retry-then-Pass = CONCERNS at THIS boundary;
    // a separate clean boundary stays PASS and does not absorb it.
    assert_eq!(
        derive_boundary_verdict(retried, &final_verdict),
        BoundaryVerdict::Concerns
    );
    let boundaries = [
        BoundaryVerdict::Concerns,
        derive_boundary_verdict(false, &GateVerdict::Pass { score: 1.0 }),
    ];
    assert_eq!(session_verdict(&boundaries), BoundaryVerdict::Concerns);

    // Events: two rubric runs → fail (retry, with attrs) then pass.
    assert_eq!(sink.kinds(), vec![EventKind::GateFail, EventKind::GatePass]);
    let events = sink.lock();
    assert_eq!(events[0].attrs.get_str("gate_id"), Some("rubric"));
    assert_eq!(events[0].attrs.get_str("reason_code"), Some("GATE_REJECT"));
    assert!(
        events[0].attrs.get("attempt").is_some(),
        "retry announces itself"
    );
    assert!(events[0].attrs.get("attempts_left").is_some());
    assert!(
        events[1].attrs.get("reason_code").is_none(),
        "pass events carry no reason code"
    );
}
