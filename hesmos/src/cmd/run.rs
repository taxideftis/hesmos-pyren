//! CLI-1 `hesmos run` — parse flags, compose the session (composition-root adapters +
//! runner), and render the exit summary. The command holds no state (SS-13 rule 3):
//! every decision comes from a crate; this file only sequences them and maps the
//! outcome onto the exit band (exit.rs — the single map).
//!
//! `run_session` is ALSO the engine behind `trace replay` (CLI-3): a fork is a new
//! session whose reproduction inputs come from the origin's session dir. Keeping both
//! entries on this one path is what makes the run↔replay outputs structurally
//! identical (§3.4: "이하 run 진행 출력과 동일").

use std::path::Path;

use hesmos_budget::Ledger;
use hesmos_core::{
    BathosEngine, BudgetEnvelope, CompileError, ErrorClass, EventKind, HesmosError, Plan,
    PolicySet, SessionId, Sha256Hex, TeamId, canonical_sha256,
};
use hesmos_orchestrator::{
    BathosCli, DeterministicEngine, EchoExecutor, EchoMode, ForkSource, GraphEngine,
    LoopGuardRouter, RunOutcome, Runner, RunnerConfig,
};
use hesmos_trace::{EventLog, LoadOutcome, SealError, seal};

use crate::composition::{
    BudgetMeter, CacheSentinel, GuardGates, GuardValidator, load_policy, parse_budget_spec,
    reason_exit, session_paths,
};
use crate::exit;
use crate::messages;
use crate::tokens::Style;

/// Everything the CLI-1 grammar contributes (`hesmos run <plan> --seed --budget --team --dry-run`).
#[derive(Debug, Clone)]
pub struct RunArgs {
    pub plan: std::path::PathBuf,
    pub seed: Option<u64>,
    pub budget: Option<String>,
    pub team: Option<String>,
    pub dry_run: bool,
}

/// One HesmosError JSON line on stderr (exceptions.md §9-3 — the CI-parseable surface).
pub(crate) fn emit_error(err: &HesmosError) {
    match serde_json::to_string(err) {
        Ok(line) => eprintln!("{line}"),
        // Serialization of a plain-struct error cannot fail; a panic here would hide
        // the original exit path, so the Debug form is the fallback line instead.
        Err(e) => eprintln!(
            r#"{{"class":"Usage","code":"USAGE-ARGS","message":"error render failed: {e}"}}"#
        ),
    }
}

/// Usage-band error line (exit 2) — shared by every command's flag validation, hence
/// `pub`: the binary's dispatch calls it directly for argv-shape errors.
pub fn usage_error(message: String) -> i32 {
    emit_error(&HesmosError {
        class: ErrorClass::Usage,
        code: "USAGE-ARGS".into(),
        session_id: None,
        node_id: None,
        message,
        hint: None,
    });
    exit::EXIT_USAGE
}

pub(crate) fn compile_error(err: &CompileError) -> i32 {
    emit_error(&HesmosError::compile(err, messages::compile_failed(err)));
    eprintln!("{}", messages::compile_failed(err));
    exit::EXIT_COMPILE
}

/// `✗ 세션 <id>의 trace를 찾을 수 없습니다` — CLI-2/3/4's shared exit-3 path (§4.4).
/// Class=Compile because exit 3 is that band's owner ("세션 없음" shares it per the
/// exit table); the code names the specific condition without borrowing a CE-xx
/// spelling it does not have.
pub(crate) fn session_missing(session_id: &SessionId) -> i32 {
    emit_error(&HesmosError {
        class: ErrorClass::Compile,
        code: "SESSION-NOT-FOUND".into(),
        session_id: Some(*session_id),
        node_id: None,
        message: messages::session_not_found(&session_id.to_string()),
        hint: None,
    });
    eprintln!("{}", messages::session_not_found(&session_id.to_string()));
    exit::EXIT_COMPILE
}

