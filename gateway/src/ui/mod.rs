//! Server-rendered dashboard D1–D4 (read-only) + the two static assets.
//!
//! Technical level is fixed by ui-spec §9.0: server-rendered HTML, one CSS
//! file, minimal JS for WS — no framework, no webfont, no chart library.
//! All styling values are tokens from `design-system/tokens.md §3` (single
//! source) baked as CSS custom properties in `assets/hesmos.css`.
//!
//! Render rules for event rows are the CLI's own (P-02/P-03 of
//! `components.md`, P-10 for HTML): commit markers, external-call folding and
//! hash shorthand are identical to `hesmos trace show`. The initial page is
//! rendered server-side through the same port query HTTP-2 exposes (zero
//! duplicated fetch, no CLS); live appends reuse the identical rules in JS.

mod assets;

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use std::collections::HashMap;

use crate::core_port::{AuditStatus, BudgetReport, CorePort, TraceEventWire};

// ---------------------------------------------------------------------------
// Shared token-ish render helpers (display side of tokens.md §1.4 / §3)
// ---------------------------------------------------------------------------

/// HTML-escape untrusted strings (team ids, roles, ids from the core).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Hash shorthand — 8 chars + ellipsis (tokens §1.4).
fn hash8(h: &str) -> String {
    if h.chars().count() > 8 {
        format!("{}…", h.get(..8).unwrap_or(h))
    } else {
        h.to_string()
    }
}

