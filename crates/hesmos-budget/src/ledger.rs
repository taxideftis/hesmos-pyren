//! The metering ledger (data-model-erd §3, AP-3/AP-4).
//!
//! One SQLite table per session inside `<session>/checkpoint.db`, denormalized on
//! purpose: `team_id` and `agent_role` are duplicated onto every row so team-level
//! aggregation is a single GROUP BY with no joins (AP-4). The truth stays in the
//! events log — the ledger is rebuildable from llm.call/tool.call events, which is
//! what audit re-checks (US-20 AC3: ledger sums == event sums).
//!
//! `ts` here is wall-clock and exists ONLY as an aggregation aid (D4 note): it never
//! feeds a chain hash, so it does not threaten S1 byte reproduction.

use rusqlite::{Connection, params};

use hesmos_core::SessionId;

/// Metered call kinds — the vocabulary mirrors the event families that carry tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterKind {
    Llm,
    Tool,
}

impl MeterKind {
    pub fn as_db(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Tool => "tool",
        }
    }
}

/// One metered call to record. `tokens_*` are counted AFTER the call completes; the
/// pre-call block (SS-15 rule 4) is the threshold engine's job, not the ledger's.
/// No `Eq` — `cost_usd` carries an `f64`.
#[derive(Debug, Clone, PartialEq)]
pub struct MeterEntry {
    pub team_id: Option<String>,
    pub agent_role: String,
    pub kind: MeterKind,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Token-centric metering: cost stays `None` until a unit table exists (out of
    /// scope per data-model-erd §7).
    pub cost_usd: Option<f64>,
}

/// A stored row — `row_seq` is 1-based and dense within the session (AP-3 PK).
#[derive(Debug, Clone, PartialEq)]
pub struct MeterRow {
    pub row_seq: u64,
    pub entry: MeterEntry,
    pub ts: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger open failed at `{path}`: {source}")]
    Open {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("ledger write failed: {0}")]
    Sql(#[source] rusqlite::Error),
}

/// Aggregate token usage — `total = tokens_in + tokens_out` on every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenTotals {
    pub tokens_in: u64,
    pub tokens_out: u64,
}

impl TokenTotals {
    pub fn total(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }
}

/// Per-session ledger over one SQLite connection (single writer — P6/P9 discipline).
pub struct Ledger {
    conn: Connection,
    session_id: String,
}

impl Ledger {
    /// Opens (creating if needed) the ledger at `db_path`. WAL journal mode per ERD §4;
    /// a single connection keeps write serialization local.
    pub fn open(session_id: &SessionId, db_path: &str) -> Result<Self, LedgerError> {
        let conn = Connection::open(db_path).map_err(|source| LedgerError::Open {
            path: db_path.to_string(),
            source,
        })?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS ledger (
                 session_id TEXT NOT NULL,
                 row_seq    INTEGER NOT NULL,
                 team_id    TEXT,
                 agent_role TEXT NOT NULL,
                 kind       TEXT NOT NULL CHECK (kind IN ('llm','tool')),
                 tokens_in  INTEGER NOT NULL,
                 tokens_out INTEGER NOT NULL,
                 cost_usd   REAL,
                 ts         INTEGER NOT NULL,
                 PRIMARY KEY (session_id, row_seq)
             );",
        )
        .map_err(LedgerError::Sql)?;
        Ok(Self {
            conn,
            session_id: session_id.to_string(),
        })
    }

    /// Appends one row. `ts` is supplied by the caller (wall clock at the call site) —
    /// this module never reads the clock itself, keeping it testable and deterministic.
    /// `row_seq` is derived inside the same transaction, so concurrent writers cannot
    /// create gaps or duplicates.
    pub fn record(&self, entry: MeterEntry, ts: i64) -> Result<u64, LedgerError> {
        self.conn
            .execute(
                "INSERT INTO ledger (session_id, row_seq, team_id, agent_role, kind, tokens_in, tokens_out, cost_usd, ts)
                 VALUES (?1, (SELECT COALESCE(MAX(row_seq), 0) + 1 FROM ledger WHERE session_id = ?1),
                         ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    self.session_id,
                    entry.team_id,
                    entry.agent_role,
                    entry.kind.as_db(),
                    entry.tokens_in as i64,
                    entry.tokens_out as i64,
                    entry.cost_usd,
                    ts,
                ],
            )
            .map_err(LedgerError::Sql)?;
        Ok(self
            .conn
            .query_row(
                "SELECT seq FROM (SELECT MAX(row_seq) AS seq FROM ledger WHERE session_id = ?1)",
                params![self.session_id],
                |r| r.get::<_, i64>(0),
            )
            .map_err(LedgerError::Sql)? as u64)
    }

