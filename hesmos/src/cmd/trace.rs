//! CLI-2 `trace show` + CLI-3 `trace replay`.
//!
//! show: full-chain verification FIRST (a tampered log renders with the
//! EVIDENCE-INVALID badge and exits 30 — display material is kept for investigation,
//! but it is labelled untrusted). The default view folds llm/tool calls per node
//! (W3-4); `--gate`/`--handoff` filter views never fold. Commit markers (W3-3) join
//! the WAL's commit rows to each boundary's final gate.pass — the WAL is the commit
//! authority, the chain is the display material.
//!
//! replay: fork at a commit point — new session id, `fork_of` lineage, origin response
//! cache reuse. NOT_COMMIT_POINT is a usage error (exit 2) that lists valid points.

use std::path::Path;

use hesmos_core::{BudgetEnvelope, CommitSeq, EventKind, SessionId};
use hesmos_orchestrator::SessionWal;
use hesmos_trace::LoadOutcome;

use crate::cmd::run::{emit_error, load_events, platform_error, session_missing, usage_error};
use crate::composition::session_paths;
use crate::exit;
use crate::messages;
use crate::tokens::Style;

pub struct ShowArgs {
    pub session_id: String,
    pub gate: bool,
    pub handoff: bool,
    pub limit: usize,
    pub json: bool,
}

pub struct ReplayArgs {
    pub session_id: String,
    /// `None` = whole-session replay from the start (fork point 0).
    pub at: Option<u64>,
    pub budget: Option<String>,
}

pub fn show(args: ShowArgs, root: &Path) -> i32 {
    let style = Style::detect();
    let Some(session_id) = parse_session(&args.session_id) else {
        return usage_error(format!(
            "세션 ID 파싱 실패: `{}` — run 출력의 session 필드 값을 사용하세요",
            args.session_id
        ));
    };
    let (_, events_path, checkpoint_db) = session_paths(root, &session_id);
    if !events_path.exists() {
        return session_missing(&session_id);
    }

    // Render-side chain load: Tampered still yields the parsed prefix for the
    // investigation render, tagged with the fault (SS-02 rule 2 / ui-spec §4.4).
    // Deliberately NOT EventLog::open — its eager scan protects APPENDS and refuses
    // a tampered file; an inspector renders the readable prefix with the badge.
    let (events, fault) = match hesmos_trace::load_path(&events_path) {
        LoadOutcome::Ok(events) => (events, None),
        LoadOutcome::Tampered { events, fault } => (events, Some(fault)),
        LoadOutcome::Io(e) => {
            return platform_error(format!("trace를 읽을 수 없습니다 — {e}"), Some(session_id));
        }
    };

    // Header facts come from the chain itself (session.open / plan.compiled /
    // session.close), so the render survives even when checkpoint.db is absent.
    let mut seed_note = String::from("?");
    let mut budget_note = String::from("?");
    let mut plan_note = String::from("?");
    let mut status = "RUNNING".to_string();
    for e in &events {
        match e.kind {
            EventKind::SessionOpen => {
                seed_note = e.attrs.get_u64("seed").unwrap_or(0).to_string();
                budget_note = e.attrs.get_str("budget").unwrap_or("?").to_string();
            }
            EventKind::PlanCompiled => {
                plan_note = e
                    .attrs
                    .get_str("plan_hash")
                    .map(crate::tokens::hash8)
                    .unwrap_or_else(|| "?".into());
            }
            EventKind::SessionClose => {
                status = e.attrs.get_str("final_state").unwrap_or("?").to_string();
            }
            _ => {}
        }
    }

    // Commit markers: WAL rows are the commit authority. k-th commit decorates the
    // k-th boundary-final gate.pass (in seq order) — the pass immediately preceding
    // the commit (only handoff/node.stop events sit between them).
    let markers = commit_markers(&events, &checkpoint_db, &session_id);
    let marker_count = markers.len();

    if args.json {
        // --json: the raw stream, no fold, no color — machine-readable (CLI-2).
        for e in &events {
            println!(
                "{}",
                serde_json::to_string(e).unwrap_or_else(|_| "{}".into())
            );
        }
    } else {
        println!(
            "session {}   seed={}   plan={}   budget={}   status={}",
            crate::tokens::hash8(&session_id.to_string()),
            seed_note,
            plan_note,
            budget_note,
            status
        );
        println!(
            "events {}   gate pass/fail {}/{}   handoffs {}",
            events.len(),
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
        );
        if let Some(fault) = &fault {
            println!(
                "{} EVIDENCE-INVALID — {} (이후 행은 조사용 렌더입니다)",
                style.fail(),
                crate::tokens::hash8(&format!("{fault:?}"))
            );
        }
        println!();
        println!("   #    event             details");
        render_rows(
            &style,
            &events,
            args.gate,
            args.handoff,
            args.limit,
            &markers,
        );
        println!();
        println!(
            "chain {} events · head {} · {}",
            events.len(),
            events
                .last()
                .map(|e| crate::tokens::hash8(e.hash.as_str()))
                .unwrap_or_else(|| "-".into()),
            sealed_note(&events)
        );
        if marker_count > 0 {
            println!(
                "커밋 지점 {marker_count}개 — 분기 재실행: hesmos trace replay {} --at step {marker_count}",
                args.session_id
            );
        }
    }

    match fault {
        None => exit::EXIT_OK,
        Some(fault) => platform_error(
            messages::evidence_invalid(&format!("{fault:?}")),
            Some(session_id),
        ),
    }
}

