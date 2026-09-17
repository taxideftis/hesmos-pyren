//! rubric — the Post quality gate, RULE-BASED only (SS-09/SS-22: 규칙 기반 우선).
//!
//! ponytail: until the WP-P3b LLM judge exists, the rubric can only score STRUCTURE. The
//! deterministic score is the sum of three components:
//!
//! - 0.6 — an output envelope with a non-null, non-empty payload (the deliverable exists)
//! - 0.2 — the payload carries content: a non-empty object or a non-empty string
//! - 0.2 — the contract supplies non-empty done_criteria (a basis to judge against)
//!
//! A well-formed output reaches 1.0 and PASSES — the rule base must be able to fully
//! pass so it never invents failures. 1.0 → Pass, 0.0 → Reject, in between → Retry on
//! the shared bounded path (which is exactly what makes W3-2 CONCERNS boundaries
//! observable end-to-end).
//!
//! upgrade trigger: WP-P3b judge lands → the score source becomes the judge verdict
//! (judge.* optional attrs already exist in the taxonomy); the mapping below stays.

use hesmos_core::{GateVerdict, ReasonCode};

use crate::gate::{Gate, GateCtx};

pub struct RubricGate;

impl Gate for RubricGate {
    fn id(&self) -> &'static str {
        "rubric"
    }

    fn check(&self, ctx: &GateCtx) -> GateVerdict {
        let mut score = 0.0;

        if let Some(out) = ctx.envelope_out {
            let json = &out.payload.json;
            if !json.is_null() {
                score += 0.6;
                let content_bearing = match json {
                    serde_json::Value::Object(map) => !map.is_empty(),
                    serde_json::Value::String(s) => !s.trim().is_empty(),
                    _ => true, // numbers / bools / arrays are content too
                };
                if content_bearing {
                    score += 0.2;
                }
            }
        }
        let criteria_present = ctx.contract.is_some_and(|c| {
            c.done_criteria
                .as_ref()
                .is_some_and(|dc| dc.items.iter().any(|i| !i.trim().is_empty()))
        });
        if criteria_present {
            score += 0.2;
        }

        if score >= 1.0 {
            GateVerdict::Pass { score: 1.0 }
        } else if score <= 0.0 {
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0,
            }
        } else {
            GateVerdict::Retry {
                reason_code: ReasonCode::GATE_REJECT,
                score,
                attempts_left: ctx.attempts_left(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GatePhase;
    use crate::test_support::{Fixture, fixture_contract};
    use hesmos_core::{
        CorrelationId, Envelope, EnvelopeId, EnvelopeKind, NodeId, Payload, SchemaId, Taint,
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

    fn post_ctx<'a>(
        fx: &'a Fixture,
        out: &'a Envelope,
        contract: Option<&'a hesmos_core::HandoffContract>,
    ) -> GateCtx<'a> {
        let mut ctx = fx.ctx(NodeId::new("n"), GatePhase::Post);
        ctx.envelope_out = Some(out);
        ctx.contract = contract;
        ctx
    }

    #[test]
    fn well_formed_output_passes() {
        let fx = Fixture::new();
        let contract = fixture_contract();
        let out = envelope(serde_json::json!({ "summary": "done", "refs": [1, 2] }));
        assert!(matches!(
            RubricGate.check(&post_ctx(&fx, &out, Some(&contract))),
            GateVerdict::Pass { score: 1.0 }
        ));
        // A plain non-empty string payload with criteria also reaches 1.0.
        let textual = envelope(serde_json::json!("the migration finished cleanly"));
        assert!(matches!(
            RubricGate.check(&post_ctx(&fx, &textual, Some(&contract))),
            GateVerdict::Pass { score: 1.0 }
        ));
    }

    #[test]
    fn degenerate_output_retries_or_rejects() {
        let fx = Fixture::new();
        let contract = fixture_contract();
        // Empty object, criteria present → 0.6+0.2 = 0.8 → Retry (improvable).
        let thin = envelope(serde_json::json!({}));
        let ctx = post_ctx(&fx, &thin, Some(&contract));
        match RubricGate.check(&ctx) {
            GateVerdict::Retry {
                score,
                attempts_left,
                ..
            } => {
                assert!((score - 0.8).abs() < f32::EPSILON);
                assert_eq!(attempts_left, 2);
            }
            other => panic!("expected Retry, got {other:?}"),
        }

        // No output at all → 0.2 → Retry.
        let mut ctx = fx.ctx(NodeId::new("n"), GatePhase::Post);
        ctx.contract = Some(&contract);
        assert!(matches!(
            RubricGate.check(&ctx),
            GateVerdict::Retry { score: 0.2, .. }
        ));

        // No output, no contract → 0.0 → Reject.
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Post);
        assert!(matches!(
            RubricGate.check(&ctx),
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0
            }
        ));
    }
}