pub(crate) fn platform_error(message: String, session: Option<SessionId>) -> i32 {
    let mut err = HesmosError {
        class: ErrorClass::Platform,
        code: "EVIDENCE-INVALID".into(),
        session_id: None,
        node_id: None,
        message,
        hint: None,
    };
    if let Some(s) = session {
        err = err.with_session(s);
    }
    emit_error(&err);
    exit::EXIT_EVIDENCE_INVALID
}

/// bathos's model-registry verdict refused the session open (ml.md §6d decision 15):
/// class Bathos + the bathos code VERBATIM (observed: E-MODEL-MIX), exit 3 — the
/// pre-execution refusal band, so a refused open leaves no artifacts (CE-* rule).
fn model_refused(code: &str, session: Option<SessionId>) -> i32 {
    let mut err = HesmosError {
        class: ErrorClass::Bathos,
        code: code.to_string(),
        session_id: None,
        node_id: None,
        message: messages::model_refused(code),
        hint: None,
    };
    if let Some(s) = session {
        err = err.with_session(s);
    }
    emit_error(&err);
    exit::EXIT_COMPILE
}

/// Reads and parses the plan WITHOUT creating anything — CE-* must leave no session
/// behind (US-04 AC2). Unreadable file is a bad invocation (exit 2); unreadable
/// CONTENT is a compile error (exit 3).
pub(crate) fn load_plan(path: &Path) -> Result<Plan, i32> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        usage_error(messages::plan_unreadable(
            &path.display().to_string(),
            &e.to_string(),
        ))
    })?;
    hesmos_orchestrator::parse_plan(&text).map_err(|e| compile_error(&e))
}

/// CLI-1 entry. `root` is the mission root (cwd for the binary, a tempdir for tests).
pub fn execute(args: RunArgs, root: &Path) -> i32 {
    let style = Style::detect();

    if args.dry_run {
        return dry_run(&args, &style);
    }

    let plan = match load_plan(&args.plan) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let budget = match &args.budget {
        Some(spec) => match parse_budget_spec(spec) {
            Ok(b) => b,
            Err(msg) => return usage_error(msg),
        },
        // Absent --budget = unbounded, but ALWAYS frozen (SS-15 rule 1 — the envelope
        // is recorded on session.open either way).
        None => BudgetEnvelope::default(),
    };
    let team_id = args.team.as_deref().map(hesmos_orchestrator::parse_team_id);

    // CE-03 (cycles) surfaces at graph build, not at parse — run it before any
    // artifact exists so a cyclic plan also leaves no session behind.
    let probe_seed = args.seed.unwrap_or(0);
    if let Err(e) = DeterministicEngine.compile(&plan, probe_seed) {
        return compile_error(&e);
    }

    let session_id = SessionId::generate();
    run_session(
        root, &style, plan, session_id, args.seed, budget, team_id, None,
    )
    .0
}