/// `trace replay` — fork at `--at` (CLI-3). All reproduction inputs are read from the
/// origin session; `--budget` re-pins the fork envelope (SS-15 rule 1).
pub fn replay(args: ReplayArgs, root: &Path) -> i32 {
    let style = Style::detect();
    let Some(origin) = parse_session(&args.session_id) else {
        return usage_error(format!("세션 ID 파싱 실패: `{}`", args.session_id));
    };
    let (origin_dir, events_path, checkpoint_db) = session_paths(root, &origin);
    if !events_path.exists() {
        return session_missing(&origin);
    }

    // Origin facts from the WAL (the reproduction basis — seed, plan hash, budget).
    let wal = match SessionWal::open(&checkpoint_db) {
        Ok(w) => w,
        Err(e) => return platform_error(format!("원본 WAL open 실패: {e}"), Some(origin)),
    };
    let row = match wal.session_row(&origin) {
        Ok(Some(row)) => row,
        Ok(None) => return session_missing(&origin),
        Err(e) => return platform_error(format!("원본 세션 행 조회 실패: {e}"), Some(origin)),
    };
    let commits = match wal.commits(&origin) {
        Ok(c) => c,
        Err(e) => return platform_error(format!("커밋 조회 실패: {e}"), Some(origin)),
    };

    // Commit-point validation BEFORE anything runs (USAGE-NOT-COMMIT-POINT, exit 2).
    let at_seq = match args.at {
        None => CommitSeq(0),
        Some(n) => {
            let point = CommitSeq(n);
            if !commits.iter().any(|c| c.seq == point) {
                let valid: Vec<u64> = commits.iter().map(|c| c.seq.0).collect();
                emit_error(&hesmos_core::HesmosError {
                    class: hesmos_core::ErrorClass::Usage,
                    code: "USAGE-NOT-COMMIT-POINT".into(),
                    session_id: Some(origin),
                    node_id: None,
                    message: messages::not_commit_point(&args.session_id, n, &valid),
                    hint: None,
                });
                return exit::EXIT_USAGE;
            }
            point
        }
    };

    // The origin chain must be verified evidence before it forks anything (SS-02
    // rule 2 — a tampered log is never a reproduction basis).
    if let Err(code) = load_events(root, &origin) {
        return code;
    }

    let plan = match crate::cmd::run::load_fork_plan(root, &origin, &row.plan_hash) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let budget = match &args.budget {
        Some(spec) => match crate::composition::parse_budget_spec(spec) {
            Ok(b) => b,
            Err(msg) => return usage_error(msg),
        },
        None => serde_json::from_str::<BudgetEnvelope>(&row.budget_json).unwrap_or_default(),
    };

    let fork_session = hesmos_orchestrator::derive_fork_session_id(&origin, at_seq);

    // CLI-3 idempotency (ui-spec §5.2): a second identical replay regenerates its
    // stale fork (see the shared guard for the lineage rules).
    match crate::cmd::run::clear_stale_fork(root, &origin, &fork_session, at_seq) {
        Ok(true) => println!(
            "{}",
            style.dim("기존 fork 재생성 — 이전 재현 결과를 지우고 다시 실행합니다")
        ),
        Ok(false) => {}
        Err(code) => return code,
    }

    println!(
        "재현 요소: session_id={}  plan_hash={}  seed={}  cache={}",
        origin,
        crate::tokens::hash8(&row.plan_hash),
        row.seed,
        if at_seq.0 == 0 {
            "미사용 (전체 재생)"
        } else {
            "origin responses"
        }
    );
    println!(
        "커밋 #{} 에서 분기 재실행 — 새 세션 {} (fork_of={}/#{})",
        at_seq.0, fork_session, origin, at_seq.0
    );

    let (code, outcome) = crate::cmd::run::run_session(
        root,
        &style,
        plan,
        fork_session,
        Some(row.seed),
        budget,
        row.team_id
            .as_deref()
            .map(hesmos_orchestrator::parse_team_id),
        Some(ForkSource {
            origin,
            at: at_seq,
            origin_responses: origin_dir.join("responses"),
        }),
    );
    render_reproduction_summary(root, &style, &origin, &fork_session, &outcome);
    code
}

