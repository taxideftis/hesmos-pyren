//! PORT-1 EventSink — the only door events leave through (P6).
//!
//! Validation lives in [`PendingEvent::new`]: by the time an event reaches an
//! `EventSink`, the taxonomy check has already passed, so an invalid event cannot exist
//! (TYPE-3 invariant: taxonomy-external kinds / missing required attrs are refused — the
//! refusal is observable as an `Err` here, and `emit` itself stays total exactly as the
//! PORT-1 signature fixes it).

use crate::error::SchemaError;
use crate::ids::NodeId;
use crate::trace_event::{EventAttrs, EventKind, TraceEvent};

/// The append input, constructed only through validation.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingEvent {
    pub kind: EventKind,
    pub node: Option<NodeId>,
    pub attrs: EventAttrs,
}

impl PendingEvent {
    /// Boundary check: fails when the kind's required attrs are missing (T2 fixture #4 —
    /// e.g. GateFail without gate_id/reason_code/score never becomes an event).
    pub fn new(
        kind: EventKind,
        node: Option<NodeId>,
        attrs: EventAttrs,
    ) -> Result<Self, SchemaError> {
        attrs.validate(kind)?;
        Ok(Self { kind, node, attrs })
    }
}

/// PORT-1. Implementors own chain linking and storage (the default impl is
/// `hesmos-trace::EventLog`, jsonl hash chain); this trait is only the seam.
///
/// The signature is infallible by contract: append-time I/O failures are fatal to the
/// evidence chain, so fallible callers (the WAL transaction in WP-P1e) use the
/// implementor's inherent `try_append`-style API and treat an error as "no commit".
pub trait EventSink: Send + Sync {
    /// The only event creation path. Components writing logs to files directly are a P6
    /// violation; emit is the single funnel.
    fn emit(&self, e: PendingEvent) -> TraceEvent;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T2 fixture #4 at the boundary: a GateFail missing any of its three attrs is
    /// refused before an EventSink ever sees it.
    #[test]
    fn gate_fail_without_required_attrs_is_refused() {
        let short = EventAttrs::new()
            .set("gate_id", "rubric.v1")
            .set("score", 0.5f32);
        let err = PendingEvent::new(EventKind::GateFail, Some(NodeId::new("n")), short)
            .expect_err("missing reason_code");
        match err {
            SchemaError::MissingAttrs { kind, missing } => {
                assert_eq!(kind, "gate.fail");
                assert_eq!(missing, vec!["reason_code".to_string()]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn complete_event_passes_boundary() {
        let attrs = EventAttrs::new()
            .set("session_id", "01ARZ3NDEKTSV4RRFFQ69G5FAV")
            .set("seed", 42u64)
            .set("budget", "tokens=250000");
        assert!(PendingEvent::new(EventKind::SessionOpen, None, attrs).is_ok());
    }
}