/// Thousands separators (tokens §1.4).
fn comma(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Integer percentage with rounding — display math only (tokens §1.4).
fn pct_of(spent: u64, limit: u64) -> u64 {
    if limit == 0 {
        return 100;
    }
    ((spent * 100 + limit / 2) / limit).min(100)
}

/// P-05 progress bar — fixed 22 chars; the number is the truth, the bar is an
/// approximation.
fn bar22(spent: u64, limit: u64) -> String {
    let pct = pct_of(spent, limit);
    let filled = ((pct * 22 + 50) / 100).min(22) as usize;
    format!("[{}{}]", "#".repeat(filled), ".".repeat(22 - filled))
}

/// P-09 status chip — symbol + English state word (colour is never the only
/// signal; palette is the CLI 3-colour scheme, tokens §2.1/§3.1). The wire
/// carries TYPE-5's PascalCase spelling; the displayed word is the display
/// vocabulary's uppercase form (`✓ COMPLETED`).
fn chip(state: &str) -> String {
    let (sym, tone) = match state {
        "Completed" => ("✓", "ok"),
        "Suspended" | "Cancelled" => ("‖", "warn"),
        "Halted" | "Failed" => ("✗", "err"),
        "Running" => (">", "neutral"),
        _ => ("·", "neutral"), // Init and any unknown → neutral
    };
    format!(
        "<span class=\"chip {tone}\">{sym} {}</span>",
        esc(&state.to_uppercase())
    )
}

/// Elapsed column — `T+<s>` format from epoch-ms timestamps (P-10).
fn elapsed(ev_ts: u64, base_ts: u64) -> String {
    let secs = ev_ts.saturating_sub(base_ts) as f64 / 1000.0;
    format!("T+{secs:.1}s")
}

// ---------------------------------------------------------------------------
// Page shell + state blocks
// ---------------------------------------------------------------------------

fn shell(title: &str, active: &str, body: &str) -> String {
    let link = |href: &str, label: &str| {
        let cur = if href == active {
            " aria-current=\"page\""
        } else {
            ""
        };
        format!("<a href=\"{href}\"{cur}>{label}</a>")
    };
    format!(
        "<!DOCTYPE html>\n<html lang=\"ko\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title} — hesmos 대시보드</title>\n\
         <link rel=\"stylesheet\" href=\"/assets/hesmos.css\">\n</head>\n<body>\n\
         <header class=\"top\">\n  <p class=\"brand\">hesmos 대시보드</p>\n  \
         <nav aria-label=\"화면 이동\">{}{}{}</nav>\n</header>\n<main>\n{body}\n</main>\n</body>\n</html>",
        link("/", "세션"),
        link("/budgets", "예산"),
        link("/audit", "감사"),
    )
}

fn card(title: &str, inner: &str) -> String {
    format!("<section class=\"card\">\n<h2>{title}</h2>\n{inner}\n</section>")
}

/// Full-screen state block — the loading/empty/error states are first-class
/// screens (ui-spec §2.6); copy comes verbatim from ui-spec §9.2–9.5.
fn state_block(tone: &str, message: &str, action: Option<(&str, &str)>) -> String {
    let action_html = match action {
        Some((label, href)) => format!("\n<p><a class=\"btn\" href=\"{href}\">{label}</a></p>"),
        None => String::new(),
    };
    format!("<div class=\"state {tone}\" role=\"status\">\n<p>{message}</p>{action_html}\n</div>")
}

fn error_page(status: StatusCode, title: &str, message: &str, retry_href: &str) -> Response {
    let body = shell(
        title,
        "",
        &state_block("error", message, Some(("다시 시도", retry_href))),
    );
    (status, Html(body)).into_response()
}

// ---------------------------------------------------------------------------
// D1 — session list (HTTP-1 fields only, newest first)
// ---------------------------------------------------------------------------

pub async fn d1(State(core): State<Arc<dyn CorePort>>) -> Response {
    let rows = match core.list_sessions() {
        Ok(rows) => rows,
        Err(_) => {
            return error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "세션",
                "세션 목록을 불러오지 못했습니다 — 코어에 연결할 수 없습니다.",
                "/",
            );
        }
    };

    let inner = if rows.is_empty() {
        state_block(
            "empty",
            "아직 세션이 없습니다 — hesmos run &lt;plan&gt;으로 첫 세션을 시작하세요",
            None,
        )
    } else {
        let mut trs = String::new();
        // Default order: newest first (시작 역순) — the port lists oldest first.
        for row in rows.iter().rev() {
            let team = row.team_id.as_deref().unwrap_or("—");
            let head = row
                .chain_head
                .as_deref()
                .map(hash8)
                .unwrap_or_else(|| "—".into());
            trs.push_str(&format!(
                "<tr data-state=\"{}\" data-team=\"{}\">\
                 <td>{}</td>\
                 <td><a class=\"session-link\" href=\"/sessions/{}\">{}</a></td>\
                 <td>{}</td>\
                 <td class=\"num\">{}</td>\
                 <td class=\"mono\">{}</td></tr>",
                esc(&row.state),
                esc(team),
                chip(&row.state),
                esc(&row.session_id),
                esc(&row.session_id),
                esc(team),
                comma(row.budget_spent),
                esc(&head),
            ));
        }
        // Filters are client-side read aids: HTTP-1 has no filter params, and
        // inventing them would be a contract extension (ui-spec §9.2).
        // Option values are the wire spelling (TYPE-5 PascalCase, matching
        // data-state attrs); labels use the display vocabulary (uppercase).
        let states = [
            "Completed",
            "Running",
            "Suspended",
            "Halted",
            "Failed",
            "Init",
            "Cancelled",
        ];
        let state_opts: String = states
            .iter()
            .map(|s| format!("<option value=\"{s}\">{}</option>", s.to_uppercase()))
            .collect();
        let mut teams: Vec<&str> = rows.iter().filter_map(|r| r.team_id.as_deref()).collect();
        teams.sort_unstable();
        teams.dedup();
        let team_opts: String = teams
            .iter()
            .map(|t| format!("<option value=\"{}\">{}</option>", esc(t), esc(t)))
            .collect();
        let table = format!(
            "<table>\n<thead><tr>\
             <th scope=\"col\">상태</th><th scope=\"col\">세션</th><th scope=\"col\">팀</th>\
             <th scope=\"col\">예산 사용(tokens)</th><th scope=\"col\">체인 head</th>\
             </tr></thead>\n<tbody id=\"d1-rows\">{trs}</tbody></table>"
        );
        format!(
            "<form class=\"filters\" aria-label=\"세션 목록 필터\">\
             <label>상태 <select id=\"f-state\"><option value=\"\">전체 상태</option>{state_opts}</select></label> \
             <label>팀 <select id=\"f-team\"><option value=\"\">전체 팀</option>{team_opts}</select></label> \
             </form>\n{table}"
        )
    };

    Html(shell("세션", "/", &card("세션", &inner))).into_response()
}