/// `--dry-run`: compile + wave schedule only. No session, no events, no WAL, no
/// external calls (US-04 AC1) — the plan.compiled EVENT NAME is deliberately not used
/// here (ui-spec §3.2: an event name may appear only for a recorded event).
fn dry_run(args: &RunArgs, style: &Style) -> i32 {
    let plan = match load_plan(&args.plan) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let compiled = match DeterministicEngine.compile(&plan, args.seed.unwrap_or(0)) {
        Ok(c) => c,
        Err(e) => return compile_error(&e),
    };
    println!(
        "컴파일 통과 — nodes={}  waves={}  외부 호출 0건 (비용 없음)",
        compiled.node_count,
        compiled.waves().len(),
    );
    for w in compiled.waves() {
        println!(
            "  wave {}  {}",
            w.index.0 + 1,
            w.nodes
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!(
        "{}",
        style.pass() + " 구조 검증 통과 — exit 0 (세션·이벤트 미생성)"
    );
    println!(
        "  실행: hesmos run {} --seed <시드> --budget tokens=<상한>",
        args.plan.display()
    );
    exit::EXIT_OK
}

/// The composed P1 executor — `HESMOS_EXECUTOR` selects the [`EchoMode`]. The modes
/// exist so the E2E suite drives every failure path through the SAME seam a real
/// adapter will use (runner.rs EchoMode doc); unknown values refuse to run.
///
/// ponytail: env-selected echo modes are P1's test surface, replaced wholesale when
/// the Python bridge (PY-1..5) lands; trigger: WP-P2e composition swap.
fn echo_mode() -> Result<EchoMode, String> {
    match std::env::var("HESMOS_EXECUTOR") {
        Err(_) => Ok(EchoMode::Echo),
        Ok(name) => match name.as_str() {
            "echo" => Ok(EchoMode::Echo),
            "reject" => Ok(EchoMode::Reject),
            "concerns" => Ok(EchoMode::Concerns),
            other => {
                if let Some(n) = other.strip_prefix("flaky:") {
                    return n
                        .parse::<usize>()
                        .map(EchoMode::Flaky)
                        .map_err(|_| messages::executor_unsupported(&name));
                }
                Err(messages::executor_unsupported(&name))
            }
        },
    }
}

/// Composes the session artifacts and the runner — everything from env discipline to
/// `Runner::open`, but NOT the run loop. Shared by `run`, `trace replay` and `eval`
/// so all three drive the SAME adapters and the same guards; the `sink_builder`
/// decides the stdout face (`progress_sink` for run/replay, `LogOnlyTee` for eval —
/// eval's stdout belongs to the suite report).
///
/// Returns only a runner or an exit code: every failure here is a CLI-band error
/// (usage/compile/platform) and no outcome exists yet.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_session_runner(
    root: &Path,
    plan: &Plan,
    session_id: SessionId,
    seed: u64,
    budget: &BudgetEnvelope,
    team_id: Option<TeamId>,
    fork: Option<ForkSource>,
    sink_builder: impl FnOnce(EventLog) -> Box<dyn hesmos_core::EventSink>,
) -> Result<Runner, i32> {
    let mode = echo_mode().map_err(usage_error)?;
    let policy: PolicySet = load_policy().map_err(usage_error)?;

    let (session_dir, _, checkpoint_db) = session_paths(root, &session_id);
    if session_dir.exists() {
        return Err(usage_error(format!(
            "세션 디렉터리가 이미 존재합니다: {} — 동일 세션 ID 재실행은 금지됩니다 (USAGE-STATE)",
            session_dir.display()
        )));
    }

    // CE-03 probe (fork/eval plans come from snapshots, still worth the same
    // no-artifact guarantee): compile BEFORE the EventLog creates the session dir.
    if let Err(e) = DeterministicEngine.compile(plan, seed) {
        return Err(compile_error(&e));
    }

    // PORT-2 model validate (ml.md §6d decision 15 — FFI parity): the compile probe
    // proved the plan's SHAPE; bathos now proves its MODELS, still before any
    // artifact exists so a refusal leaves no session behind (CE-* guarantee).
    // A bathos verdict passes through verbatim (exceptions.md §5): its `code` rides
    // the Bathos base class un-renamed. A missing/unspawnable bathos proceeds
    // UNVERIFIED — the seal's exit-127 rule: substituting our own judgment would be
    // reimplementing the registry. The seal-time 보류 note is where absence becomes
    // visible, so this stays silent (eval's stdout belongs to its report anyway).
    let engine = BathosCli::new(std::env::var("HESMOS_BATHOS").unwrap_or_else(|_| "bathos".into()))
        .with_cwd(root);
    match engine.model_validate() {
        Ok(report) if !report.ok => {
            let code = report
                .raw
                .0
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("E-MODEL-MIX")
                .to_string();
            return Err(model_refused(&code, Some(session_id)));
        }
        Ok(_) => {}
        Err(_) => {}
    }

    // ONE EventLog handle per session (P0b single-writer discipline) — the tee moves
    // it into the runner; the seal reopens only after the runner (and tee) are dropped.
    let log = match EventLog::open(root, &session_id) {
        Ok(l) => l,
        Err(e) => {
            return Err(platform_error(
                format!("trace log open 실패: {e}"),
                Some(session_id),
            ));
        }
    };
    let meter = match BudgetMeter::open(&session_id, &checkpoint_db, budget, team_id.as_ref()) {
        Ok(m) => m,
        Err(e) => {
            return Err(platform_error(
                format!("ledger open 실패: {e}"),
                Some(session_id),
            ));
        }
    };

    Runner::open(
        RunnerConfig {
            root: root.to_path_buf(),
            policy: policy.clone(),
        },
        plan,
        session_id,
        seed,
        budget.clone(),
        team_id,
        fork,
        Box::new(EchoExecutor::new(mode)),
        Box::new(GuardGates::new()),
        Box::new(meter),
        Box::new(LoopGuardRouter::new(&plan.task, GuardValidator, policy)),
        // W5 freezes no system prompt (see CacheSentinel) — the sentinel is inert
        // until WP-P2d's prompt builder supplies the SS-04 hash.
        Box::new(CacheSentinel { frozen: None }),
        sink_builder(log),
    )
    .map_err(|e| runner_error(e, session_id))
}

/// Composes and runs ONE session end-to-end (S01..S17): adapters → runner → seal →
/// summary → exit band. Shared by `run` (fresh id) and `trace replay` (derived fork
/// id). Returns the exit band AND the outcome (when the run loop executed) — replay
/// needs the outcome for its reproduction summary; `run` ignores it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_session(
    root: &Path,
    style: &Style,
    plan: Plan,
    session_id: SessionId,
    seed: Option<u64>,
    budget: BudgetEnvelope,
    team_id: Option<TeamId>,
    fork: Option<ForkSource>,
) -> (i32, Option<RunOutcome>) {
    // Seed before anything prints: the banner shows the recorded value (CLI-1 — the
    // recorded seed becomes a reproduction input).
    let seed = seed.unwrap_or_else(random_seed);

    let runner = match open_session_runner(
        root,
        &plan,
        session_id,
        seed,
        &budget,
        team_id,
        fork,
        |log| crate::composition::progress_sink(log, *style),
    ) {
        Ok(r) => r,
        Err(code) => return (code, None),
    };

    // SIGINT → cooperative suspend (ST-1: RUNNING —SIGINT→ SUSPENDED, exit 130). The
    // handler only flips the runner's flag; the run loop picks it up between steps, so
    // no boundary is ever cut in half. Handler-set failure is non-fatal: the process
    // keeps default SIGINT semantics (the shell still reports 130).
    let flag = runner.interrupt_handle();
    if let Err(e) =
        ctrlc::set_handler(move || flag.store(true, std::sync::atomic::Ordering::SeqCst))
    {
        println!(
            "{}",
            style.dim(&format!(
                "! SIGINT 핸들러 설치 실패({e}) — 기본 시그널 동작으로 폴백"
            ))
        );
    }

    // —— Banner (§3.3): the session id echoes twice (open + summary) so CI can parse it.
    let plan_hash = runner.handle().plan_hash.clone();
    let cache_note = match &fork_of_note(root, &session_id) {
        Some(_) => "fork",
        None => "미사용",
    };
    println!("hesmos run — session {session_id}");
    println!(
        "  재현 4요소: session_id={session_id}  plan_hash={}  seed={seed}  cache={cache_note}",
        crate::tokens::hash8(plan_hash.as_str())
    );
    println!("  (동일 4요소 재실행 시 동일 구조로 재생됩니다 — hesmos trace replay {session_id})");

    let outcome = match runner.run() {
        Ok(o) => o,
        Err(e) => return (runner_error(e, session_id), None),
    };

    let code = close_session(root, style, &outcome, session_id, &plan_hash);
    (code, Some(outcome))
}

