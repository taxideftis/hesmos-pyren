//! TYPE-7 judgment and error types — the closed vocabularies.
//!
//! Closed sets here are load-bearing (project-context §4): the 6 reason codes and 9
//! compile errors are exhaustive — a new reason requires an exceptions.md revision first,
//! never a new enum variant slipped into a crate. Exit-code mapping lives in the CLI
//! (single mapping site, WP-P1e); this file stays data-only.

use serde::{Deserialize, Serialize};

/// Schema-level rejection at a crate/boundary parse point (AC2 of US-01: rejected before
/// any core state is touched). Distinct from [`HesmosError`], which is the CLI/HTTP error
/// body; convert into it at the presentation edge. No `Eq` — the confidence variant
/// carries an `f32`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SchemaError {
    #[error("invalid sha256 hex `{0}` — expected 64 lowercase hex chars")]
    InvalidSha256(String),
    #[error("confidence must be within 0.0..=1.0, got {0}")]
    ConfidenceOutOfRange(f32),
    #[error("missing required attribute(s) on event `{kind}`: {missing:?}")]
    MissingAttrs { kind: String, missing: Vec<String> },
    #[error("state transition not allowed by the transition table: {0}")]
    IllegalTransition(String),
}

/// Runtime termination reasons — exactly six, forever (SS-08 rule 1).
///
/// Variant names deliberately violate upper-camel-case: they ARE the closed-vocabulary
/// spellings from exceptions.md (the serde roundtrip test pins them), and `code` strings
/// must never drift from the enum name.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReasonCode {
    /// Loop guard: cumulative handoff cap (20) reached → HALTED, exit 11.
    MAX_HANDOFFS,
    /// Node/wave timeout → HALTED (ABORTED), exit 12.
    TIMEOUT,
    /// Loop guard: A→B→A ping-pong within window 8 → HALTED, exit 11.
    REPETITIVE_HANDOFF,
    /// Budget envelope 100% → SUSPENDED + checkpoint, exit 10.
    BUDGET_EXCEEDED,
    /// Final gate Reject (incl. cache-invariant violation special case, W3-Part5) → FAILED, exit 20.
    GATE_REJECT,
    /// LLM callback error / marshalling failure → HALTED (ABORTED), exit 12.
    PROVIDER_FAILURE,
}

/// Five-class error taxonomy (exceptions.md §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Reason,
    Compile,
    Platform,
    Usage,
    Ffi,
}

/// Pre-execution plan/config errors — all map to exit 3 (exceptions.md §3).
/// Detected at compile time before any node runs and before the session exists (US-02 AC1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub enum CompileError {
    /// CE-01 — plan YAML does not match the schema; `location` points at the offender.
    SchemaParse { location: String },
    /// CE-02 — `depends` references a node that does not exist.
    UnknownNodeRef { node: String, refs: Vec<String> },
    /// CE-03 — dependency graph has a cycle; `cycle` is the offending path.
    CyclicDependency { cycle: Vec<String> },
    /// CE-04 — flow DSL outside the "a -> b, c" grammar.
    InvalidDsl { expression: String },
    /// CE-05 — goal_original missing (plan.task absent): a contract cannot be created.
    MissingGoalOriginal,
    /// CE-06 — AgentProfile absent/incomplete on a node (primary detection: compile).
    ProfileMissing { node: String },
    /// CE-07 — pattern/edge combination does not normalize to one of the 4 templates.
    PatternNormalizationFailed { reason: String },
    /// CE-08 — duplicate node id.
    DuplicateNodeId { node: String },
    /// CE-09 — budget spec malformed (`--budget key=val` guidance).
    InvalidBudgetSpec,
}

impl CompileError {
    /// Stable `CE-xx` code for the closed set of nine (exceptions.md §3).
    pub fn code(&self) -> &'static str {
        match self {
            Self::SchemaParse { .. } => "CE-01",
            Self::UnknownNodeRef { .. } => "CE-02",
            Self::CyclicDependency { .. } => "CE-03",
            Self::InvalidDsl { .. } => "CE-04",
            Self::MissingGoalOriginal => "CE-05",
            Self::ProfileMissing { .. } => "CE-06",
            Self::PatternNormalizationFailed { .. } => "CE-07",
            Self::DuplicateNodeId { .. } => "CE-08",
            Self::InvalidBudgetSpec => "CE-09",
        }
    }
}

/// Gate verdict — `Gate::check` return (TRAIT-3). Retry is still recorded as a gate.fail
/// event (retry-announcing attrs); no judgment-free progress exists (US-11 AC2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateVerdict {
    Pass {
        score: f32,
    },
    Retry {
        reason_code: ReasonCode,
        score: f32,
        attempts_left: u8,
    },
    Reject {
        reason_code: ReasonCode,
        score: f32,
    },
}

/// Handoff routing decision — `HandoffRouter::route` return (TRAIT-4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RouteDecision {
    Accept {
        to: crate::ids::NodeId,
        contract_hash: crate::ids::Sha256Hex,
    },
    /// Rejected contract returns to the sender; the next node never starts (SS-06 rule 6).
    ReturnToSender {
        reason_code: ReasonCode,
        contract_hash: crate::ids::Sha256Hex,
    },
    /// Below-threshold confidence reuses the bounded-retry path — no separate loop (SS-10 rule 3).
    RetryBounded {
        reason_code: ReasonCode,
        attempts_left: u8,
    },
    /// MAX_HANDOFFS / REPETITIVE_HANDOFF — no new node starts after a Halt (SS-11).
    Halt { reason_code: ReasonCode },
}