// ---------------------------------------------------------------------------
// D2 — timeline (HTTP-2 initial + HTTP-3 live)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Gate,
    Handoff,
    Budget,
}

impl Filter {
    fn from_query(q: &HashMap<String, String>) -> Self {
        match q.get("filter").map(String::as_str) {
            Some("gate") => Self::Gate,
            Some("handoff") => Self::Handoff,
            Some("budget") => Self::Budget,
            _ => Self::All,
        }
    }

    fn matches(self, kind: &str) -> bool {
        match self {
            Self::All => true,
            Self::Gate => kind == "gate.pass" || kind == "gate.fail",
            Self::Handoff => kind == "handoff.request" || kind == "handoff.accept",
            Self::Budget => kind == "budget.event",
        }
    }

    /// CLI filter vocabulary (ui-spec §4.3): a filter shows only its families
    /// and disables folding (필터가 우선).
    fn disables_folding(self) -> bool {
        !matches!(self, Self::All)
    }
}

pub async fn d2(
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    State(core): State<Arc<dyn CorePort>>,
) -> Response {
    let filter = Filter::from_query(&q);

    let Some(snap) = core.session(&id) else {
        // 404 page — copy from ui-spec §9.3.
        let body = shell(
            "타임라인",
            "",
            &state_block(
                "error",
                "세션을 찾을 수 없습니다 — 목록으로 돌아가세요",
                Some(("세션 목록으로", "/")),
            ),
        );
        return (StatusCode::NOT_FOUND, Html(body)).into_response();
    };

    let events = match core.events(&id, None, crate::core_port::EVENTS_LIMIT_MAX) {
        Ok(ev) => ev,
        Err(_) => {
            return error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "타임라인",
                "스트림에 연결할 수 없습니다 — serve 로그를 확인하세요",
                &format!("/sessions/{}", esc(&id)),
            );
        }
    };

    // Seal rows annotate `bathos_audit=…` from the HTTP-5 pass-through (§4.5).
    let seal_status: Option<bool> = core
        .audit(&id)
        .ok()
        .flatten()
        .map(|a: AuditStatus| a.bathos_audit_verified);

    let base_ts = events.first().map(|e| e.ts).unwrap_or(0);
    let rows_html = render_rows(&events, base_ts, filter, seal_status);

    // Continuation link only when the first page is full (pagination stays
    // bounded, HTTP-2).
    let more = if events.len() == crate::core_port::EVENTS_LIMIT_MAX as usize {
        let last = events.last().map(|e| e.seq).unwrap_or(0);
        format!(
            "<p class=\"more\"><a href=\"#\" id=\"more-link\" data-after=\"{last}\">…이후 이벤트 500건 더 불러오기</a></p>"
        )
    } else {
        String::new()
    };

    // P-01 header — same fields as the CLI trace show header.
    let budget_txt = snap
        .budget
        .session_max_tokens
        .map(|n| format!("tokens={}", comma(n)))
        .unwrap_or_else(|| "—".into());
    let header = format!(
        "<p class=\"meta mono\">session {} · seed {} · plan {} · budget {}</p>\n{}",
        esc(&snap.session_id),
        snap.seed,
        esc(&hash8(&snap.plan_hash)),
        esc(&budget_txt),
        chip(&snap.state),
    );

    // Filter chips — GET links (server re-render), CLI flag vocabulary.
    let chip_link = |f: &str, label: &str, active: bool| {
        let cur = if active { " aria-pressed=\"true\"" } else { "" };
        let href = match f {
            "" => format!("/sessions/{}", esc(&id)),
            other => format!("/sessions/{}?filter={}", esc(&id), other),
        };
        format!(
            "<a class=\"chip-link{}\" href=\"{href}\"{cur}>{label}</a>",
            if active { " on" } else { "" }
        )
    };
    let chips = format!(
        "<div class=\"chip-row\" role=\"group\" aria-label=\"이벤트 필터\">{}{}{}{}</div>",
        chip_link("", "전체", filter == Filter::All),
        chip_link("gate", "gate", filter == Filter::Gate),
        chip_link("handoff", "handoff", filter == Filter::Handoff),
        chip_link("budget", "budget", filter == Filter::Budget),
    );

    let inner = format!(
        "{header}\n{chips}\n\
         <div class=\"live-row\"><span id=\"live-badge\" class=\"badge ok\" hidden>LIVE</span>\
         <span id=\"reconnect-badge\" class=\"badge warn\" hidden>재연결 중</span></div>\n\
         <div aria-live=\"polite\" id=\"events\">\n<table class=\"events mono\">\n<thead><tr>\
         <th scope=\"col\">seq</th><th scope=\"col\">T+</th><th scope=\"col\">event</th><th scope=\"col\">details</th>\
         </tr></thead>\n<tbody id=\"event-rows\" data-base-ts=\"{base_ts}\" data-filter=\"{fname}\">{rows_html}</tbody>\
         </table>\n</div>\n{more}",
        fname = match filter {
            Filter::All => "",
            Filter::Gate => "gate",
            Filter::Handoff => "handoff",
            Filter::Budget => "budget",
        },
    );

    Html(shell("타임라인", "", &card("타임라인", &inner))).into_response()
}

