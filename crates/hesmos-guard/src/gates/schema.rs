//! schema — Envelope payload validation at Pre and Post (SS-01).
//!
//! ponytail: with no payload-schema registry anywhere in the P1 stack (plans carry only
//! schema IDs), the checkable surface is structural: the phase-mandated envelope exists,
//! the payload is a real value (not null), and the one FORBIDDEN shape the spec names is
//! rejected — a `{role, content}` chat dict crossing a boundary (SS-01 rule 2).
//! upgrade trigger: a schema registry (per-schema field definitions on the plan) lands →
//! add per-schema structural checks after the existing ones; the gate id/phase stay put.

use hesmos_core::{Envelope, GateVerdict, ReasonCode};

use crate::gate::{Gate, GateCtx, GatePhase};

pub struct SchemaGate;

impl Gate for SchemaGate {
    fn id(&self) -> &'static str {
        "schema"
    }

    fn check(&self, ctx: &GateCtx) -> GateVerdict {
        let reject = || GateVerdict::Reject {
            reason_code: ReasonCode::GATE_REJECT,
            score: 0.0,
        };
        // Pre judges the input envelope, Post the output envelope.
        let envelope: Option<&Envelope> = match ctx.phase {
            GatePhase::Pre | GatePhase::G0 => ctx.envelope_in,
            GatePhase::Post => ctx.envelope_out,
        };
        let Some(envelope) = envelope else {
            return reject();
        };
        let json = &envelope.payload.json;
        if json.is_null() {
            return reject();
        }
        // SS-01 rule 2 — a {role, content} dict is NOT a node payload, whatever else it
        // claims to be.
        if let Some(obj) = json.as_object()
            && obj.contains_key("role")
            && obj.contains_key("content")
        {
            return reject();
        }
        GateVerdict::Pass { score: 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use hesmos_core::{CorrelationId, EnvelopeId, EnvelopeKind, NodeId, Payload, SchemaId, Taint};

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

    fn post_ctx<'a>(fx: &'a Fixture, out: &'a Envelope) -> GateCtx<'a> {
        let mut ctx = fx.ctx(NodeId::new("n"), GatePhase::Post);
        ctx.envelope_out = Some(out);
        ctx
    }

    #[test]
    fn well_formed_payload_passes_role_content_dict_rejected() {
        let fx = Fixture::new();

        let good = envelope(serde_json::json!({ "summary": "ok", "count": 3 }));
        assert!(matches!(
            SchemaGate.check(&post_ctx(&fx, &good)),
            GateVerdict::Pass { score: 1.0 }
        ));

        // SS-01 rule 2 — the chat-dict shape is a boundary violation at Post too.
        let chat = envelope(serde_json::json!({ "role": "assistant", "content": "hi" }));
        assert!(matches!(
            SchemaGate.check(&post_ctx(&fx, &chat)),
            GateVerdict::Reject {
                reason_code: ReasonCode::GATE_REJECT,
                score: 0.0
            }
        ));

        let null = envelope(serde_json::Value::Null);
        assert!(matches!(
            SchemaGate.check(&post_ctx(&fx, &null)),
            GateVerdict::Reject { .. }
        ));
    }

    #[test]
    fn phase_selects_the_envelope_side() {
        let fx = Fixture::new();
        let out = envelope(serde_json::json!({ "x": 1 }));

        // At Post with NO output envelope: reject.
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Post);
        assert!(matches!(SchemaGate.check(&ctx), GateVerdict::Reject { .. }));

        // The same envelope judged at Pre (as input) passes.
        let mut pre = fx.ctx(NodeId::new("n"), GatePhase::Pre);
        pre.envelope_in = Some(&out);
        assert!(matches!(
            SchemaGate.check(&pre),
            GateVerdict::Pass { score: 1.0 }
        ));
    }
}
