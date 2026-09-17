//! CLI-4 `hesmos budget` — session spend view and team-scope aggregation. Idempotent
//! read-only: totals come from the metering ledger (CLI-4's source of truth), the warn
//! history from the session's budget.event records (the ledger has no `level` column —
//! the event log is where the crossings were recorded).

use std::path::Path;

use hesmos_budget::Ledger;
use hesmos_core::{BudgetEnvelope, EventKind, SessionId};
use hesmos_orchestrator::SessionWal;

use crate::cmd::run::{load_events, platform_error, session_missing, usage_error};
use crate::composition::session_paths;
use crate::exit;
use crate::messages;
use crate::tokens::{Style, bar, commas};

pub struct BudgetArgs {
    pub session_id: Option<String>,
    pub team: Option<String>,
    pub json: bool,
}

pub fn execute(args: BudgetArgs, root: &Path) -> i32 {
    let style = Style::detect();
    match (&args.session_id, &args.team) {
        // Both absent is the documented usage error (CLI-4 exit 2).
        (None, None) => usage_error(
            "세션 ID 또는 --team 중 하나는 필수입니다 — 예: hesmos budget <session_id> | hesmos budget --team <team_id>"
                .into(),
        ),
        (Some(id), _) => show_session(id, args.json, root, &style),
        (None, Some(team)) => show_team(team, args.json, root),
    }
}

fn open_ledger(session_id: &SessionId, root: &Path) -> Result<Ledger, i32> {
    let (_, events_path, checkpoint_db) = session_paths(root, session_id);
    if !events_path.exists() {
        return Err(session_missing(session_id));
    }
    Ledger::open(session_id, &checkpoint_db.display().to_string())
        .map_err(|e| platform_error(format!("ledger open 실패: {e}"), Some(*session_id)))
}

fn show_session(id: &str, json: bool, root: &Path, style: &Style) -> i32 {
    let Some(session_id) = hesmos_orchestrator::parse_session_id(id) else {
        return usage_error(format!("세션 ID 파싱 실패: `{id}`"));
    };
    let ledger = match open_ledger(&session_id, root) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let totals = match ledger.session_totals() {
        Ok(t) => t,
        Err(e) => return platform_error(format!("ledger 조회 실패: {e}"), Some(session_id)),
    };
    let (limit, state, team) = session_facts(root, &session_id);
    let events = match load_events(root, &session_id) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let warnings: Vec<(String, u64, u64)> = events
        .iter()
        .filter(|e| e.kind == EventKind::BudgetEvent)
        .map(|e| {
            (
                e.attrs.get_str("level").unwrap_or("?").to_string(),
                e.attrs.get_u64("spent").unwrap_or(0),
                e.attrs.get_u64("remaining").unwrap_or(0),
            )
        })
        .collect();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "session_id": session_id.to_string(),
                "team_id": team,
                "state": state,
                "spent": { "tokens_in": totals.tokens_in, "tokens_out": totals.tokens_out, "total": totals.total() },
                "limit": limit,
                "budget_events": warnings.iter().map(|(l, s, r)| serde_json::json!({
                    "level": l, "spent": s, "remaining": r
                })).collect::<Vec<_>>(),
            })
        );
        return exit::EXIT_OK;
    }

    println!(
        "session {}   team={}   status={}",
        crate::tokens::hash8(&session_id.to_string()),
        team.as_deref().unwrap_or("-"),
        state,
    );
    match limit {
        Some(cap) => println!(
            "  {}  {} / {} tokens   {}%",
            bar(totals.total(), Some(cap), style),
            commas(totals.total()),
            commas(cap),
            if cap == 0 {
                100
            } else {
                totals.total().min(cap) * 100 / cap
            }
        ),
        None => println!(
            "  {}  {} tokens (unbounded)",
            bar(totals.total(), None, style),
            commas(totals.total())
        ),
    }
    if warnings.is_empty() {
        println!("경고 이력 없음");
    } else {
        println!("경고 이력");
        for (level, spent, remaining) in &warnings {
            println!(
                "  budget.event  level={level:<8} spent={}  remaining={}",
                commas(*spent),
                commas(*remaining)
            );
        }
    }
    if state.contains("SUSPENDED") {
        println!(
            "{}",
            messages::suspended_resume(&session_id.to_string(), None)
        );
    }
    exit::EXIT_OK
}

/// Team-scope view: every session ledger under the root, filtered to `team`. The P1
/// store is one checkpoint.db per session, so the aggregation walks the session dirs —
/// the ledger's denormalized team_id column (AP-4) is what makes each row filterable
/// without joins.
fn show_team(team: &str, _json: bool, root: &Path) -> i32 {
    let sessions_dir = root.join(".hesmos").join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return usage_error(format!(
            "세션 저장소가 없습니다: {} — 먼저 hesmos run으로 세션을 만드세요",
            sessions_dir.display()
        ));
    };
    println!("team {team}");
    let mut any = false;
    let mut total = 0u64;
    let mut rows: Vec<(String, u64, u64)> = Vec::new();
    for entry in entries.flatten() {
        let db = entry.path().join("checkpoint.db");
        if !db.exists() {
            continue;
        }
        let Some(sid) =
            hesmos_orchestrator::parse_session_id(entry.file_name().to_string_lossy().as_ref())
        else {
            continue;
        };
        let Ok(ledger) = Ledger::open(&sid, &db.display().to_string()) else {
            continue;
        };
        let Ok(by_team) = ledger.totals_by_team() else {
            continue;
        };
        let Some(totals) = by_team
            .into_iter()
            .find(|(t, _)| t.as_deref() == Some(team))
            .map(|(_, t)| t)
        else {
            continue;
        };
        rows.push((sid.to_string(), totals.tokens_in, totals.tokens_out));
        total += totals.total();
        any = true;
    }
    rows.sort();
    for (sid, tin, tout) in &rows {
        println!(
            "  session {}   in {} · out {}",
            crate::tokens::hash8(sid),
            commas(*tin),
            commas(*tout)
        );
    }
    if !any {
        println!("  (이 팀의 계상 이력 없음)");
    }
    println!("  합계 {} tokens · 세션 {}개", commas(total), rows.len());
    exit::EXIT_OK
}

/// (limit, state, team) from the WAL row — the frozen envelope and the terminal state.
fn session_facts(root: &Path, session_id: &SessionId) -> (Option<u64>, String, Option<String>) {
    let (_, _, checkpoint_db) = session_paths(root, session_id);
    let Ok(wal) = SessionWal::open(&checkpoint_db) else {
        return (None, "UNKNOWN".into(), None);
    };
    let Ok(Some(row)) = wal.session_row(session_id) else {
        return (None, "UNKNOWN".into(), None);
    };
    let limit = serde_json::from_str::<BudgetEnvelope>(&row.budget_json)
        .ok()
        .and_then(|b| b.session_max_tokens);
    (limit, row.state, row.team_id)
}
