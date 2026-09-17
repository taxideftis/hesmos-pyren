//! over_delegation — spawn blocking below the token floor (SS-12 rule 3).
//!
//! A Control envelope that spawns a subtask must declare its expected token cost; a
//! spawn cheaper than the node's `spawn_token_floor` is exactly the "delegation so small
//! it costs more to supervise than to do" anti-pattern, and the core blocks it in the
//! gate — not the prompt (SS-12 rule 3, PT-4).
//!
//! The declared amount is read from the Control envelope payload's `expected_tokens`
//! (number) member. That key is this gate's input convention (GateCtx is contract-fixed
//! and carries no spawn surface); a Control envelope WITHOUT the member is judged as no
//! spawn at all and passes — only a declared spawn below the floor is a violation.

use hesmos_core::{EnvelopeKind, GateVerdict, ReasonCode};

use crate::gate::{Gate, GateCtx};

pub struct OverDelegationGate {
    /// The node's floor — from `AgentProfile::spawn_token_floor` (plan-owned), injected
    /// at construction because GateCtx has no profile field.
    spawn_token_floor: u64,
}

impl OverDelegationGate {
    pub fn new(spawn_token_floor: u64) -> Self {
        Self { spawn_token_floor }
    }
}

impl Gate for OverDelegationGate {
    fn id(&self) -> &'static str {
        "over_delegation"
    }

    fn check(&self, ctx: &GateCtx) -> GateVerdict {
        let Some(envelope) = ctx.envelope_in else {
            return GateVerdict::Pass { score: 1.0 };
        };
        if envelope.kind != EnvelopeKind::Control {
            return GateVerdict::Pass { score: 1.0 };
        }
        let declared_spawn = envelope
            .payload
            .json
            .get("expected_tokens")
            .and_then(|v| v.as_u64());
        match declared_spawn {
            Some(expected) if expected < self.spawn_token_floor => GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0,
            },
            _ => GateVerdict::Pass { score: 1.0 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GatePhase;
    use crate::test_support::Fixture;
    use hesmos_core::{CorrelationId, Envelope, EnvelopeId, NodeId, Payload, SchemaId, Taint};

    fn control_envelope(expected_tokens: Option<u64>) -> Envelope {
        let mut json = serde_json::Map::new();
        json.insert("action".into(), serde_json::json!("spawn"));
        if let Some(t) = expected_tokens {
            json.insert("expected_tokens".into(), serde_json::json!(t));
        }
        Envelope {
            id: EnvelopeId::from_u128(1),
            from: NodeId::new("router"),
            to: NodeId::new("worker"),
            kind: EnvelopeKind::Control,
            payload: Payload {
                schema_id: SchemaId::new("control.v1"),
                json: serde_json::Value::Object(json),
            },
            correlation_id: CorrelationId::from_u128(2),
            taint: Taint::Clean,
        }
    }

    fn task_envelope() -> Envelope {
        Envelope {
            id: EnvelopeId::from_u128(3),
            from: NodeId::new("router"),
            to: NodeId::new("worker"),
            kind: EnvelopeKind::Task,
            payload: Payload {
                schema_id: SchemaId::new("task.v1"),
                json: serde_json::json!({ "brief": "do the work" }),
            },
            correlation_id: CorrelationId::from_u128(2),
            taint: Taint::Clean,
        }
    }

    #[test]
    fn undersized_spawn_blocked_legal_spawn_passes() {
        let fx = Fixture::new();
        let gate = OverDelegationGate::new(1_000);

        // 400 expected tokens < 1_000 floor → blocked (AC6 스폰 차단).
        let small = control_envelope(Some(400));
        let mut ctx = fx.ctx(NodeId::new("worker"), GatePhase::Pre);
        ctx.envelope_in = Some(&small);
        assert!(matches!(
            gate.check(&ctx),
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0
            }
        ));

        // Exactly at the floor is NOT below it (boundary).
        let exact = control_envelope(Some(1_000));
        let mut ctx = fx.ctx(NodeId::new("worker"), GatePhase::Pre);
        ctx.envelope_in = Some(&exact);
        assert!(matches!(gate.check(&ctx), GateVerdict::Pass { score: 1.0 }));

        // A generous spawn passes.
        let big = control_envelope(Some(50_000));
        let mut ctx = fx.ctx(NodeId::new("worker"), GatePhase::Pre);
        ctx.envelope_in = Some(&big);
        assert!(matches!(gate.check(&ctx), GateVerdict::Pass { score: 1.0 }));
    }

    #[test]
    fn non_control_envelopes_and_undeclared_spawns_pass() {
        let fx = Fixture::new();
        let gate = OverDelegationGate::new(1_000);

        // Task envelopes are never spawns.
        let task = task_envelope();
        let mut ctx = fx.ctx(NodeId::new("worker"), GatePhase::Pre);
        ctx.envelope_in = Some(&task);
        assert!(matches!(gate.check(&ctx), GateVerdict::Pass { score: 1.0 }));

        // Control without a declared cost: no spawn to judge.
        let control = control_envelope(None);
        let mut ctx = fx.ctx(NodeId::new("worker"), GatePhase::Pre);
        ctx.envelope_in = Some(&control);
        assert!(matches!(gate.check(&ctx), GateVerdict::Pass { score: 1.0 }));

        // No envelope at all: nothing to block.
        let ctx = fx.ctx(NodeId::new("worker"), GatePhase::Pre);
        assert!(matches!(gate.check(&ctx), GateVerdict::Pass { score: 1.0 }));
    }
}
