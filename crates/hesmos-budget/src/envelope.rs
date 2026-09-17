//! The session-frozen budget envelope (SS-15 rule 1).
//!
//! [`SessionBudget`] takes core's [`BudgetEnvelope`] at construction and never lets it
//! out mutably — there is no setter, so "change the envelope mid-session" is not
//! expressible in safe code (T8 ⑤ checks this at compile time, not by convention).
//! The thresholds themselves (80/100) are charter §6 values owned by the envelope; this
//! crate reads them, it does not invent numbers.

use hesmos_core::{BudgetEnvelope, SessionId, TeamId};

/// A frozen budget bound to one session. Clone freely — the clone is equally frozen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBudget {
    envelope: BudgetEnvelope,
    session_id: SessionId,
    team_id: Option<TeamId>,
}

impl SessionBudget {
    /// Freezes the envelope for a session. The only way in; there is no way to change.
    pub fn freeze(
        envelope: BudgetEnvelope,
        session_id: SessionId,
        team_id: Option<TeamId>,
    ) -> Self {
        Self {
            envelope,
            session_id,
            team_id,
        }
    }

    /// Read-only view of the frozen limits and thresholds.
    pub fn envelope(&self) -> &BudgetEnvelope {
        &self.envelope
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn team_id(&self) -> Option<&TeamId> {
        self.team_id.as_ref()
    }

    /// Absolute token count at which the session-scope warn fires.
    /// `None` limit = no cap = the threshold can never fire for that scope.
    pub fn session_warn_limit(&self) -> Option<u64> {
        pct_of(self.envelope.session_max_tokens, self.envelope.warn_pct)
    }

    pub fn session_suspend_limit(&self) -> Option<u64> {
        pct_of(self.envelope.session_max_tokens, self.envelope.suspend_pct)
    }

    /// Per-agent warn/suspend limits for `role` — `None` when the role has no cap.
    pub fn agent_limits(&self, role: &hesmos_core::AgentRole) -> (Option<u64>, Option<u64>) {
        let cap = self.envelope.agent_max_tokens.get(role).copied();
        (
            pct_of(cap, self.envelope.warn_pct),
            pct_of(cap, self.envelope.suspend_pct),
        )
    }

    pub fn team_suspend_limit(&self) -> Option<u64> {
        pct_of(self.envelope.team_max_tokens, self.envelope.suspend_pct)
    }

    /// Team-scope warn line (same 80% default; the envelope's one warn_pct governs
    /// every scope so the numbers can never disagree).
    pub fn team_warn_limit(&self) -> Option<u64> {
        pct_of(self.envelope.team_max_tokens, self.envelope.warn_pct)
    }
}

/// `pct%` of `total`, truncating — thresholds fire at *reaching* the limit, and
/// truncation can only make the limit equal-or-lower, never above 100% of the cap.
fn pct_of(total: Option<u64>, pct: u8) -> Option<u64> {
    total.map(|t| (t as u128 * u128::from(pct) / 100) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(session: u64) -> BudgetEnvelope {
        BudgetEnvelope {
            session_max_tokens: Some(session),
            team_max_tokens: None,
            agent_max_tokens: Default::default(),
            warn_pct: 80,
            suspend_pct: 100,
        }
    }

    /// Charter thresholds are the defaults; percent math truncates (200_000 of 250_000).
    #[test]
    fn warn_and_suspend_limits_are_eighty_and_full() {
        let b = SessionBudget::freeze(envelope(250_000), SessionId::from_u128(1), None);
        assert_eq!(b.session_warn_limit(), Some(200_000));
        assert_eq!(b.session_suspend_limit(), Some(250_000));
    }

    /// No cap in that scope = the threshold cannot fire (Option, not zero).
    #[test]
    fn absent_cap_means_no_threshold() {
        let b = SessionBudget::freeze(envelope(0), SessionId::from_u128(1), None);
        // session cap 0: 80% of 0 = 0 — a zero cap blocks from the first token, which
        // is the correct reading of "max_tokens: 0". The absent team cap is the
        // no-threshold case.
        assert_eq!(b.team_suspend_limit(), None);
    }

    /// Frozen means frozen: `envelope()` hands out a shared reference and no API exists
    /// to put a different envelope back — a new session budget would have to be
    /// re-frozen under a new object (SS-15 rule 1).
    #[test]
    fn envelope_is_only_reachable_by_reference() {
        let b = SessionBudget::freeze(envelope(100), SessionId::from_u128(1), None);
        let view: &BudgetEnvelope = b.envelope();
        assert_eq!(view.session_max_tokens, Some(100));
    }

    /// Per-agent limits resolve from the role map.
    #[test]
    fn agent_limits_resolve_per_role() {
        use hesmos_core::AgentRole;
        let mut env = envelope(1_000_000);
        env.agent_max_tokens
            .insert(AgentRole::new("writer"), 50_000);
        let b = SessionBudget::freeze(env, SessionId::from_u128(1), None);

        let (warn, suspend) = b.agent_limits(&AgentRole::new("writer"));
        assert_eq!((warn, suspend), (Some(40_000), Some(50_000)));

        let (warn, suspend) = b.agent_limits(&AgentRole::new("researcher"));
        assert_eq!((warn, suspend), (None, None), "uncapped role");
    }
}
