//! TEMPORARY in-process core stand-in — the only stateful thing in this crate.
//!
//! It exists because `hesmos-core` (Phillip, WP-P0a/P1e) is not on the
//! workspace yet, while WP-P3c must prove HTTP-1~5 end-to-end now (lead
//! directive: build against the HTTP contract, consume a stub). Everything
//! here sits behind [`CorePort`]; routes/UI/serve never reference this module
//! except the `devserve` binary.
//!
//! ponytail: in-memory fake core (seed fixture) standing in for hesmos-core;
//! replace with a thin CorePort adapter over the core's public API
//! (SessionHandle / trace query / budget aggregate / PORT-2 audit_verify).
//! Trigger: the orchestrator exposes its read surface — session store listing
//! (hesmos-orchestrator WAL/runner, WP-P1e remainder), trace re-composition
//! query, budget-ledger aggregate and audit_verify pass-through. hesmos-core's
//! types landed (SessionHandle/TraceEvent/EventSink) but nothing persists or
//! queries yet, so a real adapter has nothing to consume — the stub stands
//! until then; `hesmos serve` boots against `StubCore::empty()` meanwhile.
//!
//! Notes for reviewers:
//! - Session state lives HERE (behind the port) exactly as it would live in
//!   the core — never in routes. The static boundary test enforces that
//!   routes/ui/stream contain no locks or caches.
//! - Hashes are fake (counter-hex): the gateway must never recompute the
//!   chain (HTTP-5 sourcing rule), so the stub doesn't need real hashing.

use std::collections::BTreeMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::core_port::{
    AgentShare, AuditStatus, BudgetReport, BudgetSessionRow, BudgetWire, CoreError, CorePort,
    SessionSnapshot, SessionSummary, TeamSummary, TraceEventWire,
};

const FANOUT_CAPACITY: usize = 256;

#[derive(Default)]
struct Inner {
    /// Insertion order preserved for D1's "newest first" (list is oldest-first).
    order: Vec<String>,
    sessions: BTreeMap<String, SessionSnapshot>,
    events: BTreeMap<String, BTreeMap<u64, TraceEventWire>>,
    next_seq: BTreeMap<String, u64>,
    prev_hash: BTreeMap<String, String>,
    fans: BTreeMap<String, broadcast::Sender<TraceEventWire>>,
    /// Precomputed 3-unit aggregates keyed by `Some(team)` / `None` (all).
    budgets: BTreeMap<Option<String>, BudgetReport>,
    audits: BTreeMap<String, Option<AuditStatus>>,
    /// Spent tokens per session — the HTTP-1 `budget_spent` source.
    spent: BTreeMap<String, u64>,
}

pub struct StubCore {
    inner: Mutex<Inner>,
}

impl StubCore {
    /// Empty store — no sessions (drives the D1/D3/D4 empty states in tests).
    pub fn empty() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Deterministic seed fixture covering D1–D4: four sessions (COMPLETED,
    /// SUSPENDED, RUNNING-with-warn, FAILED+tampered) across two teams, one
    /// sealed+verified, one sealed+FAILED, two unsealed.
    pub fn seeded() -> Self {
        let core = Self::empty();
        seed(&core);
        core
    }

    /// EventSink stand-in: append one event (auto seq, fake hash chain) and
    /// fan it out to current subscribers. Tests drive this to observe HTTP-3.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &self,
        session: &str,
        kind: &str,
        node: Option<&str>,
        attrs: &[(&str, &str)],
        commit_seq: Option<u64>,
        ts_ms: u64,
    ) -> u64 {
        let mut inner = self.inner.lock().expect("stub lock");
        let seq = inner.next_seq.entry(session.to_string()).or_insert(0);
        let seq = *seq;
        *inner.next_seq.get_mut(session).expect("just inserted") = seq + 1;

        let prev = inner
            .prev_hash
            .get(session)
            .cloned()
            .unwrap_or_else(|| "0".repeat(64));
        // Fake chain hash — display data only (see module notes).
        let hash = format!("{:0>64}", format!("{seq:x}"));
        inner.prev_hash.insert(session.to_string(), hash.clone());

        let ev = TraceEventWire {
            seq,
            kind: kind.to_string(),
            node: node.map(str::to_string),
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
                .collect(),
            prev_hash: prev,
            hash,
            ts: ts_ms,
            commit_seq,
        };
        if let Some(sender) = inner.fans.get(session) {
            // A send with no receivers errors — that's fine (subscription-point
            // semantics: pre-subscription events are not replayed).
            let _ = sender.send(ev.clone());
        }
        inner
            .events
            .entry(session.to_string())
            .or_default()
            .insert(seq, ev);
        seq
    }

