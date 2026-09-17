//! TRAIT-4 HandoffRouter — contract validation, loop guards, and the RouteDecision,
//! in that fixed order.
//!
//! **The D-2 seam.** code-structure D-2 forbids orchestrator→guard dependencies, while
//! TRAIT-4 step 1 delegates contract validation to the guard's validator. The
//! resolution is dependency inversion: [`HandoffRouter`] consumes validation through the
//! [`ContractValidator`] trait below, phrased purely in core types ([`ContractJudgment`]
//! lives in core for exactly this reason). The concrete adapter — a 3-line impl that
//! calls `hesmos_guard::validate` — is wired by whichever crate composes the runner
//! (the `hesmos` binary from WP-P1e, integration tests today): "runner가 지휘" made
//! literal.
//!
//! **Decisions only, no events.** Route input/output eventing (TRAIT-4 invariant —
//! handoff.request on entry; handoff.accept for Accept, gate.fail for the other three)
//! is the runner's conducting job, same split as WP-P1a's engine: this module returns
//! the complete decision data (`contract_hash`, destination, reason code) and nothing
//! writes to a sink here.
//!
//! **Interior mutability.** The contract fixes `fn route(&self, …)`; the loop-guard
//! state behind it is [`RefCell`]-borrowed per call. Session execution is
//! single-runner-threaded by design (P1 determinism), so borrow conflicts cannot occur;
//! if a future runner goes concurrent, routing serializes at the runner first.
//!
//! Ping-pong rule (SS-11 rule 2): a proposed handoff A→B is REPETITIVE when the pair
//! {A,B} already occurs — in either direction — within the last `ping_pong_window`
//! (8) recorded handoffs. A forward-DAG execution can never satisfy this (edges never
//! repeat), so the guard fires exactly on control-flow recurrence. Cumulative rule:
//! recording handoff number `max_handoffs` (20) latches the halt — no handoff #21
//! exists, and after a latch EVERY route answers Halt (US-13 AC3: 신규 노드 시작 0건).

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};

use hesmos_core::{
    ContractJudgment, HandoffContract, NodeId, PolicySet, ReasonCode, RouteDecision,
};

/// The validation seam (see module doc). One method, core types only.
pub trait ContractValidator {
    fn validate(
        &self,
        contract: &HandoffContract,
        session_task: &str,
        confidence_floor: f32,
    ) -> ContractJudgment;
}

/// TRAIT-4 — decide where a handoff goes.
pub trait HandoffRouter {
    fn route(&self, contract: HandoffContract) -> RouteDecision;
}

/// Verdict of a loop-guard evaluation of a PROPOSED handoff (nothing recorded yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopGuardVerdict {
    Clear,
    Halt(ReasonCode),
}

/// Loop-guard state: the ping-pong window, the cumulative handoff count, and the halt
/// latch. Pure state — halting is a latch, not a throw (after a halt the answer stays
/// Halt forever; a session never routes again).
#[derive(Debug, Clone)]
pub struct LoopGuardState {
    max_handoffs: u32,
    ping_pong_window: u32,
    window: VecDeque<(NodeId, NodeId)>,
    total: u32,
    halted: Option<ReasonCode>,
}

impl LoopGuardState {
    pub fn new(policy: &PolicySet) -> Self {
        Self {
            max_handoffs: policy.max_handoffs,
            ping_pong_window: policy.ping_pong_window,
            window: VecDeque::new(),
            total: 0,
            halted: None,
        }
    }

    /// The latched halt reason, if the guard has already stopped the session.
    pub fn halt_reason(&self) -> Option<ReasonCode> {
        self.halted
    }

    /// Latches a halt permanently (SS-11 rule 3: after a halt, zero new nodes start).
    /// Called by the router when an evaluation halts — the detection itself stays pure.
    pub fn latch(&mut self, reason: ReasonCode) {
        if self.halted.is_none() {
            self.halted = Some(reason);
        }
    }

    /// Evaluates a proposed from→to handoff WITHOUT recording it. Order inside:
    /// an existing latch dominates, then the cumulative cap, then ping-pong.
    pub fn evaluate(&self, from: &NodeId, to: &NodeId) -> LoopGuardVerdict {
        if let Some(reason) = self.halted {
            return LoopGuardVerdict::Halt(reason);
        }
        if self.total >= self.max_handoffs {
            return LoopGuardVerdict::Halt(ReasonCode::MAX_HANDOFFS);
        }
        let repeated = self
            .window
            .iter()
            .any(|(a, b)| (a == from && b == to) || (a == to && b == from));
        if repeated {
            return LoopGuardVerdict::Halt(ReasonCode::REPETITIVE_HANDOFF);
        }
        LoopGuardVerdict::Clear
    }

