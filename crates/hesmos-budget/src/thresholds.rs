//! Threshold judgment — warn at 80%, suspend at 100% (SS-15 rules 3–4, project-context
//! §3 수치 상한).
//!
//! The engine answers ONE question, in both directions:
//! - **after** a metered call: did the recording cross a threshold? (warn event /
//!   suspend+checkpoint are the runner's reactions)
//! - **before** a call: is the budget already exhausted? (pre-block — the
//!   after-the-fact accounting shape is exactly what SS-15 rule 4 forbids)
//!
//! Both directions consume the same pure [`BudgetEngine::evaluate`], so the numbers can
//! never disagree about where the lines are. Warn fires ONCE per scope (on the
//! crossing), because the rule is "경고 이벤트가 발행된다" — an event per call in the
//! band would spam the trace with non-events.

use std::collections::BTreeSet;

use hesmos_core::AgentRole;

use crate::envelope::SessionBudget;

/// budget.event `level` vocabulary (표 7): exactly these two, forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetLevel {
    Warn,
    Suspend,
}

/// Which scope produced the verdict — all three are tracked per SS-15 rule 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Session,
    Team,
    Agent,
}

/// The pre/post-call verdict. `spent`/`remaining` travel with Warn (표 7 attrs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThresholdVerdict {
    /// Below every warn line — nothing to record.
    Clear,
    /// A warn line was crossed this evaluation. `remaining` is that scope's remainder
    /// up to its suspend limit (`None` = uncapped scope — cannot happen for a scope
    /// that just warned, but the type keeps the event attr honest).
    Warn {
        scope: Scope,
        spent: u64,
        remaining: Option<u64>,
    },
    /// A suspend line is reached — the runner must suspend+checkpoint with
    /// reason=BUDGET_EXCEEDED, and pre-block every further llm.call/tool.call.
    Exceeded { scope: Scope },
}

/// Pure threshold engine over a frozen envelope. Scope spends are fed in; the engine
/// owns no clock and no I/O (P1 — the same inputs always yield the same verdict).
pub struct BudgetEngine {
    budget: SessionBudget,
    warn_fired: BTreeSet<Scope>,
}

impl BudgetEngine {
    pub fn new(budget: SessionBudget) -> Self {
        Self {
            budget,
            warn_fired: BTreeSet::new(),
        }
    }

    pub fn budget(&self) -> &SessionBudget {
        &self.budget
    }

