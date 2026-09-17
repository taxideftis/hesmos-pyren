//! Node-unit leases over external resources (SS-12 rule 2, PT-11).
//!
//! A lease is exclusive per resource string: while a node holds it, any other node's
//! access is a blockable violation. The registry is pure state — deciding WHAT counts as
//! an "external resource access" belongs to the enforcement point (permission gate in
//! WP-P2a, the runner until then); this type only answers "who, if anyone, holds it" so
//! the answer cannot drift between call sites.
//!
//! BTreeMap keeps iteration order deterministic (P1) — lease order is never observable
//! as a HashMap-style iteration artifact.

use std::collections::BTreeMap;

use hesmos_core::NodeId;

/// Lease acquisition/conflict errors. Not a `SchemaError` — leases are runtime guard
/// state, not wire schema.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    #[error("resource `{resource}` is already leased to node `{holder}`")]
    Held { resource: String, holder: String },
    #[error("node `{0}` does not hold a lease on `{1}`")]
    NotHeld(String, String),
}

/// Exclusive node→resource leases. Keys are free-form resource identifiers (path, URL,
/// tool-qualified name) — the vocabulary belongs to the tool layer, not here.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LeaseRegistry {
    held: BTreeMap<String, NodeId>,
}

impl LeaseRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grants an exclusive lease. Re-acquiring the SAME (resource, node) pair is
    /// idempotent — a node re-checking before its access must not deadlock against
    /// itself; a DIFFERENT node fails with [`LeaseError::Held`].
    pub fn acquire(&mut self, resource: &str, holder: &NodeId) -> Result<(), LeaseError> {
        match self.held.get(resource) {
            Some(current) if current == holder => Ok(()),
            Some(current) => Err(LeaseError::Held {
                resource: resource.to_string(),
                holder: current.to_string(),
            }),
            None => {
                self.held.insert(resource.to_string(), holder.clone());
                Ok(())
            }
        }
    }

    /// Releases a lease. Only the holder may release — a release by anyone else is an
    /// invariant violation, not a silent no-op (that would let node B free a resource
    /// node A is still writing).
    pub fn release(&mut self, resource: &str, holder: &NodeId) -> Result<(), LeaseError> {
        match self.held.get(resource) {
            Some(current) if current == holder => {
                self.held.remove(resource);
                Ok(())
            }
            _ => Err(LeaseError::NotHeld(
                holder.to_string(),
                resource.to_string(),
            )),
        }
    }

    /// The holder, if the resource is leased at all.
    pub fn holder(&self, resource: &str) -> Option<&NodeId> {
        self.held.get(resource)
    }

    /// The enforcement question: may `accessor` touch `resource` right now?
    /// `false` is the block signal (AC6 — 접근 차단).
    pub fn check(&self, resource: &str, accessor: &NodeId) -> bool {
        self.held.get(resource).is_some_and(|h| h == accessor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_lease_blocks_other_nodes() {
        let mut reg = LeaseRegistry::new();
        let a = NodeId::new("writer");
        let b = NodeId::new("reviewer");

        reg.acquire("data/report.csv", &a).expect("first grant");
        assert!(reg.check("data/report.csv", &a));
        assert!(
            !reg.check("data/report.csv", &b),
            "un-leased access must be blockable (AC6)"
        );
        assert_eq!(reg.holder("data/report.csv"), Some(&a));
        assert_eq!(
            reg.acquire("data/report.csv", &b),
            Err(LeaseError::Held {
                resource: "data/report.csv".into(),
                holder: "writer".into(),
            })
        );
    }

    #[test]
    fn reacquire_by_holder_is_idempotent_release_requires_holder() {
        let mut reg = LeaseRegistry::new();
        let a = NodeId::new("a");
        let b = NodeId::new("b");
        reg.acquire("net:api.example.com", &a).expect("grant");
        reg.acquire("net:api.example.com", &a).expect("idempotent");

        assert_eq!(
            reg.release("net:api.example.com", &b),
            Err(LeaseError::NotHeld(
                "b".into(),
                "net:api.example.com".into()
            )),
            "a non-holder cannot free someone else's lease"
        );
        reg.release("net:api.example.com", &a)
            .expect("holder frees");
        assert_eq!(reg.holder("net:api.example.com"), None);
        // Freed → another node can take it.
        reg.acquire("net:api.example.com", &b).expect("re-grant");
    }
}
