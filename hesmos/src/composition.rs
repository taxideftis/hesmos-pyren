//! The composition root's adapters — the ONLY place where the orchestrator's seam
//! traits meet the real guard / budget / trace machinery (code-structure §2, D-2: the
//! runner consumes seams; this module implements them with the sibling crates).
//!
//! Everything here is glue by design: each adapter maps seam vocabulary onto the real
//! crate's API in a few total lines and owns no judgment logic of its own.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hesmos_budget::{
    BudgetEngine, Ledger, MeterEntry, MeterKind as LedgerMeterKind, ThresholdVerdict,
};
use hesmos_core::{
    BudgetEnvelope, BudgetState, EventSink, GateVerdict, HandoffContract, PendingEvent, PolicySet,
    ReasonCode, SessionId, Sha256Hex, TeamId, TraceEvent,
};
use hesmos_guard::{Gate, GateCtx, GatePhase, LeaseRegistry, instantiate, run_checked_with};
use hesmos_orchestrator::{
    BudgetConductor, ContractValidator, GateConductor, GateRun, MeterKind, RunnerPhase,
    SpendVerdict,
};
use hesmos_trace::EventLog;

// ---------------------------------------------------------------------------
// Contract validation — the TRAIT-4 step-1 delegation, made real
// ---------------------------------------------------------------------------

/// Delegates to the guard's field-absence matrix validator. This is the adapter the
/// router's `ContractValidator` seam has been waiting for since WP-P1c.
pub struct GuardValidator;

impl ContractValidator for GuardValidator {
    fn validate(
        &self,
        contract: &HandoffContract,
        session_task: &str,
        confidence_floor: f32,
    ) -> hesmos_core::ContractJudgment {
        hesmos_guard::validate(contract, session_task, confidence_floor)
    }
}

// ---------------------------------------------------------------------------
// Gates — instantiate + run_checked_with behind the conductor seam
// ---------------------------------------------------------------------------

/// Runs real guard gates. Owns the [`LeaseRegistry`] (a guard type — it cannot cross
/// the seam, so the adapter holds it and lends `&` to each `GateCtx`).
pub struct GuardGates {
    leases: LeaseRegistry,
}

impl GuardGates {
    pub fn new() -> Self {
        Self {
            leases: LeaseRegistry::new(),
        }
    }
}

impl Default for GuardGates {
    fn default() -> Self {
        Self::new()
    }
}

impl GateConductor for GuardGates {
    fn run_gate(&self, req: GateRun<'_>) -> GateVerdict {
        // Unknown gate references are a composition invariant: the plan compiled, so a
        // ref that `instantiate` cannot name is a schema bug — panic, never pass.
        let gate: Box<dyn Gate> =
            instantiate(req.gate_ref, req.session_task, req.spawn_token_floor)
                .unwrap_or_else(|| panic!("unknown gate reference `{}`", req.gate_ref));

        let phase = match req.phase {
            RunnerPhase::Pre => GatePhase::Pre,
            RunnerPhase::Post => GatePhase::Post,
            RunnerPhase::G0 => GatePhase::G0,
        };
        let mut ctx = GateCtx::base(
            req.session,
            req.node.clone(),
            phase,
            req.policy,
            req.budget_state,
            &self.leases,
        );
        ctx.envelope_in = req.envelope_in;
        ctx.envelope_out = req.envelope_out;
        ctx.contract = req.contract;
        ctx.attempt = req.attempt;
        // run_checked_with emits the gate.pass/gate.fail event — the judgment-event
        // invariant (SS-09 rule 3) holds even though the orchestrator cannot name
        // `hesmos_guard` (D-2): the ADAPTER keeps the funnel, and the sink is the
        // runner's own (lent through GateRun), so every event lands on ONE chain.
        run_checked_with(gate.as_ref(), &ctx, req.sink, req.extra)
    }
}

// ---------------------------------------------------------------------------
// Budget — freeze → meter → judge behind the conductor seam
// ---------------------------------------------------------------------------

/// Meters into the session ledger and answers with threshold verdicts. `&self` +
/// interior mutability per the seam contract (single-runner-threaded execution).
pub struct BudgetMeter {
    ledger: Ledger,
    engine: RefCell<BudgetEngine>,
    team_id: Option<String>,
    /// Agent-scope spend within this session, per role (ledger GROUP BY is equally
    /// valid; the map avoids a query per gate).
    agent_spent: RefCell<BTreeMap<String, u64>>,
    /// ponytail: the TEAM scope counts THIS session's contribution only — true
    /// cross-session sums need the team's other ledgers, which P1's single-session
    /// world cannot observe. upgrade trigger: WP-P2b (gateway dashboards) or any
    /// multi-session feature must replace this with a cross-ledger sum.
    team_spent: RefCell<u64>,
}