/// The ui-spec §5.2 reproduction summary — structural comparison of origin vs fork
/// (AP-6 projection via [`hesmos_trace::compare_structure`]), commit replay count,
/// and the W3-7 verdict line (same reason_code ⇒ "장애 재현"). Rendered after EVERY
/// replay that actually ran; a composition failure (no outcome) has nothing to
/// compare and stays silent.
fn render_reproduction_summary(
    root: &Path,
    style: &Style,
    origin: &SessionId,
    fork: &SessionId,
    outcome: &Option<hesmos_orchestrator::RunOutcome>,
) {
    let Some(_) = outcome else { return };
    let Ok(origin_events) = load_events(root, origin) else {
        return;
    };
    let Ok(fork_events) = load_events(root, fork) else {
        return;
    };

    println!("── 재현 대조 {}", "─".repeat(46));

    // Structure: path (node starts) + gate verdicts, projected on every read.
    let cmp = hesmos_trace::compare_structure(
        &hesmos_trace::project_structure(&origin_events),
        &hesmos_trace::project_structure(&fork_events),
    );
    if cmp.matches {
        let o = hesmos_trace::project_structure(&origin_events);
        println!(
            "  구조 대조 — 경로 {}/{} · 게이트 판정 {}/{} 일치",
            o.nodes.len(),
            o.nodes.len(),
            o.gates.len(),
            o.gates.len()
        );
    } else {
        println!(
            "{} 구조 불일치 — 원본과 다른 실행 구조가 관측됐습니다 (아래 차이 항목)",
            style.fail()
        );
        for m in &cmp.mismatches {
            println!("    {}", mismatch_line(m.clone()));
        }
    }

    // Commits replayed: the fork's own WAL rows (checkpoint authority), not guesses.
    let fork_db = session_paths(root, fork).2;
    let fork_commits = SessionWal::open(&fork_db)
        .ok()
        .and_then(|w| w.commits(fork).ok())
        .map(|c| c.len());
    if let Some(n) = fork_commits {
        println!("  커밋 재생 — fork 커밋 {n}개 기록");
    }

    // W3-7 verdict: same terminal state AND same reason code ⇒ the failure
    // reproduced (표 10 P3). Reasons derive from each chain's last gate.fail —
    // the WAL keeps no reason column.
    let (origin_state, origin_reason) = terminal_facts(&origin_events);
    let (fork_state, fork_reason) = terminal_facts(&fork_events);
    if origin_state == fork_state && origin_reason == fork_reason {
        match fork_reason {
            Some(r) => println!(
                "{} 장애 재현 — 원본과 동일 reason_code={r} (표 10 P3)",
                style.pass()
            ),
            None => println!(
                "{} 재현 성공 — 원본과 동일하게 {fork_state} 종결",
                style.pass()
            ),
        }
    } else {
        println!(
            "{} 재현 불일치 — 원본 {}/{} ≠ 재현 {}/{}",
            style.fail(),
            origin_state,
            origin_reason.as_deref().unwrap_or("-"),
            fork_state,
            fork_reason.as_deref().unwrap_or("-")
        );
    }
}