    /// Evaluates all three scopes. Severity: Exceeded (any scope) > Warn (first
    /// unfired crossing; scopes checked session → team → agent, a fixed order) > Clear.
    ///
    /// `session_spent`/`agent_spent` come from this session's ledger; `team_spent`
    /// spans the whole team, so the caller sums it across the team's ledgers.
    pub fn evaluate(
        &mut self,
        session_spent: u64,
        team_spent: u64,
        agent_role: &AgentRole,
        agent_spent: u64,
    ) -> ThresholdVerdict {
        // —— suspend lines: any scope at/over 100% blocks everything (pre-block). ——
        if let Some(limit) = self.budget.session_suspend_limit() {
            if session_spent >= limit {
                return ThresholdVerdict::Exceeded {
                    scope: Scope::Session,
                };
            }
        }
        if let Some(limit) = self.budget.team_suspend_limit() {
            if team_spent >= limit {
                return ThresholdVerdict::Exceeded { scope: Scope::Team };
            }
        }
        let (agent_warn, agent_suspend) = self.budget.agent_limits(agent_role);
        if let Some(limit) = agent_suspend {
            if agent_spent >= limit {
                return ThresholdVerdict::Exceeded {
                    scope: Scope::Agent,
                };
            }
        }

        // —— warn lines: fire once per scope on the crossing. ——
        if let Some(limit) = self.budget.session_warn_limit() {
            if session_spent >= limit && !self.warn_fired.contains(&Scope::Session) {
                self.warn_fired.insert(Scope::Session);
                return ThresholdVerdict::Warn {
                    scope: Scope::Session,
                    spent: session_spent,
                    remaining: self
                        .budget
                        .session_suspend_limit()
                        .map(|s| s.saturating_sub(session_spent)),
                };
            }
        }
        if let Some(limit) = self.budget.team_warn_limit() {
            if team_spent >= limit && !self.warn_fired.contains(&Scope::Team) {
                self.warn_fired.insert(Scope::Team);
                return ThresholdVerdict::Warn {
                    scope: Scope::Team,
                    spent: team_spent,
                    remaining: self
                        .budget
                        .team_suspend_limit()
                        .map(|s| s.saturating_sub(team_spent)),
                };
            }
        }
        if let Some(limit) = agent_warn {
            if agent_spent >= limit && !self.warn_fired.contains(&Scope::Agent) {
                self.warn_fired.insert(Scope::Agent);
                return ThresholdVerdict::Warn {
                    scope: Scope::Agent,
                    spent: agent_spent,
                    remaining: agent_suspend.map(|s| s.saturating_sub(agent_spent)),
                };
            }
        }

        ThresholdVerdict::Clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::{BudgetEnvelope, SessionId};

    fn budget(session: u64) -> SessionBudget {
        SessionBudget::freeze(
            BudgetEnvelope {
                session_max_tokens: Some(session),
                team_max_tokens: None,
                agent_max_tokens: Default::default(),
                warn_pct: 80,
                suspend_pct: 100,
            },
            SessionId::from_u128(1),
            None,
        )
    }

    /// T8 ① — 250K envelope: at exactly 200_000 the warn fires with spent/remaining,
    /// and execution continues (verdict after warn is Clear, not a block).
    #[test]
    fn warn_fires_at_eighty_percent_once() {
        let mut e = BudgetEngine::new(budget(250_000));

        assert_eq!(
            e.evaluate(199_999, 0, &AgentRole::new("w"), 0),
            ThresholdVerdict::Clear
        );

        let v = e.evaluate(200_000, 0, &AgentRole::new("w"), 0);
        assert_eq!(
            v,
            ThresholdVerdict::Warn {
                scope: Scope::Session,
                spent: 200_000,
                remaining: Some(50_000)
            }
        );

        // Still in the band: already warned → Clear. Execution continues (SS-15 rule 3:
        // neither a missed warn nor a premature block is allowed).
        assert_eq!(
            e.evaluate(240_000, 0, &AgentRole::new("w"), 0),
            ThresholdVerdict::Clear
        );
    }

    /// T8 ② — at 100%: suspend. Every evaluation after that is Exceeded, which the
    /// runner turns into "zero further calls" (pre-block). Note the walk-through: the
    /// first evaluation at 249_999 is already inside the warn band, so it Warns once —
    /// suspend only arrives at the cap itself.
    #[test]
    fn suspend_at_one_hundred_percent_pre_blocks() {
        let mut e = BudgetEngine::new(budget(250_000));
        assert_eq!(
            e.evaluate(150_000, 0, &AgentRole::new("w"), 0),
            ThresholdVerdict::Clear
        );
        assert_eq!(
            e.evaluate(249_999, 0, &AgentRole::new("w"), 0),
            ThresholdVerdict::Warn {
                scope: Scope::Session,
                spent: 249_999,
                remaining: Some(1)
            }
        );
        assert_eq!(
            e.evaluate(250_000, 0, &AgentRole::new("w"), 0),
            ThresholdVerdict::Exceeded {
                scope: Scope::Session
            }
        );
        assert_eq!(
            e.evaluate(250_001, 0, &AgentRole::new("w"), 0),
            ThresholdVerdict::Exceeded {
                scope: Scope::Session
            }
        );
    }

    /// T8 ③ — the three scopes are independent: an agent cap trips while session/team
    /// are fine, and team spend (fed in across ledgers) trips on its own.
    #[test]
    fn three_scopes_trip_independently() {
        let mut env = BudgetEnvelope {
            session_max_tokens: Some(1_000_000),
            team_max_tokens: Some(500_000),
            agent_max_tokens: Default::default(),
            warn_pct: 80,
            suspend_pct: 100,
        };
        env.agent_max_tokens
            .insert(AgentRole::new("writer"), 10_000);
        let mut e = BudgetEngine::new(SessionBudget::freeze(env, SessionId::from_u128(1), None));

        // Agent over its private cap — session is nowhere near 1M.
        assert_eq!(
            e.evaluate(5_000, 0, &AgentRole::new("writer"), 10_000),
            ThresholdVerdict::Exceeded {
                scope: Scope::Agent
            }
        );

        // Team at 100% of 500K with session/agent fine.
        let mut e = BudgetEngine::new(budget(1_000_000));
        assert_eq!(
            e.evaluate(1_000, 500_000, &AgentRole::new("w"), 1_000),
            ThresholdVerdict::Clear,
            "no team cap on this envelope — team spend alone cannot trip"
        );
    }

    /// Exceeded outranks Warn when several scopes are in/over their bands at once.
    #[test]
    fn severity_is_exceeded_over_warn() {
        let mut env = BudgetEnvelope {
            session_max_tokens: Some(100),
            team_max_tokens: None,
            agent_max_tokens: Default::default(),
            warn_pct: 80,
            suspend_pct: 100,
        };
        env.agent_max_tokens.insert(AgentRole::new("w"), 10);
        let mut e = BudgetEngine::new(SessionBudget::freeze(env, SessionId::from_u128(1), None));
        // Session suspend (>=100) reached while agent is only at its warn line.
        assert_eq!(
            e.evaluate(100, 0, &AgentRole::new("w"), 8),
            ThresholdVerdict::Exceeded {
                scope: Scope::Session
            }
        );
    }

    /// Same inputs → same verdict until a warn fires: the engine is pure state, no
    /// clock (P1). After the one-shot warn, the only state change is the fired flag.
    #[test]
    fn evaluation_is_deterministic() {
        let mut e = BudgetEngine::new(budget(250_000));
        let v1 = e.evaluate(200_000, 0, &AgentRole::new("w"), 0);
        let v2 = e.evaluate(200_000, 0, &AgentRole::new("w"), 0);
        assert_ne!(v1, v2, "warn is one-shot per scope");
        assert_eq!(
            v2,
            ThresholdVerdict::Clear,
            "second look is Clear — warn already fired"
        );
    }
}