/// CLI/HTTP common error body (TYPE-7, exceptions.md §9-3: stderr carries exactly one
/// JSON line of this on failure). `code` holds ReasonCode | CE-xx | E-* | USAGE_* | FFI_*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HesmosError {
    pub class: ErrorClass,
    pub code: String,
    pub session_id: Option<crate::ids::SessionId>,
    pub node_id: Option<crate::ids::NodeId>,
    pub message: String,
    pub hint: Option<String>,
}

impl HesmosError {
    /// Builds a Reason-class error. Message tone contract: cause + next action — a bare
    /// "failed" string is forbidden (exceptions.md §9-4).
    pub fn reason(code: ReasonCode, message: impl Into<String>) -> Self {
        Self {
            class: ErrorClass::Reason,
            code: code_to_static(code).to_string(),
            session_id: None,
            node_id: None,
            message: message.into(),
            hint: None,
        }
    }

    /// Builds a Compile-class error from a CE-variant; hint travels with it.
    pub fn compile(err: &CompileError, hint: impl Into<String>) -> Self {
        Self {
            class: ErrorClass::Compile,
            code: err.code().to_string(),
            session_id: None,
            node_id: None,
            message: format!("{err:?}"),
            hint: Some(hint.into()),
        }
    }

    pub fn with_session(mut self, session_id: crate::ids::SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn with_node(mut self, node_id: crate::ids::NodeId) -> Self {
        self.node_id = Some(node_id);
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Reason code as its canonical spelling (`code` strings must not drift from the enum).
pub fn code_to_static(code: ReasonCode) -> &'static str {
    match code {
        ReasonCode::MAX_HANDOFFS => "MAX_HANDOFFS",
        ReasonCode::TIMEOUT => "TIMEOUT",
        ReasonCode::REPETITIVE_HANDOFF => "REPETITIVE_HANDOFF",
        ReasonCode::BUDGET_EXCEEDED => "BUDGET_EXCEEDED",
        ReasonCode::GATE_REJECT => "GATE_REJECT",
        ReasonCode::PROVIDER_FAILURE => "PROVIDER_FAILURE",
    }
}

impl ReasonCode {
    /// Terminal session state for a reason (SS-08 rule 2 — the CANCELLED/FAILED
    /// separation). BUDGET_EXCEEDED suspends (resume = checkpoint), the loop/timeout
    /// family halts (resume = fork), GATE_REJECT fails (resume = fix + rerun).
    /// `SessionState::Cancelled` has no entry transition (W3-Part4) — external
    /// cancellation is process-level (exit 130) and never minted by a reason code.
    pub fn terminal_state(self) -> crate::session::SessionState {
        use crate::session::SessionState;
        match self {
            Self::BUDGET_EXCEEDED => SessionState::Suspended,
            Self::MAX_HANDOFFS
            | Self::REPETITIVE_HANDOFF
            | Self::TIMEOUT
            | Self::PROVIDER_FAILURE => SessionState::Halted,
            Self::GATE_REJECT => SessionState::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 6-code set is closed: serialization must round-trip to the exact spelling so
    /// events and exit mapping never disagree on a code name.
    #[test]
    fn reason_code_names_roundtrip_exactly() {
        for code in [
            ReasonCode::MAX_HANDOFFS,
            ReasonCode::TIMEOUT,
            ReasonCode::REPETITIVE_HANDOFF,
            ReasonCode::BUDGET_EXCEEDED,
            ReasonCode::GATE_REJECT,
            ReasonCode::PROVIDER_FAILURE,
        ] {
            let as_json = serde_json::to_string(&code).expect("serialize");
            assert_eq!(as_json, format!("\"{}\"", code_to_static(code)));
            let back: ReasonCode = serde_json::from_str(&as_json).expect("deserialize");
            assert_eq!(back, code);
        }
    }

    #[test]
    fn compile_error_codes_match_the_ce_table() {
        assert_eq!(CompileError::MissingGoalOriginal.code(), "CE-05");
        assert_eq!(
            CompileError::ProfileMissing { node: "n".into() }.code(),
            "CE-06"
        );
    }

    /// SS-08 rule 2: the reason → terminal-state mapping separates SUSPENDED (budget,
    /// resumable via checkpoint) from HALTED (fault family, resumable via fork) from
    /// FAILED (judgment did not pass — fix + rerun). Cancelled is intentionally
    /// unreachable: external cancellation has no reason code.
    #[test]
    fn terminal_states_separate_suspended_halted_failed() {
        use crate::session::SessionState;
        assert_eq!(
            ReasonCode::BUDGET_EXCEEDED.terminal_state(),
            SessionState::Suspended
        );
        for code in [
            ReasonCode::MAX_HANDOFFS,
            ReasonCode::REPETITIVE_HANDOFF,
            ReasonCode::TIMEOUT,
            ReasonCode::PROVIDER_FAILURE,
        ] {
            assert_eq!(code.terminal_state(), SessionState::Halted);
        }
        assert_eq!(
            ReasonCode::GATE_REJECT.terminal_state(),
            SessionState::Failed
        );
    }
}
