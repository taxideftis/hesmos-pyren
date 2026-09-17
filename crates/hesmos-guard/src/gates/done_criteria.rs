//! done_criteria — the Post gate over completion evidence (SS-06 / ADR-0002).
//!
//! ponytail: the gate verifies the OUTPUT SURFACE — an output envelope with a real
//! payload exists, and the contract's done_criteria are present and non-empty (the
//! no-retry row re-checked at the boundary). Semantic satisfaction of individual
//! criteria items is a judging problem: the rubric's rule-based score covers structure
//! today and the WP-P3b LLM judge covers meaning later.
//! upgrade trigger: WP-P3b judge lands → criterion-level evaluation moves here or into
//! the rubric; the no-retry rule below never changes.

use hesmos_core::{GateVerdict, ReasonCode};

use crate::gate::{Gate, GateCtx};

pub struct DoneCriteriaGate;

impl Gate for DoneCriteriaGate {
    fn id(&self) -> &'static str {
        "done_criteria"
    }

    fn check(&self, ctx: &GateCtx) -> GateVerdict {
        let reject = || GateVerdict::Reject {
            reason_code: ReasonCode::GATE_REJECT,
            score: 0.0,
        };

        // No output to examine — the stage produced nothing; retry can produce one.
        let Some(out) = ctx.envelope_out else {
            return GateVerdict::Retry {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0,
                attempts_left: ctx.attempts_left(),
            };
        };
        if out.payload.json.is_null() {
            return GateVerdict::Retry {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0,
                attempts_left: ctx.attempts_left(),
            };
        }

        // ADR-0002: criteria absent/empty → there is nothing to re-check against, ever.
        // Defense in depth — the matrix already rejects this Pre; a boundary echo keeps
        // the invariant true even if a contract skipped the Pre gate.
        if let Some(contract) = ctx.contract {
            let has_criteria = contract
                .done_criteria
                .as_ref()
                .is_some_and(|dc| dc.items.iter().any(|i| !i.trim().is_empty()));
            if !has_criteria {
                return reject();
            }
        }

        GateVerdict::Pass { score: 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GatePhase;
    use crate::test_support::Fixture;
    use hesmos_core::{
        CorrelationId, DoneCriteria, Envelope, EnvelopeId, EnvelopeKind, NodeId, Payload, SchemaId,
        Taint,
    };

    fn envelope(json: serde_json::Value) -> Envelope {
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

    /// pass / fail / retry branches of the T5 matrix for this gate.
    #[test]
    fn pass_fail_retry_branches() {
        let fx = Fixture::new();
        let out = envelope(serde_json::json!({ "done": true }));

        // Pass — real output plus criteria present on the contract.
        let mut contract = crate::test_support::fixture_contract();
        let mut ctx = fx.ctx(NodeId::new("n"), GatePhase::Post);
        ctx.envelope_out = Some(&out);
        ctx.contract = Some(&contract);
        assert!(matches!(
            DoneCriteriaGate.check(&ctx),
            GateVerdict::Pass { score: 1.0 }
        ));

        // Reject-no-retry — empty criteria at the boundary (defense in depth).
        contract.done_criteria = Some(DoneCriteria { items: vec![] });
        let mut ctx = fx.ctx(NodeId::new("n"), GatePhase::Post);
        ctx.envelope_out = Some(&out);
        ctx.contract = Some(&contract);
        assert!(matches!(
            DoneCriteriaGate.check(&ctx),
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0
            }
        ));

        // Retry — output envelope missing entirely.
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Post);
        match DoneCriteriaGate.check(&ctx) {
            GateVerdict::Retry { attempts_left, .. } => assert_eq!(attempts_left, 2),
            other => panic!("expected Retry, got {other:?}"),
        }
    }
}