/// Terminal facts from a chain: final_state (session.close) + the terminal reason
/// (last gate.fail's reason_code, or a suspend-line BudgetEvent for budget
/// suspends). Provider halts record no gate.fail → reason None → rendered "-".
fn terminal_facts(events: &[hesmos_core::TraceEvent]) -> (String, Option<String>) {
    let mut state = String::from("RUNNING");
    let mut reason = None;
    for e in events {
        match e.kind {
            EventKind::GateFail => reason = e.attrs.get_str("reason_code").map(String::from),
            // A suspend-line BudgetEvent IS the recorded terminal reason for a
            // budget suspend (SS-15): budget suspends never pass through a gate,
            // so this event is the only evidence of WHY the session suspended.
            EventKind::BudgetEvent if e.attrs.get_str("level") == Some("suspend") => {
                reason = Some("BUDGET_EXCEEDED".into())
            }
            EventKind::SessionClose => {
                state = e.attrs.get_str("final_state").unwrap_or("?").to_string()
            }
            _ => {}
        }
    }
    (state, reason)
}

/// One structural difference → a diagnosis line (기대 vs 실제, in event order).
/// Shared with `hesmos eval`, whose regression diff uses the same rendering — one
/// mismatch always reads the same way, whichever command observed it.
pub(crate) fn mismatch_line(m: hesmos_trace::StructureMismatch) -> String {
    use hesmos_trace::{GateStep, StructureMismatch as M, VerdictStep};
    fn verdict_desc(g: &GateStep) -> String {
        match g.verdict {
            VerdictStep::Pass => "pass".into(),
            VerdictStep::Fail => match (&g.reason_code, g.attempts_left) {
                (Some(r), Some(n)) => format!("fail({r}, 남은 {n}회)"),
                (Some(r), None) => format!("fail({r})"),
                (None, _) => "fail".into(),
            },
        }
    }
    match m {
        M::NodeCount { expected, actual } => {
            format!("경로 길이 — 기대 {expected}단계 ≠ 실제 {actual}단계")
        }
        M::NodeStep {
            index,
            expected,
            actual,
        } => format!(
            "경로[{}] — 기대 {}(wave {}) ≠ 실제 {}(wave {})",
            index, expected.node, expected.wave, actual.node, actual.wave
        ),
        M::GateCount { expected, actual } => {
            format!("게이트 수 — 기대 {expected}판정 ≠ 실제 {actual}판정")
        }
        M::GateStep {
            index,
            expected,
            actual,
        } => format!(
            "게이트[{}] {} — 기대 {} ≠ 실제 {}",
            index,
            expected.gate_id,
            verdict_desc(&expected),
            verdict_desc(&actual)
        ),
    }
}

use hesmos_orchestrator::ForkSource;

/// Session id text → [`SessionId`] (the WAL's parser, reused so one spelling rules).
fn parse_session(text: &str) -> Option<SessionId> {
    hesmos_orchestrator::parse_session_id(text)
}

/// (gate.pass event seq → commit seq) markers. Structural rule: within one boundary
/// (node.start .. next boundary), the LAST gate.pass is the committing pass; the WAL's
/// k-th commit row consumes it. Crash windows (events appended, WAL txn missing) yield
/// NO marker — an uncheckpointed pass is not a commit point (SS-14).
fn commit_markers(
    events: &[hesmos_core::TraceEvent],
    checkpoint_db: &Path,
    session_id: &SessionId,
) -> Vec<(u64, u64)> {
    let Ok(wal) = SessionWal::open(checkpoint_db) else {
        return Vec::new();
    };
    let Ok(commits) = wal.commits(session_id) else {
        return Vec::new();
    };
    boundary_final_passes(events)
        .into_iter()
        .zip(commits.iter().map(|c| c.seq.0))
        .collect()
}