/// One event row (P-02 grid: seq · T+ · event · details). `tone` adds the
/// row-level symbol/colour for fail/warn kinds.
fn row_html(ev: &TraceEventWire, base_ts: u64, symbol: &str, tone: &str, details: &str) -> String {
    let commit = ev
        .commit_seq
        .map(|n| format!(" <span class=\"commit\">→ commit #{n}</span>"))
        .unwrap_or_default();
    format!(
        "<tr class=\"ev {tone}\" data-seq=\"{}\" data-kind=\"{}\">\
         <td class=\"num\">{}</td><td>{}</td><td>{}{}</td><td class=\"details\">{}{}</td></tr>",
        ev.seq,
        esc(&ev.kind),
        ev.seq,
        elapsed(ev.ts, base_ts),
        symbol,
        esc(&ev.kind),
        details,
        commit,
    )
}

/// P-03 fold row — collapsed external-call summary for one node span
/// (`⋮` carries the row; the T+ cell stays empty, per the P-03 block).
fn fold_row(llm: u64, tool: u64, ms: i64) -> String {
    let mut parts = Vec::new();
    if llm > 0 {
        parts.push(format!("llm.call {llm}"));
    }
    if tool > 0 {
        parts.push(format!("tool.call {tool}"));
    }
    if parts.is_empty() {
        return String::new();
    }
    let secs = ms as f64 / 1000.0;
    format!(
        "<tr class=\"ev fold\"><td class=\"num\">⋮</td><td></td><td></td>\
         <td class=\"details\">({} 생략 — {secs:.1}s)</td></tr>",
        parts.join(" · "),
    )
}

/// Render initial rows with the CLI's exact rules: default view folds
/// llm.call/tool.call per node span; a filter shows only its families with no
/// folding (W3-4, ui-spec §4.2).
fn render_rows(
    events: &[TraceEventWire],
    base_ts: u64,
    filter: Filter,
    seal_status: Option<bool>,
) -> String {
    let mut out = String::new();
    let (mut fold_llm, mut fold_tool, mut fold_ms) = (0u64, 0u64, 0i64);

    // Flush the pending external-call summary before the next structural row.
    let flush_fold = |out: &mut String, llm: &mut u64, tool: &mut u64, ms: &mut i64| {
        if *llm > 0 || *tool > 0 {
            out.push_str(&fold_row(*llm, *tool, *ms));
        }
        (*llm, *tool, *ms) = (0, 0, 0);
    };

    for ev in events {
        match ev.kind.as_str() {
            "llm.call" | "tool.call" => {
                if filter.disables_folding() {
                    continue; // filters never select external calls — no rows, no fold line
                }
                let ms = ev
                    .attrs
                    .get("latency_ms")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                fold_ms += ms;
                if ev.kind == "llm.call" {
                    fold_llm += 1;
                } else {
                    fold_tool += 1;
                }
                continue;
            }
            _ => {}
        }
        if !filter.matches(&ev.kind) {
            continue;
        }
        if !filter.disables_folding() {
            flush_fold(&mut out, &mut fold_llm, &mut fold_tool, &mut fold_ms);
        }
        let (symbol, tone, details) = details_for(ev, seal_status);
        out.push_str(&row_html(ev, base_ts, symbol, tone, &details));
    }
    if !filter.disables_folding() {
        flush_fold(&mut out, &mut fold_llm, &mut fold_tool, &mut fold_ms);
    }
    out
}

