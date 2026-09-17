//! permission — the Pre gate seat for SS-19 enforcement.
//!
//! ponytail: this gate is a seat reservation — it always Passes (with a recorded
//! gate.pass event, so the boundary is still judged) until WP-P2a wires capability
//! checks (tool allowlists, permission_cap, taint rules) into it.
//! upgrade trigger: WP-P2a permission enforcement lands → replace the body; the gate's
//! id, phase and event path stay identical, so plans referencing `permission.*` do not
//! change.

use hesmos_core::GateVerdict;

use crate::gate::{Gate, GateCtx};

pub struct PermissionGate;

impl Gate for PermissionGate {
    fn id(&self) -> &'static str {
        "permission"
    }

    fn check(&self, _ctx: &GateCtx) -> GateVerdict {
        GateVerdict::Pass { score: 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GatePhase;
    use crate::test_support::Fixture;
    use hesmos_core::NodeId;

    /// The seat holds the boundary: a judgment event exists (via run_checked) even while
    /// enforcement is deferred — a Pre boundary must never pass unjudged.
    #[test]
    fn seat_passes_every_boundary() {
        let fx = Fixture::new();
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Pre);
        assert!(matches!(
            PermissionGate.check(&ctx),
            GateVerdict::Pass { score: 1.0 }
        ));
    }
}