/// S16: seal a COMPLETED session and render the exit summary (§3.5). Every terminal
/// state leaves through here so the exit mapping happens in exactly one place.
fn close_session(
    root: &Path,
    style: &Style,
    outcome: &RunOutcome,
    session_id: SessionId,
    plan_hash: &Sha256Hex,
) -> i32 {
    let (_, events_path, checkpoint_db) = session_paths(root, &session_id);
    let _ = events_path;

    // Seal ONLY a completed session (ST-1: COMPLETED —trace seal→ 증거 성립). Other
    // terminals stay unsealed: fork/resume continues from them.
    let mut seal_note = String::from("seal 없음 (미완료 세션 — 재생 대상)");
    let code = reason_exit(
        outcome.reason,
        outcome.final_state == hesmos_core::SessionState::Suspended,
    );
    if outcome.final_state == hesmos_core::SessionState::Completed {
        let log = match EventLog::open(root, &session_id) {
            Ok(l) => l,
            Err(e) => {
                return platform_error(format!("trace log reopen 실패: {e}"), Some(session_id));
            }
        };
        let engine =
            BathosCli::new(std::env::var("HESMOS_BATHOS").unwrap_or_else(|_| "bathos".into()))
                .with_cwd(root);
        match seal(&log, Some(&engine)) {
            Ok(out) => {
                seal_note = match out.audit_verified {
                    Some(true) => format!(
                        "증거     trace.seal chain_head={} — bathos audit verified",
                        crate::tokens::hash8(out.chain_head.as_str())
                    ),
                    // 127 = the bathos binary never ran (spawn failure) — the LOCAL
                    // seal stands (it was appended before the audit call); only the
                    // audit join is deferred. Not a verify FAILURE — no exit 30.
                    _ => {
                        println!("{}", style.dim(&messages::bathos_absent()));
                        "증거     trace.seal chain_head=… — bathos audit 보류".to_string()
                    }
                };
            }
            Err(SealError::Audit {
                bathos_exit: 127, ..
            }) => {
                println!("{}", style.dim(&messages::bathos_absent()));
                seal_note = "증거     trace.seal chain_head=… — bathos audit 보류".to_string();
            }
            Err(SealError::AlreadySealed) => return exit::EXIT_USAGE,
            Err(e @ (SealError::Chain(_) | SealError::Audit { .. })) => {
                return platform_error(format!("seal 실패: {e}"), Some(session_id));
            }
        }
    }

    // Post-run tallies derive from the RECORDED chain (the evidence, not bookkeeping).
    let (gate_pass, gate_fail, handoffs) = match load_events(root, &session_id) {
        Ok(events) => (
            events
                .iter()
                .filter(|e| e.kind == EventKind::GatePass)
                .count(),
            events
                .iter()
                .filter(|e| e.kind == EventKind::GateFail)
                .count(),
            events
                .iter()
                .filter(|e| e.kind == EventKind::HandoffRequest)
                .count(),
        ),
        Err(e) => return e,
    };
    // Token totals come from the ledger (CLI-4's source of truth) — same file the
    // metering wrote; a read after the run cannot race the single writer.
    let spent = Ledger::open(&session_id, &checkpoint_db.display().to_string())
        .ok()
        .and_then(|l| l.session_totals().ok())
        .map(|t| t.total());

    render_summary(
        style,
        outcome,
        session_id,
        plan_hash,
        &SummaryNumbers {
            gate_pass,
            gate_fail,
            handoffs,
            spent,
            seal_note: &seal_note,
        },
    );

    if code != exit::EXIT_OK {
        let reason_line = outcome.reason.map(messages::reason_message);
        if outcome.final_state == hesmos_core::SessionState::Suspended {
            println!(
                "{}",
                messages::suspended_resume(
                    &session_id.to_string(),
                    last_commit(root, &session_id).as_ref().map(|c| c.0)
                )
            );
        }
        if let Some(line) = reason_line {
            println!("{line}");
        }
        emit_error(&HesmosError {
            class: ErrorClass::Reason,
            code: outcome
                .reason
                .map(hesmos_core::code_to_static)
                .unwrap_or("CANCELLED")
                .to_string(),
            session_id: Some(session_id),
            node_id: None,
            message: format!(
                "세션 종료: {} (exit {code})",
                format!("{:?}", outcome.final_state).to_uppercase()
            ),
            hint: None,
        });
    }
    code
}