/// Display rename for attribute keys — P-02/§4.1 wording (node_count→nodes,
/// chain_head_hash→chain_head, reason_code→reason, contract_hash→contract).
fn attr_label(key: &str) -> &str {
    match key {
        "node_count" => "nodes",
        "chain_head_hash" => "chain_head",
        "reason_code" => "reason",
        "contract_hash" => "contract",
        other => other,
    }
}

/// `k=v` fragment with hash shorthand applied to hash-ish keys.
fn attr_kv(key: &str, val: &serde_json::Value) -> String {
    let owned;
    let s = match val.as_str() {
        Some(s) => s,
        None => {
            owned = val.to_string();
            owned.as_str()
        }
    };
    let v = if key.contains("hash") {
        hash8(s)
    } else {
        s.to_string()
    };
    format!("{}={}", attr_label(key), esc(&v))
}

/// Per-kind details line — mirrors ui-spec §4.1 examples exactly (P-02).
/// Returns (symbol, tone, details-html).
fn details_for(
    ev: &TraceEventWire,
    seal_status: Option<bool>,
) -> (&'static str, &'static str, String) {
    let a = |k: &str| ev.attrs.get(k);
    let s = |k: &str| a(k).and_then(|v| v.as_str()).unwrap_or("");

    match ev.kind.as_str() {
        "session.open" | "session.close" => {
            let parts: Vec<String> = ev
                .attrs
                .iter()
                .filter(|(k, _)| k.as_str() != "session_id")
                .map(|(k, v)| attr_kv(k, v))
                .collect();
            ("", "", parts.join(" "))
        }
        "plan.compiled" => (
            "",
            "",
            attr_kv(
                "plan_hash",
                a("plan_hash").unwrap_or(&serde_json::Value::Null),
            ) + " "
                + &attr_kv(
                    "node_count",
                    a("node_count").unwrap_or(&serde_json::Value::Null),
                ),
        ),
        "gate.pass" => {
            let mut parts = vec![
                esc(s("gate_id")),
                attr_kv("score", a("score").unwrap_or(&serde_json::Value::Null)),
            ];
            // Optional judge meta rides along verbatim (W3-5).
            for (k, v) in &ev.attrs {
                if k.starts_with("judge.") {
                    parts.push(attr_kv(k, v));
                }
            }
            ("", "", parts.join(" "))
        }
        "gate.fail" => {
            let retry = a("attempt")
                .and_then(|v| v.as_str())
                .map(|att| format!(" <span class=\"retry\">(retry {att}/2)</span>"))
                .unwrap_or_default();
            // Fixed 3-element array (clippy useless_vec): nothing grows it.
            let parts = [
                esc(s("gate_id")),
                attr_kv(
                    "reason_code",
                    a("reason_code").unwrap_or(&serde_json::Value::Null),
                ),
                attr_kv("score", a("score").unwrap_or(&serde_json::Value::Null)),
            ];
            ("✗ ", "err", parts.join(" ") + &retry)
        }
        "node.start" | "node.stop" => {
            let mut parts = vec![esc(s("node_id"))];
            if let Some(w) = a("wave") {
                parts.push(attr_kv("wave", w));
            }
            if let Some(sk) = a("stop_kind") {
                parts.push(attr_kv("stop_kind", sk));
            }
            ("", "", parts.join(" "))
        }
        "handoff.request" => {
            let to = if s("to").is_empty() {
                "—".to_string()
            } else {
                esc(s("to"))
            };
            (
                "",
                "",
                format!(
                    "{}→{} {}",
                    esc(s("from")),
                    to,
                    attr_kv(
                        "contract_hash",
                        a("contract_hash").unwrap_or(&serde_json::Value::Null)
                    )
                ),
            )
        }
        "handoff.accept" => ("", "", esc(s("to"))),
        "budget.event" => {
            let (symbol, tone) = match s("level") {
                "suspend" => ("✗ ", "err"),
                _ => ("! ", "warn"),
            };
            let parts: Vec<String> = ev.attrs.iter().map(|(k, v)| attr_kv(k, v)).collect();
            (symbol, tone, parts.join(" "))
        }
        "trace.seal" => {
            let mut line = attr_kv(
                "chain_head_hash",
                a("chain_head_hash").unwrap_or(&serde_json::Value::Null),
            );
            if let Some(verified) = seal_status {
                line.push_str(" bathos_audit=");
                line.push_str(if verified { "verified" } else { "FAILED" });
            }
            ("", "", line)
        }
        // Generic fallback: k=v in attr order (stable — BTreeMap).
        _ => {
            let parts: Vec<String> = ev.attrs.iter().map(|(k, v)| attr_kv(k, v)).collect();
            ("", "", parts.join(" "))
        }
    }
}