impl BudgetMeter {
    /// Opens the ledger INSIDE the session's checkpoint.db (same file as the WAL —
    /// the runner owns that file's other tables; the table set is disjoint).
    pub fn open(
        session_id: &SessionId,
        db_path: &Path,
        budget: &BudgetEnvelope,
        team_id: Option<&TeamId>,
    ) -> Result<Self, hesmos_budget::LedgerError> {
        let ledger = Ledger::open(session_id, &db_path.display().to_string())?;
        let frozen =
            hesmos_budget::SessionBudget::freeze(budget.clone(), *session_id, team_id.cloned());
        Ok(Self {
            ledger,
            engine: RefCell::new(BudgetEngine::new(frozen)),
            team_id: team_id.map(|t| t.to_string()),
            agent_spent: RefCell::new(BTreeMap::new()),
            team_spent: RefCell::new(0),
        })
    }

    fn session_spent(&self) -> u64 {
        self.ledger.session_totals().map(|t| t.total()).unwrap_or(0)
    }
}

impl BudgetConductor for BudgetMeter {
    fn snapshot(&self, role: &hesmos_core::AgentRole) -> BudgetState {
        self.engine.borrow().budget_state(
            self.session_spent(),
            *self.team_spent.borrow(),
            role,
            *self.agent_spent.borrow().get(role.as_str()).unwrap_or(&0),
        )
    }

    fn meter(
        &self,
        role: &hesmos_core::AgentRole,
        kind: MeterKind,
        tokens_in: u64,
        tokens_out: u64,
    ) -> SpendVerdict {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_millis() as i64;
        let entry = MeterEntry {
            team_id: self.team_id.clone(),
            agent_role: role.as_str().to_string(),
            kind: match kind {
                MeterKind::Llm => LedgerMeterKind::Llm,
                MeterKind::Tool => LedgerMeterKind::Tool,
            },
            tokens_in,
            tokens_out,
            // Token-centric metering: cost stays None until a unit table exists
            // (data-model-erd §7) — the CLI-4 grammar rejects cost_usd for now.
            cost_usd: None,
        };
        if let Err(_e) = self.ledger.record(entry, ts) {
            // A lost ledger row would corrupt US-20 AC3 (ledger sums == event sums);
            // crash rather than drift silently.
            panic!("metering ledger write failed — the budget evidence would diverge");
        }
        *self
            .agent_spent
            .borrow_mut()
            .entry(role.as_str().to_string())
            .or_insert(0) += tokens_in + tokens_out;
        *self.team_spent.borrow_mut() += tokens_in + tokens_out;

        let verdict = self.engine.borrow_mut().evaluate(
            self.session_spent(),
            *self.team_spent.borrow(),
            role,
            *self.agent_spent.borrow().get(role.as_str()).unwrap_or(&0),
        );
        match verdict {
            ThresholdVerdict::Clear => SpendVerdict::Clear,
            // The seam's Warn drops `scope`: the CLI-4 contract renders
            // budget.event(level·spent·remaining) only, and the runner's warn-once
            // bookkeeping lives inside the engine's fired-flag set.
            ThresholdVerdict::Warn {
                spent, remaining, ..
            } => SpendVerdict::Warn { spent, remaining },
            ThresholdVerdict::Exceeded { .. } => SpendVerdict::Exceeded,
        }
    }
}

// ---------------------------------------------------------------------------
// Budget spec / policy / executor — the CLI's text surfaces
// ---------------------------------------------------------------------------

/// Parses the CLI-1 `--budget` grammar: `tokens=unbounded` | `tokens=N[,cost_usd=F]`.
///
/// `cost_usd` is RECOGNIZED but rejected: the frozen envelope (TYPE-6) has no cost
/// field and metering is token-centric until a unit table exists — silently dropping a
/// user-specified cost would be a swallowed input. Unknown keys are likewise usage
/// errors. (Plan-YAML budget blocks take the CE-09 path inside compile instead.)
pub fn parse_budget_spec(spec: &str) -> Result<BudgetEnvelope, String> {
    let mut envelope = BudgetEnvelope::default();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| crate::messages::budget_spec_malformed(spec))?;
        match key.trim() {
            "tokens" => {
                envelope.session_max_tokens = match value.trim() {
                    "unbounded" => None,
                    n => Some(
                        n.parse::<u64>()
                            .map_err(|_| crate::messages::budget_spec_malformed(spec))?,
                    ),
                };
            }
            "cost_usd" => {
                return Err(
                    "cost_usd는 아직 고정할 수 없습니다 (단위 테이블 미정의, P1은 토큰 계량만) — tokens만 지정하세요"
                        .to_string(),
                );
            }
            _ => return Err(crate::messages::budget_spec_malformed(spec)),
        }
    }
    Ok(envelope)
}

/// The composed policy: charter defaults, overridden by `HESMOS_POLICY_YAML` (inline
/// YAML of the PolicyOverrides surface) when set. Env errors are the caller's usage
/// error — a silently ignored policy file would lie about the discipline in force.
pub fn load_policy() -> Result<PolicySet, String> {
    match std::env::var("HESMOS_POLICY_YAML") {
        Ok(text) => {
            hesmos_guard::parse_str(&text).map_err(|e| format!("HESMOS_POLICY_YAML 파싱 실패: {e}"))
        }
        Err(_) => Ok(PolicySet::default()),
    }
}