/// Seqs of each boundary's final gate.pass, in event order.
fn boundary_final_passes(events: &[hesmos_core::TraceEvent]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut last_pass: Option<u64> = None;
    for e in events {
        match e.kind {
            EventKind::GatePass => last_pass = Some(e.seq),
            // The boundary's terminal gate state decides: a gate.fail (reject or
            // retry-restart) clears the pending pass — a pass followed by a fail was
            // NOT the committing pass. The next boundary's G0/pre passes then re-seed
            // it.
            EventKind::GateFail => last_pass = None,
            // node.stop fires once per boundary AFTER route/commit — the pending pass
            // here is the committing one. session.close ends the final boundary.
            // (node.start is NOT a flush point: the next boundary's G0 pass precedes
            // it and must not steal the previous boundary's marker.)
            EventKind::NodeStop | EventKind::SessionClose => {
                if let Some(seq) = last_pass.take() {
                    out.push(seq);
                }
            }
            _ => {}
        }
    }
    // Defensive trailing window (e.g. a crash between the last pass and node.stop):
    // extra candidates are dropped by the zip against WAL commit rows.
    if let Some(seq) = last_pass {
        out.push(seq);
    }
    out
}

fn sealed_note(events: &[hesmos_core::TraceEvent]) -> String {
    match events.last() {
        Some(e) if e.kind == EventKind::TraceSeal => format!(
            "sealed (chain_head={})",
            crate::tokens::hash8(e.attrs.get_str("chain_head_hash").unwrap_or("?"))
        ),
        _ => "unsealed (실행 중 또는 미완료 — seal은 종료 시)".to_string(),
    }
}

/// The event-table renderer. Filtering (US-05 AC1) and the W3-4 fold are mutually
/// exclusive by design: a filter view shows every matching row raw.
fn render_rows(
    style: &Style,
    events: &[hesmos_core::TraceEvent],
    gate: bool,
    handoff: bool,
    limit: usize,
    markers: &[(u64, u64)],
) {
    let filtered = gate || handoff;
    let keep = |e: &hesmos_core::TraceEvent| {
        (!filtered)
            || (gate && matches!(e.kind, EventKind::GatePass | EventKind::GateFail))
            || (handoff && matches!(e.kind, EventKind::HandoffRequest | EventKind::HandoffAccept))
    };

    if filtered {
        let mut shown = 0usize;
        for e in events {
            if !keep(e) {
                continue;
            }
            if shown == limit {
                println!(
                    "{}  {}행 생략 — --limit {}으로 확장",
                    style.dim("…"),
                    events.len() - shown,
                    limit * 2
                );
                break;
            }
            println!("{:>4}  {}", e.seq, detail_line(style, e));
            shown += 1;
        }
        return;
    }

    // Default view: fold llm/tool per boundary into ONE row at the first folded
    // position (W3-4 — 건수·누적 시간).
    let mut rows: Vec<String> = Vec::new();
    let mut fold: Option<(u64, u64, u64, u64)> = None; // (count llm, count tool, sum ms, first seq)
    let mut emitted_markers = 0usize;
    for e in events {
        match e.kind {
            EventKind::LlmCall | EventKind::ToolCall => {
                let slot = fold.get_or_insert((0, 0, 0, e.seq));
                if e.kind == EventKind::LlmCall {
                    slot.0 += 1;
                } else {
                    slot.1 += 1;
                }
                slot.2 += e.attrs.get_u64("latency_ms").unwrap_or(0);
            }
            _ => {
                if let Some((llm, tool, ms, seq)) = fold.take() {
                    rows.push(fold_line(seq, llm, tool, ms, style));
                }
                let marker = if e.kind == EventKind::GatePass
                    && emitted_markers < markers.len()
                    && markers[emitted_markers].0 == e.seq
                {
                    emitted_markers += 1;
                    format!(
                        " {} commit #{}",
                        style.pass(),
                        markers[emitted_markers - 1].1
                    )
                } else {
                    String::new()
                };
                rows.push(format!("{:>4}  {}{}", e.seq, detail_line(style, e), marker));
            }
        }
    }
    if let Some((llm, tool, ms, seq)) = fold {
        rows.push(fold_line(seq, llm, tool, ms, style));
    }

    for (i, row) in rows.iter().enumerate() {
        if i == limit {
            println!(
                "{}  {}행 생략 — --limit {}으로 확장",
                style.dim("…"),
                rows.len() - limit,
                limit * 2
            );
            break;
        }
        println!("{row}");
    }
}