// ---------------------------------------------------------------------------
// D3 — budget, 3 units (HTTP-4 verbatim)
// ---------------------------------------------------------------------------

pub async fn d3(
    Query(q): Query<HashMap<String, String>>,
    State(core): State<Arc<dyn CorePort>>,
) -> Response {
    let team_sel = q
        .get("team_id")
        .map(String::as_str)
        .filter(|s| !s.is_empty());

    let report: BudgetReport = match core.budgets(team_sel) {
        Ok(r) => r,
        Err(_) => {
            return error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "예산",
                "예산 집계를 불러오지 못했습니다 — 코어에 연결할 수 없습니다.",
                "/budgets",
            );
        }
    };

    if report.session.is_empty() {
        let body = shell(
            "예산",
            "/budgets",
            &card(
                "예산",
                &state_block(
                    "empty",
                    "아직 세션이 없습니다 — hesmos run &lt;plan&gt;으로 첫 세션을 시작하세요",
                    None,
                ),
            ),
        );
        return Html(body).into_response();
    }

    // Team summary line — §6.3 wording: bar, totals, warn/suspend counts.
    let t = &report.team;
    let (spent, limit) = (t.spent, t.limit.unwrap_or(0));
    let team_bar = format!(
        "<p class=\"mono team-line\">팀 합계 {} {} / {} tokens ({}%)</p>\n\
         <p class=\"counts\">! 경고 {} · ✗ suspend {}</p>",
        bar22(spent, limit),
        comma(spent),
        comma(limit),
        t.pct.map(|p| p.to_string()).unwrap_or_else(|| "—".into()),
        t.warn_count,
        t.suspend_count,
    );

    // Session table — §6.3 columns: 세션 · 사용률 · 상태 (+ warn marker).
    let mut trs = String::new();
    for r in &report.session {
        let lim = r.limit.unwrap_or(0);
        let warn = if r.warn {
            format!(" <span class=\"warn-mark\">! {}% 경고</span>", r.warn_pct)
        } else {
            String::new()
        };
        trs.push_str(&format!(
            "<tr><td><a class=\"session-link\" href=\"/sessions/{}\">{}</a></td>\
             <td class=\"mono\">{} {} / {} tokens ({}%)</td><td>{}{}</td></tr>",
            esc(&r.session_id),
            esc(&r.session_id),
            bar22(r.spent, lim),
            comma(r.spent),
            comma(lim),
            r.pct.map(|p| p.to_string()).unwrap_or_else(|| "—".into()),
            chip(&r.state),
            warn,
        ));
    }
    let session_table = format!(
        "<table>\n<thead><tr><th scope=\"col\">세션</th><th scope=\"col\">사용률</th>\
         <th scope=\"col\">상태</th></tr></thead>\n<tbody>{trs}</tbody></table>"
    );

    // Agent distribution bars — §6.3 “에이전트 상위”.
    let agents: String = report
        .agent
        .iter()
        .map(|a| {
            let total = report.team.spent.max(1);
            format!(
                "<li><span class=\"role\">{}</span> <span class=\"mono\">{}</span> <span class=\"num\">{}%</span></li>",
                esc(&a.agent_role),
                bar22(a.spent, total),
                a.pct,
            )
        })
        .collect();
    let agent_block = format!("<h3>에이전트 상위</h3>\n<ul class=\"agents\">{agents}</ul>");

    // Team filter — GET form; options from the session list (data field only,
    // no RBAC UI — US-17 AC3).
    let teams: Vec<String> = core
        .list_sessions()
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.team_id.clone())
        .collect();
    let mut teams = teams;
    teams.sort();
    teams.dedup();
    let opts: String = std::iter::once("<option value=\"\">전체 팀</option>".to_string())
        .chain(teams.iter().map(|t| {
            let sel = if Some(t.as_str()) == team_sel {
                " selected"
            } else {
                ""
            };
            format!("<option value=\"{}\"{}>{}</option>", esc(t), sel, esc(t))
        }))
        .collect();
    let filter_form = format!(
        "<form class=\"filters\" method=\"get\" action=\"/budgets\" aria-label=\"팀 필터\">\
         <label>팀 <select name=\"team_id\" id=\"f-team\">{opts}</select></label> \
         <button type=\"submit\">적용</button></form>"
    );

    let inner = format!("{filter_form}\n{team_bar}\n{session_table}\n{agent_block}");
    Html(shell("예산", "/budgets", &card("예산", &inner))).into_response()
}

