//! Bounded retry — the ONE retry path (SS-10).
//!
//! Post-gate Retry verdicts AND below-confidence contract retries consume the same
//! per-node budget (`PolicySet::bounded_retry`, default 2); no third loop may exist
//! (SS-10 rule 3, US-12). A third attempt is unrepresentable: [`RetryTracker::record`]
//! returns [`RetryOutcome::Exhausted`] and the caller escalates to the parent node —
//! the escalation is recorded as attributes on the final gate.fail event (the 13-kind
//! taxonomy has no dedicated escalation kind; the exhausting attempt IS the event).

use std::collections::BTreeMap;

use hesmos_core::NodeId;

/// What a retry attempt leaves behind. `attempts_left` counts REMAINING retries after
/// this one, so the first retry of a default budget reports 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// The retry is granted; the node may re-execute.
    Granted { attempts_left: u8 },
    /// The budget is spent — NO third attempt. The caller escalates to the parent.
    Exhausted,
}

/// Per-node retry budgets. Node-keyed: exhaustion on one node never consumes another
/// node's budget (the escalation moves the problem to the parent, it does not poison
/// siblings).
#[derive(Debug, Default, Clone)]
pub struct RetryTracker {
    budget: u8,
    spent: BTreeMap<NodeId, u8>,
}

impl RetryTracker {
    /// `budget` comes from `PolicySet::bounded_retry` — never hard-coded here (PT-4).
    pub fn new(budget: u8) -> Self {
        Self {
            budget,
            spent: BTreeMap::new(),
        }
    }

    /// Remaining retries for the node BEFORE spending one (feeds `GateCtx.attempt` and
    /// `GateVerdict::Retry.attempts_left`).
    pub fn attempts_left(&self, node: &NodeId) -> u8 {
        self.budget
            .saturating_sub(*self.spent.get(node).unwrap_or(&0))
    }

    /// Spends one retry. `Ok(Granted)` while budget remains; `Err(Exhausted)` when the
    /// node has already used its whole budget — the call still records the attempt so
    /// repeated Exhausted answers are stable and auditable.
    pub fn record(&mut self, node: &NodeId) -> RetryOutcome {
        let spent = self.spent.entry(node.clone()).or_insert(0);
        *spent += 1;
        if *spent > self.budget {
            RetryOutcome::Exhausted
        } else {
            RetryOutcome::Granted {
                attempts_left: self.attempts_left(node),
            }
        }
    }
}

/// Attributes describing a bounded-retry exhaustion, for the escalating gate.fail event.
pub fn escalation_attrs(parent: &NodeId) -> [(&'static str, String); 2] {
    [
        ("escalated_to", parent.to_string()),
        ("escalation_reason", "bounded_retry_exhausted".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SS-10 rule 1: exactly `bounded_retry` retries, then exhaustion — no third attempt
    /// is representable, and further records stay Exhausted.
    #[test]
    fn two_retries_then_escalation() {
        let n = NodeId::new("summarize");
        let mut t = RetryTracker::new(2);

        assert_eq!(t.attempts_left(&n), 2);
        assert_eq!(t.record(&n), RetryOutcome::Granted { attempts_left: 1 });
        assert_eq!(t.record(&n), RetryOutcome::Granted { attempts_left: 0 });
        assert_eq!(t.record(&n), RetryOutcome::Exhausted);
        assert_eq!(t.record(&n), RetryOutcome::Exhausted, "stays exhausted");
    }

    /// Budgets are per node — sibling nodes are independent, and the escalation attrs
    /// name the parent deterministically.
    #[test]
    fn budgets_are_node_scoped_and_escalation_names_parent() {
        let mut t = RetryTracker::new(1);
        let a = NodeId::new("a");
        let b = NodeId::new("b");

        assert_eq!(t.record(&a), RetryOutcome::Granted { attempts_left: 0 });
        assert_eq!(t.record(&a), RetryOutcome::Exhausted);
        assert_eq!(
            t.record(&b),
            RetryOutcome::Granted { attempts_left: 0 },
            "a's exhaustion must not consume b's budget"
        );
        let attrs = escalation_attrs(&NodeId::new("plan"));
        assert_eq!(attrs[0].0, "escalated_to");
        assert_eq!(attrs[0].1, "plan");
        assert_eq!(attrs[1].1, "bounded_retry_exhausted");
    }

    /// The default budget of 2 comes from PolicySet — this pins the wiring shape (the
    /// tracker takes the number; it never owns a default).
    #[test]
    fn budget_is_injected_from_policy() {
        let policy = hesmos_core::PolicySet::default();
        let mut t = RetryTracker::new(policy.bounded_retry);
        let n = NodeId::new("n");
        assert_eq!(policy.bounded_retry, 2);
        assert_eq!(t.record(&n), RetryOutcome::Granted { attempts_left: 1 });
    }
}