fn fold_line(seq: u64, llm: u64, tool: u64, ms: u64, style: &Style) -> String {
    let mut parts = Vec::new();
    if llm > 0 {
        parts.push(format!("llm.call {llm}"));
    }
    if tool > 0 {
        parts.push(format!("tool.call {tool}"));
    }
    format!(
        "{:>4}  {} ({} 생략 — {:.1}s)",
        seq,
        style.dim("⋮"),
        parts.join(" · "),
        ms as f64 / 1000.0
    )
}

/// One event → its summary line (§4.2 행 형식: seq · event · 요약 속성).
fn detail_line(style: &Style, e: &hesmos_core::TraceEvent) -> String {
    let a = &e.attrs;
    let body = match e.kind {
        EventKind::GatePass => format!(
            "gate_id={} score={:.2}",
            a.get_str("gate_id").unwrap_or("?"),
            a.get_f32("score").unwrap_or(0.0)
        ),
        EventKind::GateFail => {
            // SS-18 cache violation: the ui-spec §8.2 display phrase, derived from the
            // event's gate_id="cache" (the runner emits a plain gate.fail with
            // reason_code GATE_REJECT — CACHE_INVARIANT is display material only).
            if a.get_str("gate_id") == Some("cache") {
                format!(
                    "gate_id=cache {}CACHE_INVARIANT — 시스템 프롬프트 해시 불일치, 세션을 즉시 중단했습니다 (reason=GATE_REJECT)",
                    style.fail()
                )
            } else {
                let retry = a
                    .get_u64("attempts_left")
                    .map(|left| format!(" (retry 시도, 남은 {}회)", left))
                    .unwrap_or_default();
                format!(
                    "gate_id={} {}reason={} score={:.2}{}",
                    a.get_str("gate_id").unwrap_or("?"),
                    style.fail(),
                    a.get_str("reason_code").unwrap_or("?"),
                    a.get_f32("score").unwrap_or(0.0),
                    retry
                )
            }
        }
        EventKind::HandoffRequest => {
            // The request event carries only `from` + the contract hash — the `to` is
            // the ROUTER's decision and lives on the accept event. Rendering a
            // guessed `to` here would fabricate a decision that hadn't happened yet.
            format!(
                "from={} contract={}",
                a.get_str("from").unwrap_or("?"),
                a.get_str("contract_hash")
                    .map(crate::tokens::hash8)
                    .unwrap_or_else(|| "?".into())
            )
        }
        EventKind::HandoffAccept => a.get_str("to").unwrap_or("?").to_string(),
        EventKind::NodeStart => format!(
            "node={} wave={}",
            a.get_str("node_id").unwrap_or("?"),
            a.get_u64("wave").unwrap_or(0)
        ),
        EventKind::NodeStop => format!(
            "node={} stop_kind={}",
            a.get_str("node_id").unwrap_or("?"),
            a.get_str("stop_kind").unwrap_or("?")
        ),
        EventKind::BudgetEvent => format!(
            "level={} spent={} remaining={}",
            a.get_str("level").unwrap_or("?"),
            crate::tokens::commas(a.get_u64("spent").unwrap_or(0)),
            crate::tokens::commas(a.get_u64("remaining").unwrap_or(0))
        ),
        EventKind::SessionClose => {
            format!("final_state={}", a.get_str("final_state").unwrap_or("?"))
        }
        EventKind::PlanCompiled => format!(
            "plan_hash={} nodes={}",
            a.get_str("plan_hash")
                .map(crate::tokens::hash8)
                .unwrap_or_else(|| "?".into()),
            a.get_u64("node_count").unwrap_or(0)
        ),
        EventKind::TraceSeal => format!(
            "chain_head={}",
            a.get_str("chain_head_hash")
                .map(crate::tokens::hash8)
                .unwrap_or_else(|| "?".into())
        ),
        _ => String::new(),
    };
    format!("{:<16} {}", e.kind.as_vocab(), body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::EventAttrs;

    /// Builds a minimal event with the given kind/attrs (seq order is all the
    /// terminal-facts scan needs — no hash linking in this projection).
    fn ev(kind: EventKind, attrs: EventAttrs) -> hesmos_core::TraceEvent {
        hesmos_core::TraceEvent {
            seq: 0,
            kind,
            node: None,
            prev_hash: hesmos_core::Sha256Hex::parse("0".repeat(64)).expect("genesis"),
            hash: hesmos_core::Sha256Hex::parse("0".repeat(64)).expect("genesis"),
            attrs,
            ts: 0,
        }
    }

    /// W3-7's verdict fingerprint must come out of the CHAIN, not bookkeeping:
    /// budget suspends carry their reason on the suspend-line BudgetEvent (no gate
    /// ever failed), gate rejects on the last gate.fail, and a provider halt —
    /// which records neither — reads as "-".
    #[test]
    fn terminal_facts_cover_all_three_reproduction_kinds() {
        // BUDGET_EXCEEDED: suspend-line BudgetEvent, no gate.fail.
        let budget = vec![
            ev(
                EventKind::BudgetEvent,
                EventAttrs::new().set("level", "warn").set("spent", 1u64),
            ),
            ev(
                EventKind::BudgetEvent,
                EventAttrs::new().set("level", "suspend").set("spent", 9u64),
            ),
            ev(
                EventKind::SessionClose,
                EventAttrs::new().set("final_state", "SUSPENDED"),
            ),
        ];
        assert_eq!(
            terminal_facts(&budget),
            ("SUSPENDED".into(), Some("BUDGET_EXCEEDED".into()))
        );

        // GATE_REJECT: the last gate.fail wins over earlier retryable fails.
        let gate = vec![
            ev(
                EventKind::GateFail,
                EventAttrs::new()
                    .set("gate_id", "rubric.v1")
                    .set("reason_code", "GATE_REJECT")
                    .set("attempts_left", 1u64),
            ),
            ev(
                EventKind::GateFail,
                EventAttrs::new()
                    .set("gate_id", "rubric.v1")
                    .set("reason_code", "GATE_REJECT"),
            ),
            ev(
                EventKind::SessionClose,
                EventAttrs::new().set("final_state", "FAILED"),
            ),
        ];
        assert_eq!(
            terminal_facts(&gate),
            ("FAILED".into(), Some("GATE_REJECT".into()))
        );

        // Provider halt: no recorded reason → None (rendered "-").
        let provider = vec![ev(
            EventKind::SessionClose,
            EventAttrs::new().set("final_state", "HALTED"),
        )];
        assert_eq!(terminal_facts(&provider), ("HALTED".into(), None));

        // Still running: no session.close yet.
        assert_eq!(terminal_facts(&[]), ("RUNNING".into(), None));
    }

    /// The diff lines are the operator's diagnosis surface — each mismatch shape
    /// names WHERE structure diverged (경로/게이트, index, 기대 vs 실제).
    #[test]
    fn mismatch_lines_name_the_divergence() {
        use hesmos_trace::{GateStep, NodeStep, StructureMismatch, VerdictStep};
        let line = mismatch_line(StructureMismatch::NodeCount {
            expected: 2,
            actual: 3,
        });
        assert!(line.contains("경로 길이") && line.contains('2') && line.contains('3'));

        let line = mismatch_line(StructureMismatch::NodeStep {
            index: 1,
            expected: NodeStep {
                node: "fetch".into(),
                wave: 0,
            },
            actual: NodeStep {
                node: "verify".into(),
                wave: 1,
            },
        });
        assert!(line.contains("경로[1]") && line.contains("fetch") && line.contains("verify"));

        let line = mismatch_line(StructureMismatch::GateStep {
            index: 0,
            expected: GateStep {
                node: Some("fetch".into()),
                gate_id: "g0".into(),
                verdict: VerdictStep::Pass,
                reason_code: None,
                attempts_left: None,
            },
            actual: GateStep {
                node: Some("fetch".into()),
                gate_id: "g0".into(),
                verdict: VerdictStep::Fail,
                reason_code: Some("GATE_REJECT".into()),
                attempts_left: None,
            },
        });
        assert!(
            line.contains("게이트[0] g0")
                && line.contains("pass")
                && line.contains("fail(GATE_REJECT)")
        );
    }
}