pub(crate) struct SummaryNumbers<'a> {
    pub gate_pass: usize,
    pub gate_fail: usize,
    pub handoffs: usize,
    pub spent: Option<u64>,
    pub seal_note: &'a str,
}

/// The §3.5 exit-summary block — a pure function of the outcome + recorded numbers.
fn render_summary(
    style: &Style,
    outcome: &RunOutcome,
    session_id: SessionId,
    plan_hash: &Sha256Hex,
    n: &SummaryNumbers<'_>,
) {
    let badge = match outcome.final_state {
        hesmos_core::SessionState::Completed => format!("── COMPLETED {}", "─".repeat(41)),
        hesmos_core::SessionState::Suspended => format!("── SUSPENDED {}", "─".repeat(41)),
        hesmos_core::SessionState::Halted => {
            format!("── HALTED {} {}", style.fail(), "─".repeat(39))
        }
        hesmos_core::SessionState::Failed => {
            format!("── FAILED {} {}", style.fail(), "─".repeat(39))
        }
        other => format!("── {:?} {}", other, "─".repeat(45)),
    };
    println!("{badge}");
    println!(
        "  exit {:<3} tokens {} / unbounded   min score {}",
        reason_exit(
            outcome.reason,
            outcome.final_state == hesmos_core::SessionState::Suspended
        ),
        crate::tokens::commas(n.spent.unwrap_or(0)),
        outcome
            .min_score
            .map(|s| format!("{s:.2}"))
            .unwrap_or_else(|| "-".into()),
    );
    println!(
        "  게이트   PASS {} · CONCERNS {} · FAIL {}   handoffs {}",
        n.gate_pass, outcome.concerns, n.gate_fail, n.handoffs
    );
    println!("  {}", n.seal_note);
    println!(
        "  세션     {session_id} — hesmos trace show {session_id}   hesmos trace replay {session_id}  plan={}",
        crate::tokens::hash8(plan_hash.as_str())
    );
}

