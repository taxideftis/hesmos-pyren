//! G0 — the transfer-size gate (SS-07 rule 3): the handoff snapshot may not exceed
//! `PolicySet::max_transfer_tokens` (charter: 8K).
//!
//! The measurement is the deterministic proxy the core sanctioned for exactly this
//! check: canonical envelope bytes / 4 ≈ tokens (`Envelope::transfer_bytes`; the same
//! chars/4 estimator the contract validator uses for its 8K measurement). Exact
//! tokenization is explicitly out of scope — the LIMIT is what the contract freezes.

use hesmos_core::{GateVerdict, ReasonCode};

use crate::gate::{Gate, GateCtx};

pub struct G0Gate;

impl Gate for G0Gate {
    fn id(&self) -> &'static str {
        "G0"
    }

    fn check(&self, ctx: &GateCtx) -> GateVerdict {
        let Some(envelope) = ctx.envelope_in else {
            // G0 judges a transfer; no transfer to judge is a malformed boundary.
            return GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0,
            };
        };
        let tokens = (envelope.transfer_bytes().div_ceil(4)) as u64;
        if tokens <= ctx.policy.max_transfer_tokens {
            GateVerdict::Pass { score: 1.0 }
        } else {
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0,
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
        CorrelationId, Envelope, EnvelopeId, EnvelopeKind, NodeId, Payload, SchemaId, Taint,
    };

    fn envelope(summary_len: usize) -> Envelope {
        Envelope {
            id: EnvelopeId::from_u128(1),
            from: NodeId::new("a"),
            to: NodeId::new("b"),
            kind: EnvelopeKind::Task,
            payload: Payload {
                schema_id: SchemaId::new("s.v1"),
                json: serde_json::json!({ "summary": "x".repeat(summary_len) }),
            },
            correlation_id: CorrelationId::from_u128(2),
            taint: Taint::Clean,
        }
    }

    #[test]
    fn within_limit_passes_over_limit_rejects() {
        let fx = Fixture::new();
        let small = envelope(100);
        let mut ctx = fx.ctx(NodeId::new("n"), GatePhase::G0);
        ctx.envelope_in = Some(&small);
        assert!(matches!(
            G0Gate.check(&ctx),
            GateVerdict::Pass { score: 1.0 }
        ));

        // 8192 tokens ≈ 32_768 payload bytes — well past the charter cap.
        let big = envelope(40_000);
        let mut ctx = fx.ctx(NodeId::new("n"), GatePhase::G0);
        ctx.envelope_in = Some(&big);
        assert!(matches!(
            G0Gate.check(&ctx),
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0
            }
        ));
    }

    #[test]
    fn missing_envelope_is_rejected() {
        let fx = Fixture::new();
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::G0);
        assert!(matches!(G0Gate.check(&ctx), GateVerdict::Reject { .. }));
    }
}