    /// Session totals — the AP-3 single-session consumption figure.
    pub fn session_totals(&self) -> Result<TokenTotals, LedgerError> {
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0)
                 FROM ledger WHERE session_id = ?1",
                params![self.session_id],
                |r| {
                    Ok(TokenTotals {
                        tokens_in: r.get::<_, i64>(0)? as u64,
                        tokens_out: r.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .map_err(LedgerError::Sql)
    }

    /// Per-agent totals within the session (denormalized column → plain GROUP BY).
    pub fn totals_by_agent(&self) -> Result<Vec<(String, TokenTotals)>, LedgerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT agent_role, COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0)
                 FROM ledger WHERE session_id = ?1 GROUP BY agent_role ORDER BY agent_role",
            )
            .map_err(LedgerError::Sql)?;
        let rows = stmt
            .query_map(params![self.session_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    TokenTotals {
                        tokens_in: r.get::<_, i64>(1)? as u64,
                        tokens_out: r.get::<_, i64>(2)? as u64,
                    },
                ))
            })
            .map_err(LedgerError::Sql)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(LedgerError::Sql)
    }

    /// Per-team totals (AP-4). Within one session DB there is one team, but the GROUP
    /// BY is written team-scoped so a combined DB (many sessions, one file) aggregates
    /// cross-session in the same single pass.
    pub fn totals_by_team(&self) -> Result<Vec<(Option<String>, TokenTotals)>, LedgerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT team_id, COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0)
                 FROM ledger GROUP BY team_id ORDER BY team_id",
            )
            .map_err(LedgerError::Sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    TokenTotals {
                        tokens_in: r.get::<_, i64>(1)? as u64,
                        tokens_out: r.get::<_, i64>(2)? as u64,
                    },
                ))
            })
            .map_err(LedgerError::Sql)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(LedgerError::Sql)
    }

    /// Audit seam (US-20 AC3): the full row list, oldest first — replaying these rows
    /// against the event log must reproduce identical sums.
    pub fn all_rows(&self) -> Result<Vec<MeterRow>, LedgerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT row_seq, team_id, agent_role, kind, tokens_in, tokens_out, cost_usd, ts
                 FROM ledger WHERE session_id = ?1 ORDER BY row_seq",
            )
            .map_err(LedgerError::Sql)?;
        let rows = stmt
            .query_map(params![self.session_id], |r| {
                let kind_db: String = r.get(3)?;
                Ok(MeterRow {
                    row_seq: r.get::<_, i64>(0)? as u64,
                    entry: MeterEntry {
                        team_id: r.get(1)?,
                        agent_role: r.get(2)?,
                        kind: if kind_db == "llm" {
                            MeterKind::Llm
                        } else {
                            MeterKind::Tool
                        },
                        tokens_in: r.get::<_, i64>(4)? as u64,
                        tokens_out: r.get::<_, i64>(5)? as u64,
                        cost_usd: r.get(6)?,
                    },
                    ts: r.get(7)?,
                })
            })
            .map_err(LedgerError::Sql)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(LedgerError::Sql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(role: &str, kind: MeterKind, tokens: u64) -> MeterEntry {
        MeterEntry {
            team_id: Some("alpha".into()),
            agent_role: role.into(),
            kind,
            tokens_in: tokens / 2,
            tokens_out: tokens - tokens / 2,
            cost_usd: None,
        }
    }

    fn ledger() -> Ledger {
        Ledger::open(&SessionId::from_u128(7), ":memory:").expect("open")
    }

    /// row_seq is dense from 1 regardless of insertion order/timing.
    #[test]
    fn row_seqs_are_dense() {
        let l = ledger();
        assert_eq!(l.record(entry("a", MeterKind::Llm, 100), 1).expect("r1"), 1);
        assert_eq!(l.record(entry("b", MeterKind::Tool, 50), 2).expect("r2"), 2);
        assert_eq!(l.record(entry("a", MeterKind::Llm, 60), 3).expect("r3"), 3);
        let rows = l.all_rows().expect("rows");
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .enumerate()
                .all(|(i, r)| r.row_seq == i as u64 + 1)
        );
    }

    /// Session totals equal the token sum of every metered call.
    #[test]
    fn session_totals_match_recorded_calls() {
        let l = ledger();
        l.record(entry("a", MeterKind::Llm, 100), 1).expect("r");
        l.record(entry("a", MeterKind::Llm, 200), 2).expect("r");
        l.record(entry("b", MeterKind::Tool, 30), 3).expect("r");
        assert_eq!(l.session_totals().expect("totals").total(), 330);
    }

    /// AP-4: one GROUP BY over the denormalized columns aggregates all three scopes.
    #[test]
    fn aggregation_by_agent_and_team_is_single_pass() {
        let l = ledger();
        l.record(entry("a", MeterKind::Llm, 100), 1).expect("r");
        l.record(entry("b", MeterKind::Llm, 40), 2).expect("r");
        l.record(entry("a", MeterKind::Tool, 60), 3).expect("r");

        let by_agent = l.totals_by_agent().expect("by agent");
        assert_eq!(
            by_agent,
            vec![
                (
                    "a".to_string(),
                    TokenTotals {
                        tokens_in: 80,
                        tokens_out: 80
                    }
                ),
                (
                    "b".to_string(),
                    TokenTotals {
                        tokens_in: 20,
                        tokens_out: 20
                    }
                ),
            ]
        );

        let by_team = l.totals_by_team().expect("by team");
        assert_eq!(by_team.len(), 1);
        assert_eq!(by_team[0].0.as_deref(), Some("alpha"));
        assert_eq!(by_team[0].1.total(), 200);
    }

    /// Unknown kinds cannot exist: the CHECK constraint is the schema-level guard.
    #[test]
    fn kind_vocabulary_is_enforced_by_schema() {
        let l = ledger();
        let bad = l
            .conn
            .execute(
                "INSERT INTO ledger (session_id, row_seq, team_id, agent_role, kind, tokens_in, tokens_out, ts)
                 VALUES ('s', 1, NULL, 'a', 'judge', 1, 1, 0)",
                [],
            )
            .expect_err("judge kind is out of the envelope (W3-부2)");
        assert!(bad.to_string().contains("CHECK"));
    }
}