pub(crate) fn load_events(
    root: &Path,
    session_id: &SessionId,
) -> Result<Vec<hesmos_core::TraceEvent>, i32> {
    let log = match EventLog::open(root, session_id) {
        Ok(l) => l,
        Err(e) => {
            return Err(platform_error(
                format!("trace log open 실패: {e}"),
                Some(*session_id),
            ));
        }
    };
    match log.load() {
        LoadOutcome::Ok(events) => Ok(events),
        LoadOutcome::Tampered { fault, .. } => Err(platform_error(
            messages::evidence_invalid(&format!("{fault:?}")),
            Some(*session_id),
        )),
        LoadOutcome::Io(e) => Err(platform_error(
            format!("trace를 읽을 수 없습니다 — {e}"),
            Some(*session_id),
        )),
    }
}

/// The last WAL commit seq (the resume point Suspend guidance names).
pub(crate) fn last_commit(root: &Path, session_id: &SessionId) -> Option<hesmos_core::CommitSeq> {
    let (_, _, checkpoint_db) = session_paths(root, session_id);
    hesmos_orchestrator::SessionWal::open(&checkpoint_db)
        .ok()?
        .commit_seqs(session_id)
        .ok()?
        .pop()
}

/// The origin link of a fork session (WAL row), rendered on the fork banner.
fn fork_of_note(root: &Path, session_id: &SessionId) -> Option<String> {
    let (_, _, checkpoint_db) = session_paths(root, session_id);
    let wal = hesmos_orchestrator::SessionWal::open(&checkpoint_db).ok()?;
    let row = wal.session_row(session_id).ok()??;
    row.fork_of
}