    /// Records an ACCEPTED handoff: pushes into the window (evicting the oldest beyond
    /// the window size), counts it, and latches the halt on reaching the cumulative cap.
    pub fn record(&mut self, from: NodeId, to: NodeId) {
        if self.ping_pong_window == 0 {
            // A zero window disables recurrence detection (defensive against a policy
            // override of 0); the cap still applies.
            self.total += 1;
            if self.total >= self.max_handoffs {
                self.halted = Some(ReasonCode::MAX_HANDOFFS);
            }
            return;
        }
        if self.window.len() as u32 >= self.ping_pong_window {
            self.window.pop_front();
        }
        self.window.push_back((from, to));
        self.total += 1;
        if self.total >= self.max_handoffs && self.halted.is_none() {
            self.halted = Some(ReasonCode::MAX_HANDOFFS);
        }
    }
}

/// The contract-faithful router: validator → loop guards → decision, with bounded-retry
/// bookkeeping for below-confidence contracts (SS-10 rule 3 — the SAME budget, tracked
/// per sender node; exhaustion answers ReturnToSender and the runner escalates).
pub struct LoopGuardRouter<V: ContractValidator> {
    task: String,
    validator: V,
    policy: PolicySet,
    guards: RefCell<LoopGuardState>,
    retries: RefCell<BTreeMap<NodeId, u8>>,
}

impl<V: ContractValidator> LoopGuardRouter<V> {
    pub fn new(task: &str, validator: V, policy: PolicySet) -> Self {
        // Build the guard state BEFORE `policy` moves into the struct.
        let guards = LoopGuardState::new(&policy);
        Self {
            task: task.to_string(),
            validator,
            policy,
            guards: RefCell::new(guards),
            retries: RefCell::new(BTreeMap::new()),
        }
    }

    /// Read access for the runner/tests (e.g. asserting the halt latch stuck).
    pub fn loop_guards(&self) -> LoopGuardState {
        self.guards.borrow().clone()
    }
}

impl<V: ContractValidator> HandoffRouter for LoopGuardRouter<V> {
    fn route(&self, contract: HandoffContract) -> RouteDecision {
        // —— (1) delegate contract validation (TRAIT-4 step 1). ——
        let contract_hash = contract.contract_hash();
        match self
            .validator
            .validate(&contract, &self.task, self.policy.min_confidence)
        {
            ContractJudgment::Accept
            | ContractJudgment::WarnProceed { .. }
            | ContractJudgment::PostGateFail { .. } => {}
            ContractJudgment::Reject { .. } | ContractJudgment::RejectNoRetry { .. } => {
                // No next-node start (SS-06 rule 6); done_criteria absence rides the
                // same arm — its no-retry property was already settled by the matrix.
                return RouteDecision::ReturnToSender {
                    reason_code: ReasonCode::GATE_REJECT,
                    contract_hash,
                };
            }
            ContractJudgment::BoundedRetry => {
                // Below-floor confidence reuses the bounded path; per-sender budget,
                // exhaustion → return to sender (escalation event is the runner's).
                // `attempts_left` counts retries REMAINING INCLUDING this grant — the
                // same semantics GateVerdict::Retry announces (first retry reports the
                // full budget, exhaustion reports 0 and stops retrying).
                let mut retries = self.retries.borrow_mut();
                let spent = retries.entry(contract.from_node.clone()).or_insert(0);
                let consumed_before = *spent;
                *spent += 1;
                drop(retries);
                let attempts_left = self.policy.bounded_retry.saturating_sub(consumed_before);
                if attempts_left == 0 {
                    return RouteDecision::ReturnToSender {
                        reason_code: ReasonCode::GATE_REJECT,
                        contract_hash,
                    };
                }
                return RouteDecision::RetryBounded {
                    reason_code: ReasonCode::GATE_REJECT,
                    attempts_left,
                };
            }
        }

        // —— (2) loop guards on the proposed edge (TRAIT-4 step 2). ——
        let Some(to) = contract.to_node.clone() else {
            // A handoff with no destination is a malformed envelope: back to sender.
            return RouteDecision::ReturnToSender {
                reason_code: ReasonCode::GATE_REJECT,
                contract_hash,
            };
        };
        let mut guards = self.guards.borrow_mut();
        match guards.evaluate(&contract.from_node, &to) {
            LoopGuardVerdict::Halt(reason) => {
                // A Halt is terminal for the session — latch it so EVERY later route
                // answers Halt too (US-13 AC3: halt 후 신규 노드 시작 0건).
                guards.latch(reason);
                return RouteDecision::Halt {
                    reason_code: reason,
                };
            }
            LoopGuardVerdict::Clear => {}
        }

        // —— (3) the decision — accept and record (TRAIT-4 step 3). ——
        guards.record(contract.from_node.clone(), to.clone());
        RouteDecision::Accept { to, contract_hash }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::{PermissionCap, Sha256Hex};
    use std::collections::BTreeSet;

    /// Validator double: echoes a preset judgment (the runner's adapter has exactly this
    /// shape, calling hesmos_guard::validate instead).
    struct Fixed(ContractJudgment);
    impl ContractValidator for Fixed {
        fn validate(&self, _c: &HandoffContract, _t: &str, _f: f32) -> ContractJudgment {
            self.0.clone()
        }
    }

    fn contract(from: &str, to: &str) -> HandoffContract {
        use hesmos_core::{
            ArtifactRef, Assumption, Confidence, DoneCriteria, FailedApproach, Payload, SchemaId,
            SnapshotSlice,
        };
        HandoffContract {
            goal_original: "task".into(),
            goal_current: "step".into(),
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
                goal_original: "task".into(),
                prev_contract_summary: String::new(),
                node_input: Payload {
                    schema_id: SchemaId::new("s.v1"),
                    json: serde_json::json!({}),
                },
                shared_knowledge: vec![],
            },
        }
    }