    /// Register a session snapshot (fixture helper).
    fn add_session(&self, snap: SessionSnapshot) {
        let mut inner = self.inner.lock().expect("stub lock");
        let id = snap.session_id.clone();
        inner.order.push(id.clone());
        inner.sessions.insert(id, snap);
    }

    fn precompute_budget(&self, key: Option<&str>, report: BudgetReport) {
        let mut inner = self.inner.lock().expect("stub lock");
        inner.budgets.insert(key.map(str::to_string), report);
    }

    fn set_audit(&self, session: &str, audit: Option<AuditStatus>) {
        let mut inner = self.inner.lock().expect("stub lock");
        inner.audits.insert(session.to_string(), audit);
    }

    /// Record spent tokens for a session (fixture/test helper — HTTP-1 source).
    fn set_spent(&self, session: &str, spent: u64) {
        let mut inner = self.inner.lock().expect("stub lock");
        inner.spent.insert(session.to_string(), spent);
    }
}

impl CorePort for StubCore {
    fn list_sessions(&self) -> Result<Vec<SessionSummary>, CoreError> {
        let inner = self.inner.lock().expect("stub lock");
        Ok(inner
            .order
            .iter()
            .filter_map(|id| {
                inner.sessions.get(id).map(|s| {
                    let spent = inner.spent.get(id).copied().unwrap_or(0);
                    summary_of(s, spent)
                })
            })
            .collect())
    }

    fn session(&self, id: &str) -> Option<SessionSnapshot> {
        self.inner
            .lock()
            .expect("stub lock")
            .sessions
            .get(id)
            .cloned()
    }

    fn events(
        &self,
        id: &str,
        after: Option<u64>,
        limit: u16,
    ) -> Result<Vec<TraceEventWire>, CoreError> {
        let inner = self.inner.lock().expect("stub lock");
        let map = inner
            .events
            .get(id)
            .ok_or_else(|| CoreError::NotFound(id.to_string()))?;
        // Exclusive cursor: `after=Some(n)` → seq > n; `None` → from seq 0.
        let start = match after {
            None => 0,
            Some(n) => n.saturating_add(1),
        };
        Ok(map
            .range(start..)
            .take(limit as usize)
            .map(|(_, ev)| ev.clone())
            .collect())
    }

    fn budgets(&self, team_id: Option<&str>) -> Result<BudgetReport, CoreError> {
        let inner = self.inner.lock().expect("stub lock");
        // Unknown team → empty aggregate with zeroed summary (a filter result,
        // not an error — D3 renders its empty state).
        Ok(inner
            .budgets
            .get(&team_id.map(str::to_string))
            .cloned()
            .unwrap_or_else(|| empty_report(team_id)))
    }

    fn audit(&self, id: &str) -> Result<Option<AuditStatus>, CoreError> {
        let inner = self.inner.lock().expect("stub lock");
        if !inner.sessions.contains_key(id) {
            return Err(CoreError::NotFound(id.to_string()));
        }
        Ok(inner.audits.get(id).cloned().flatten())
    }

    fn subscribe(&self, id: &str) -> Result<broadcast::Receiver<TraceEventWire>, CoreError> {
        let mut inner = self.inner.lock().expect("stub lock");
        if !inner.sessions.contains_key(id) {
            return Err(CoreError::NotFound(id.to_string()));
        }
        let sender = inner
            .fans
            .entry(id.to_string())
            .or_insert_with(|| broadcast::channel(FANOUT_CAPACITY).0);
        Ok(sender.subscribe())
    }
}

fn summary_of(s: &SessionSnapshot, spent: u64) -> SessionSummary {
    SessionSummary {
        session_id: s.session_id.clone(),
        state: s.state.clone(),
        team_id: s.team_id.clone(),
        budget_spent: spent,
        chain_head: s.chain_head.clone(),
    }
}

fn empty_report(team_id: Option<&str>) -> BudgetReport {
    BudgetReport {
        session: vec![],
        team: TeamSummary {
            team_id: team_id.map(str::to_string),
            spent: 0,
            limit: None,
            pct: None,
            warn_count: 0,
            suspend_count: 0,
        },
        agent: vec![],
    }
}