/// The P1 executor selection — `HESMOS_EXECUTOR` must be absent or `echo`. Refusing
/// unknown values beats silently running the wrong executor.
pub fn resolve_executor() -> Result<(), String> {
    match std::env::var("HESMOS_EXECUTOR") {
        Ok(name) if name != "echo" => Err(crate::messages::executor_unsupported(&name)),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Sinks — the trace log plus one-line progress on stderr
// ---------------------------------------------------------------------------

/// pipes every event into the session's [`EventLog`] AND renders a one-line progress
/// record (CLI-1 stdout contract: "웨이브·노드·게이트 판정 한 줄 진행"). The log stays
/// the single source of truth; the console line is derived, never stored.
pub struct ProgressTee {
    pub log: EventLog,
    pub style: crate::tokens::Style,
}

impl EventSink for ProgressTee {
    fn emit(&self, e: PendingEvent) -> TraceEvent {
        let appended = self.log.emit(e);
        let node = appended.node.as_ref().map(|n| n.as_str()).unwrap_or("-");
        let detail = match appended.kind {
            hesmos_core::EventKind::GatePass => format!(
                "gate_id={} score={:.2}",
                appended.attrs.get_str("gate_id").unwrap_or("?"),
                appended.attrs.get_f32("score").unwrap_or(0.0)
            ),
            hesmos_core::EventKind::GateFail => format!(
                "gate_id={} reason={} score={:.2}",
                appended.attrs.get_str("gate_id").unwrap_or("?"),
                appended.attrs.get_str("reason_code").unwrap_or("?"),
                appended.attrs.get_f32("score").unwrap_or(0.0)
            ),
            hesmos_core::EventKind::LlmCall => format!(
                "tokens={}/{} latency={}ms",
                appended.attrs.get_u64("tokens_in").unwrap_or(0),
                appended.attrs.get_u64("tokens_out").unwrap_or(0),
                appended.attrs.get_u64("latency_ms").unwrap_or(0),
            ),
            hesmos_core::EventKind::SessionClose => format!(
                "final_state={}",
                appended.attrs.get_str("final_state").unwrap_or("?")
            ),
            _ => String::new(),
        };
        let glyph = match appended.kind {
            hesmos_core::EventKind::GatePass | hesmos_core::EventKind::HandoffAccept => {
                self.style.pass()
            }
            hesmos_core::EventKind::GateFail => self.style.fail(),
            _ => self.style.dim("·"),
        };
        // CLI-1 stdout contract: the progress stream IS stdout (stderr carries only the
        // one-line HesmosError JSON on failure).
        if detail.is_empty() {
            println!(
                "{} {:>4} {:<14} {}",
                glyph,
                appended.seq,
                appended.kind.as_vocab(),
                node
            );
        } else {
            println!(
                "{} {:>4} {:<14} {} {}",
                glyph,
                appended.seq,
                appended.kind.as_vocab(),
                node,
                self.style.dim(&detail)
            );
        }
        appended
    }
}

/// Re-exported for the replay path, which must open the SAME artifacts under a
/// derived fork id (the CLI composes them before the runner exists).
pub fn session_paths(root: &Path, session_id: &SessionId) -> (PathBuf, PathBuf, PathBuf) {
    let dir = root
        .join(".hesmos")
        .join("sessions")
        .join(session_id.to_string());
    (
        dir.clone(),
        dir.join("events.jsonl"),
        dir.join("checkpoint.db"),
    )
}

/// sha256 helper re-export so the CLI never formats a hash differently.
pub fn short_hash(hash: &Sha256Hex) -> String {
    crate::tokens::hash8(hash.as_str())
}

/// Convenience: an Arc is often what survives into closures (ctrlc handlers).
pub type SharedFlag = Arc<std::sync::atomic::AtomicBool>;

/// Maps a runner outcome to the CLI exit band (US-07 AC1 — all nine values live in
/// exit.rs; reason-less suspends are the SIGINT band).
pub fn reason_exit(code: Option<ReasonCode>, suspended: bool) -> i32 {
    match code {
        None if suspended => crate::exit::EXIT_SIGINT,
        Some(ReasonCode::BUDGET_EXCEEDED) => crate::exit::EXIT_BUDGET_SUSPENDED,
        Some(ReasonCode::MAX_HANDOFFS) | Some(ReasonCode::REPETITIVE_HANDOFF) => {
            crate::exit::EXIT_HALTED_LOOP
        }
        Some(ReasonCode::TIMEOUT) | Some(ReasonCode::PROVIDER_FAILURE) => {
            crate::exit::EXIT_HALTED_ABORTED
        }
        Some(ReasonCode::GATE_REJECT) => crate::exit::EXIT_FAILED,
        None => crate::exit::EXIT_OK,
    }
}

/// Unused-import guard for types used only in signature positions above.
#[allow(unused)]
fn _type_touch(_: &dyn Fn(&Sha256Hex, &HandoffContract, &Arc<AtomicBoolAlias>)) {}
type AtomicBoolAlias = std::sync::atomic::AtomicBool;