// ---------------------------------------------------------------------------
// D4 — audit, trace seal list (HTTP-5 pass-through)
// ---------------------------------------------------------------------------

pub async fn d4(State(core): State<Arc<dyn CorePort>>) -> Response {
    let sessions = match core.list_sessions() {
        Ok(s) => s,
        Err(_) => {
            return error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "감사",
                "감사 목록을 불러오지 못했습니다 — 코어에 연결할 수 없습니다.",
                "/audit",
            );
        }
    };

    let mut trs = String::new();
    let mut count = 0usize;
    // Newest first (same order as D1).
    for s in sessions.iter().rev() {
        let audit = match core.audit(&s.session_id) {
            Ok(Some(a)) => a,
            Ok(None) => continue, // not sealed — not part of the seal list
            Err(_) => {
                return error_page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "감사",
                    "감사 목록을 불러오지 못했습니다 — 코어에 연결할 수 없습니다.",
                    "/audit",
                );
            }
        };
        count += 1;
        // FAILED rows link into D2 for tamper investigation (C7→A6 handoff).
        let verify_chip = if audit.bathos_audit_verified {
            "<span class=\"chip ok\">✓ verified</span>".to_string()
        } else {
            format!(
                "<a class=\"chip err\" href=\"/sessions/{}\">✗ FAILED</a>",
                esc(&s.session_id)
            )
        };
        let actor = s.team_id.as_deref().unwrap_or("—");
        trs.push_str(&format!(
            "<tr><td><a class=\"session-link\" href=\"/sessions/{}\">{}</a></td>\
             <td class=\"mono\">{}</td><td>{verify_chip}</td><td>{}</td></tr>",
            esc(&s.session_id),
            esc(&s.session_id),
            esc(&hash8(&audit.chain_head_hash)),
            esc(actor),
        ));
    }

    if count == 0 {
        let body = shell(
            "감사",
            "/audit",
            &card(
                "감사",
                &state_block(
                    "empty",
                    "봉인된 세션이 없습니다 — 세션 종료 시 trace.seal이 기록됩니다",
                    None,
                ),
            ),
        );
        return Html(body).into_response();
    }

    let table = format!(
        "<table>\n<thead><tr><th scope=\"col\">세션</th><th scope=\"col\">chain_head</th>\
         <th scope=\"col\">bathos verify</th><th scope=\"col\">actor</th></tr></thead>\n<tbody>{trs}</tbody></table>"
    );
    Html(shell(
        "감사",
        "/audit",
        &card("감사 — trace seal 목록", &table),
    ))
    .into_response()
}

// ---------------------------------------------------------------------------
// Static assets (single CSS + minimal WS JS — ui-spec §9.0)
// ---------------------------------------------------------------------------

pub async fn css() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
        assets::CSS,
    )
}

pub async fn js() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        assets::JS,
    )
}
