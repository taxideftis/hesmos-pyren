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

    crate::cmd::run::run_session(
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
    )
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