// ---------------------------------------------------------------------------
// Seed fixture — deterministic constants only (no system time, no RNG).
// ---------------------------------------------------------------------------

const BASE_TS: u64 = 1_789_612_800_000; // 2026-09-17T00:00:00Z (epoch ms), fixed
const STEP_MS: u64 = 1_100;

fn hex64(prefix: &str) -> String {
    // 64-hex display constant from a short prefix (deterministic).
    let mut s = prefix.to_string();
    while s.len() < 64 {
        s.push_str("0123456789abcdef");
    }
    s.truncate(64);
    s
}

fn snap(
    id: &str,
    state: &str,
    team: Option<&str>,
    seed: u64,
    plan_hash: &str,
    max_tokens: u64,
    chain_head: Option<&str>,
) -> SessionSnapshot {
    SessionSnapshot {
        session_id: id.to_string(),
        run_id: format!("{id}-r1"),
        seed,
        plan_hash: hex64(plan_hash),
        budget: BudgetWire {
            session_max_tokens: Some(max_tokens),
            team_max_tokens: None,
            agent_max_tokens: BTreeMap::new(),
            warn_pct: 80,
            suspend_pct: 100,
        },
        state: state.to_string(),
        team_id: team.map(str::to_string),
        fork_of: None,
        chain_head: chain_head.map(hex64),
    }
}

/// (id, spent, limit, state, warn) row builder for the budget fixtures.
struct BRow(&'static str, u64, u64, &'static str, bool);

fn pct(spent: u64, limit: u64) -> u8 {
    // Integer rounding, matching tokens §1.4 (정수 1개) — display math only.
    if limit == 0 {
        return 100;
    }
    let v = ((spent * 100 + limit / 2) / limit).min(100);
    v as u8
}

fn build_report(
    team: Option<&str>,
    rows: &[BRow],
    team_limit: u64,
    warn_count: u32,
    suspend_count: u32,
    agents: &[(&str, u64)],
) -> BudgetReport {
    let spent_total: u64 = rows.iter().map(|r| r.1).sum();
    BudgetReport {
        session: rows
            .iter()
            .map(|r| BudgetSessionRow {
                session_id: r.0.to_string(),
                spent: r.1,
                limit: Some(r.2),
                pct: Some(pct(r.1, r.2)),
                state: r.3.to_string(),
                warn: r.4,
                warn_pct: 80,
            })
            .collect(),
        team: TeamSummary {
            team_id: team.map(str::to_string),
            spent: spent_total,
            limit: Some(team_limit),
            pct: Some(pct(spent_total, team_limit)),
            warn_count,
            suspend_count,
        },
        agent: {
            let total: u64 = agents.iter().map(|a| a.1).sum();
            agents
                .iter()
                .map(|(role, spent)| AgentShare {
                    agent_role: role.to_string(),
                    spent: *spent,
                    pct: pct(*spent, total),
                })
                .collect()
        },
    }
}