    #[test]
    fn reject_decisions_precede_loop_guards() {
        // A REJECTED contract answers ReturnToSender without touching the loop-guard
        // state — TRAIT-4's order is validator first, guards second.
        let router = LoopGuardRouter::new(
            "task",
            Fixed(ContractJudgment::Reject {
                field: "invariants",
                detail: "empty".into(),
            }),
            PolicySet::default(),
        );
        let decision = HandoffRouter::route(&router, contract("a", "b"));
        assert!(matches!(
            decision,
            RouteDecision::ReturnToSender {
                reason_code: ReasonCode::GATE_REJECT,
                ..
            }
        ));
        // Proof it never reached the guards: nothing was recorded, nothing latched.
        assert_eq!(router.loop_guards().halt_reason(), None);
    }

    #[test]
    fn bounded_retry_budget_is_per_sender_and_exhausts() {
        let policy = PolicySet::default().apply(&hesmos_core::PolicyOverrides {
            bounded_retry: Some(2),
            ..Default::default()
        });
        let router = LoopGuardRouter::new("task", Fixed(ContractJudgment::BoundedRetry), policy);

        for expected_left in [2u8, 1] {
            match HandoffRouter::route(&router, contract("s", "t")) {
                RouteDecision::RetryBounded { attempts_left, .. } => {
                    assert_eq!(attempts_left, expected_left);
                }
                other => panic!("expected RetryBounded, got {other:?}"),
            }
        }
        // Exhausted → no further retry announcement; the runner escalates.
        assert!(matches!(
            HandoffRouter::route(&router, contract("s", "t")),
            RouteDecision::ReturnToSender { .. }
        ));
        // A different sender has its own budget.
        assert!(matches!(
            HandoffRouter::route(&router, contract("other", "t")),
            RouteDecision::RetryBounded {
                attempts_left: 2,
                ..
            }
        ));
    }

    #[test]
    fn missing_destination_returns_to_sender() {
        let mut c = contract("a", "b");
        c.to_node = None;
        let router = LoopGuardRouter::new(
            "task",
            Fixed(ContractJudgment::Accept),
            PolicySet::default(),
        );
        assert!(matches!(
            HandoffRouter::route(&router, c),
            RouteDecision::ReturnToSender {
                reason_code: ReasonCode::GATE_REJECT,
                ..
            }
        ));
    }

    /// Loop-guard internals: ping-pong either direction inside the window, the
    /// cumulative cap, and the latch that answers Halt forever after.
    #[test]
    fn guard_state_window_cap_and_latch() {
        let policy = PolicySet {
            max_handoffs: 3,
            ping_pong_window: 4,
            ..Default::default()
        };
        let mut g = LoopGuardState::new(&policy);

        g.record(NodeId::new("a"), NodeId::new("b"));
        assert_eq!(
            g.evaluate(&NodeId::new("b"), &NodeId::new("a")),
            LoopGuardVerdict::Halt(ReasonCode::REPETITIVE_HANDOFF),
            "immediate bounce"
        );
        assert_eq!(
            g.evaluate(&NodeId::new("b"), &NodeId::new("c")),
            LoopGuardVerdict::Clear
        );
        g.record(NodeId::new("b"), NodeId::new("c"));
        // Old pair still inside window 4, from the OTHER direction.
        assert_eq!(
            g.evaluate(&NodeId::new("c"), &NodeId::new("a")),
            LoopGuardVerdict::Clear
        );
        assert_eq!(
            g.evaluate(&NodeId::new("a"), &NodeId::new("b")),
            LoopGuardVerdict::Halt(ReasonCode::REPETITIVE_HANDOFF),
            "same-edge recurrence"
        );

        // Third handoff reaches the cap → latch MAX_HANDOFFS.
        g.record(NodeId::new("c"), NodeId::new("d"));
        assert_eq!(g.halt_reason(), Some(ReasonCode::MAX_HANDOFFS));
        // Latch answers Halt for EVERY new route — 신규 노드 시작 0건.
        assert_eq!(
            g.evaluate(&NodeId::new("d"), &NodeId::new("e")),
            LoopGuardVerdict::Halt(ReasonCode::MAX_HANDOFFS)
        );
    }
}