/// RunnerError → exit band: compile stays 3; local store/I-O failures are the
/// platform band (30) — there is no session to keep evidence for, but nothing about
/// the invocation was wrong either.
fn runner_error(e: hesmos_orchestrator::RunnerError, session_id: SessionId) -> i32 {
    match e {
        hesmos_orchestrator::RunnerError::Compile(c) => compile_error(&c),
        other => platform_error(format!("세션 저장소 오류: {other}"), Some(session_id)),
    }
}

/// OS-entropy seed (CLI-1: `--seed` absent → entropy, recorded in the banner). A fresh
/// `RandomState` draws its keys from the OS and an empty SipHash finish() is keyed by
/// them — 64 entropy-derived bits with zero dependencies.
fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// Fork regeneration guard, shared by `trace replay` (CLI-3) and `hesmos eval`
/// (CLI-5). `derive_fork_session_id` is DETERMINISTIC, so re-running the same
/// replay/eval case lands on an existing fork dir. When the occupant IS this fork
/// (WAL lineage == this origin at this commit) it is a stale reproduction result
/// and is removed for regeneration (`Ok(true)`); any other occupant — or a dir
/// without a verifiable WAL row — refuses (`Err`: USAGE-STATE). Arbitrary same-id
/// re-runs stay banned, and no lineage means no permission to overwrite.
pub(crate) fn clear_stale_fork(
    root: &Path,
    origin: &SessionId,
    fork_session: &SessionId,
    at: hesmos_core::CommitSeq,
) -> Result<bool, i32> {
    let (fork_dir, _, fork_db) = session_paths(root, fork_session);
    if !fork_dir.exists() {
        return Ok(false);
    }
    let lineage_ok = hesmos_orchestrator::SessionWal::open(&fork_db)
        .ok()
        .and_then(|w| w.session_row(fork_session).ok().flatten())
        .and_then(|r| r.fork_of)
        .and_then(|f| hesmos_orchestrator::parse_fork_of(&f))
        .is_some_and(|(o, a)| o == *origin && a == at);
    if !lineage_ok {
        return Err(usage_error(format!(
            "세션 디렉터리가 이미 존재합니다: {} — fork ID가 계보 불명의 세션과 충돌합니다 (USAGE-STATE)",
            fork_dir.display()
        )));
    }
    std::fs::remove_dir_all(&fork_dir)
        .map_err(|e| platform_error(format!("fork 재생성 실패: {e}"), Some(*fork_session)))?;
    Ok(true)
}

/// Builds a fork plan for `trace replay`: load the origin's compiled-plan snapshot,
/// verify it still hashes to the recorded plan_hash (evidence check), return it.
pub(crate) fn load_fork_plan(
    root: &Path,
    origin: &SessionId,
    origin_plan_hash: &str,
) -> Result<Plan, i32> {
    let (dir, _, _) = session_paths(root, origin);
    let text = std::fs::read_to_string(dir.join("plan_compiled.yaml")).map_err(|e| {
        platform_error(
            format!("원본 계획 스냅샷(plan_compiled.yaml)을 읽을 수 없습니다 — {e}"),
            Some(*origin),
        )
    })?;
    let plan: Plan = serde_yaml_ng::from_str(&text)
        .map_err(|e| platform_error(format!("원본 계획 스냅샷 파싱 실패 — {e}"), Some(*origin)))?;
    if canonical_sha256(&plan).as_str() != origin_plan_hash {
        return Err(platform_error(
            "계획 스냅샷의 plan_hash가 세션 기록과 불일치 — 원본 세션의 증거가 손상됐습니다".into(),
            Some(*origin),
        ));
    }
    Ok(plan)
}