fn seed(core: &StubCore) {
    // --- sessions -----------------------------------------------------------
    let s1 = "01JATS8W6Z9F3A2C5HQ1VMDKR7"; // COMPLETED, platform, sealed+verified
    let s2 = "01JATS9K2M5C81AA7XPT4BNQWE"; // SUSPENDED (BUDGET_EXCEEDED), platform
    let s3 = "01JATSB21E0D04C9A3RGY7VKS5"; // RUNNING 99% warn, platform
    let s4 = "01JATSC4D77E22H8N6JQZW3MT9"; // FAILED (GATE_REJECT), dev, sealed+FAILED
    let head1 = hex64("77b0d2c9a1");
    let head4 = hex64("0e55bb220e");

    core.add_session(snap(
        s1,
        "Completed",
        Some("platform"),
        42,
        "8c1d44e8",
        250_000,
        Some(&head1),
    ));
    core.add_session(snap(
        s2,
        "Suspended",
        Some("platform"),
        7,
        "5b2aa091",
        250_000,
        None,
    ));
    core.add_session(snap(
        s3,
        "Running",
        Some("platform"),
        13,
        "d04c9a33",
        200_000,
        None,
    ));
    core.add_session(snap(
        s4,
        "Failed",
        Some("dev"),
        99,
        "1f0e55b2",
        250_000,
        Some(&head4),
    ));

    // HTTP-1 budget_spent values (the "core ledger" already metered these).
    core.set_spent(s1, 172_420);
    core.set_spent(s2, 235_000);
    core.set_spent(s3, 198_050);
    core.set_spent(s4, 88_120);

    // --- audit results (pass-through values the "core" already determined) ---
    core.set_audit(
        s1,
        Some(AuditStatus {
            chain_head_hash: head1.clone(),
            bathos_audit_verified: true,
        }),
    );
    core.set_audit(
        s4,
        Some(AuditStatus {
            chain_head_hash: head4.clone(),
            bathos_audit_verified: false,
        }),
    );
    core.set_audit(s2, None);
    core.set_audit(s3, None);

    // --- budget aggregates (the "core ledger" already summed these) ----------
    core.precompute_budget(
        Some("platform"),
        build_report(
            Some("platform"),
            &[
                BRow(s1, 172_420, 250_000, "Completed", false),
                BRow(s2, 235_000, 250_000, "Suspended", true),
                BRow(s3, 198_050, 200_000, "Running", true),
            ],
            700_000,
            2,
            1,
            &[
                ("research", 230_079),
                ("draft", 163_627),
                ("verify", 127_149),
                ("misc", 84_615),
            ],
        ),
    );
    core.precompute_budget(
        Some("dev"),
        build_report(
            Some("dev"),
            &[BRow(s4, 88_120, 250_000, "Failed", false)],
            300_000,
            0,
            0,
            &[("draft", 62_000), ("verify", 26_120)],
        ),
    );
    core.precompute_budget(
        None,
        build_report(
            None,
            &[
                BRow(s1, 172_420, 250_000, "Completed", false),
                BRow(s2, 235_000, 250_000, "Suspended", true),
                BRow(s3, 198_050, 200_000, "Running", true),
                BRow(s4, 88_120, 250_000, "Failed", false),
            ],
            1_000_000,
            2,
            1,
            &[
                ("research", 230_079),
                ("draft", 225_627),
                ("verify", 153_269),
                ("misc", 84_615),
            ],
        ),
    );

    // --- s1 events: mirrors ui-spec §4.1 (fold, commit markers, seal) --------
    let mut t = BASE_TS;
    let mut ts = || {
        t += STEP_MS;
        t
    };
    core.push(
        s1,
        "session.open",
        None,
        &[
            ("session_id", s1),
            ("seed", "42"),
            ("budget", "tokens=250000"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "plan.compiled",
        None,
        &[("plan_hash", &hex64("8c1d44e8")), ("node_count", "4")],
        None,
        ts(),
    );
    core.push(
        s1,
        "gate.pass",
        None,
        &[("gate_id", "pre g0-size"), ("score", "1.00")],
        None,
        ts(),
    );
    core.push(
        s1,
        "node.start",
        Some("research"),
        &[
            ("node_id", "research"),
            ("agent_role", "research"),
            ("wave", "1"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "llm.call",
        Some("research"),
        &[
            ("provider", "glm"),
            ("model", "glm-5.3-flash"),
            ("tokens_in", "4120"),
            ("tokens_out", "980"),
            ("latency_ms", "4100"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "llm.call",
        Some("research"),
        &[
            ("provider", "glm"),
            ("model", "glm-5.3-flash"),
            ("tokens_in", "3801"),
            ("tokens_out", "1024"),
            ("latency_ms", "3900"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "llm.call",
        Some("research"),
        &[
            ("provider", "glm"),
            ("model", "glm-5.3-flash"),
            ("tokens_in", "3950"),
            ("tokens_out", "870"),
            ("latency_ms", "3500"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "tool.call",
        Some("research"),
        &[
            ("tool_name", "web.search"),
            ("ok", "true"),
            ("latency_ms", "600"),
            ("actor", "research"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "gate.pass",
        None,
        &[("gate_id", "post research"), ("score", "0.92")],
        Some(1),
        ts(),
    );
    let contract1 = hex64("ab12ef34");
    core.push(
        s1,
        "handoff.request",
        None,
        &[
            ("contract_hash", &contract1),
            ("from", "research"),
            ("to", "draft"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "handoff.accept",
        None,
        &[
            ("contract_hash", &contract1),
            ("from", "research"),
            ("to", "draft"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "node.start",
        Some("draft"),
        &[("node_id", "draft"), ("agent_role", "draft"), ("wave", "2")],
        None,
        ts(),
    );
    core.push(
        s1,
        "gate.fail",
        None,
        &[
            ("gate_id", "post draft"),
            ("reason_code", "SCHEMA_VIOLATION"),
            ("score", "0.41"),
            ("attempt", "1"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "gate.pass",
        None,
        &[("gate_id", "post draft"), ("score", "0.88")],
        Some(2),
        ts(),
    );
    core.push(
        s1,
        "node.stop",
        Some("verify"),
        &[
            ("node_id", "verify"),
            ("agent_role", "verify"),
            ("wave", "3"),
            ("stop_kind", "done"),
        ],
        None,
        ts(),
    );
    core.push(
        s1,
        "gate.pass",
        None,
        &[("gate_id", "post verify"), ("score", "0.95")],
        Some(3),
        ts(),
    );
    core.push(
        s1,
        "trace.seal",
        None,
        &[("chain_head_hash", &head1)],
        None,
        ts(),
    );

    // --- s2: suspend path ----------------------------------------------------
    core.push(
        s2,
        "session.open",
        None,
        &[
            ("session_id", s2),
            ("seed", "7"),
            ("budget", "tokens=250000"),
        ],
        None,
        ts(),
    );
    core.push(
        s2,
        "plan.compiled",
        None,
        &[("plan_hash", &hex64("5b2aa091")), ("node_count", "3")],
        None,
        ts(),
    );
    core.push(
        s2,
        "node.start",
        Some("research"),
        &[
            ("node_id", "research"),
            ("agent_role", "research"),
            ("wave", "1"),
        ],
        None,
        ts(),
    );
    core.push(
        s2,
        "budget.event",
        None,
        &[
            ("level", "warn"),
            ("spent", "200000"),
            ("remaining", "50000"),
        ],
        None,
        ts(),
    );
    core.push(
        s2,
        "budget.event",
        None,
        &[
            ("level", "suspend"),
            ("spent", "235000"),
            ("remaining", "15000"),
        ],
        None,
        ts(),
    );
    core.push(
        s2,
        "session.close",
        None,
        &[
            ("session_id", s2),
            ("final_state", "Suspended"),
            ("reason_code", "BUDGET_EXCEEDED"),
        ],
        None,
        ts(),
    );

    // --- s3: still running, warn observed ------------------------------------
    core.push(
        s3,
        "session.open",
        None,
        &[
            ("session_id", s3),
            ("seed", "13"),
            ("budget", "tokens=200000"),
        ],
        None,
        ts(),
    );
    core.push(
        s3,
        "plan.compiled",
        None,
        &[("plan_hash", &hex64("d04c9a33")), ("node_count", "4")],
        None,
        ts(),
    );
    core.push(
        s3,
        "node.start",
        Some("draft"),
        &[("node_id", "draft"), ("agent_role", "draft"), ("wave", "1")],
        None,
        ts(),
    );
    core.push(
        s3,
        "budget.event",
        None,
        &[
            ("level", "warn"),
            ("spent", "198050"),
            ("remaining", "1950"),
        ],
        None,
        ts(),
    );

    // --- s4: failed + tampered audit -----------------------------------------
    core.push(
        s4,
        "session.open",
        None,
        &[
            ("session_id", s4),
            ("seed", "99"),
            ("budget", "tokens=250000"),
        ],
        None,
        ts(),
    );
    core.push(
        s4,
        "plan.compiled",
        None,
        &[("plan_hash", &hex64("1f0e55b2")), ("node_count", "3")],
        None,
        ts(),
    );
    core.push(
        s4,
        "node.start",
        Some("draft"),
        &[("node_id", "draft"), ("agent_role", "draft"), ("wave", "1")],
        None,
        ts(),
    );
    core.push(
        s4,
        "gate.fail",
        None,
        &[
            ("gate_id", "post draft"),
            ("reason_code", "GATE_REJECT"),
            ("score", "0.31"),
            ("attempt", "2"),
        ],
        None,
        ts(),
    );
    core.push(
        s4,
        "session.close",
        None,
        &[
            ("session_id", s4),
            ("final_state", "Failed"),
            ("reason_code", "GATE_REJECT"),
        ],
        None,
        ts(),
    );
    core.push(
        s4,
        "trace.seal",
        None,
        &[("chain_head_hash", &head4)],
        None,
        ts(),
    );
}
