//! SessionWal — the write-ahead commit store (SS-14, data-model-erd §3 AP-2 / §4).
//!
//! One SQLite database per session, `<root>/.hesmos/sessions/<id>/checkpoint.db`,
//! holding the sessions row and the commit points. The ledger table in the same file is
//! owned by `hesmos_budget::Ledger` (P1d); both open their own connection to the same
//! database and both rely on the runner's single-writer discipline — the runner is the
//! only component that writes, and it writes sequentially, so no cross-connection lock
//! contention exists by construction.
//!
//! The commit invariant (SS-14 rule 1): a checkpoint row exists ONLY for a post-gate
//! commit. There is no API here to write anything but commit points — the restore
//! candidate set and the commit set are the same set, which is what makes
//! `trace replay --at` sound (a restored state is always a committed state, AP-7:
//! "커밋 레코드만 진실").
//!
//! Transaction boundary (data-model-erd §4): one commit point = one SQLite transaction
//! (checkpoint row). The event append happens BEFORE this transaction in the runner
//! (backend.md decision #11): an append failure means this method is never reached (no
//! commit), and a transaction failure leaves extra events but no commit — safe in both
//! directions, because replay trusts only checkpoint rows.
//!
//! `fork_of` is stored as the pair `(session_id, commit_seq)` of the origin (TYPE-5
//! lineage; the suspend-resume path — SS-14 rule 4 — mints these, never a `resume`
//! command).

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use hesmos_core::{
    CommitSeq, Envelope, HandoffContract, RunId, SessionHandle, SessionId, SessionState, Sha256Hex,
    TeamId,
};

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("wal open failed at `{path}`: {source}")]
    Open {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("wal write failed: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("wal read returned malformed data: {0}")]
    Malformed(String),
}

/// One stored commit point — the restore unit of `trace replay --at`.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredCommit {
    pub seq: CommitSeq,
    pub node: hesmos_core::NodeId,
    pub envelope: Envelope,
    /// Present for every commit that followed a routed handoff (absent for a terminal
    /// node's own commit — nothing was routed INTO it).
    pub contract: Option<HandoffContract>,
}

/// The sessions row, as raw schema fields (string forms of the newtypes). Typed enough
/// to rebuild a reproduction basis; the fork CLI composes the new SessionHandle itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRow {
    pub session_id: String,
    pub run_id: String,
    pub seed: u64,
    pub plan_hash: String,
    /// The frozen envelope JSON (`BudgetEnvelope` schema).
    pub budget_json: String,
    pub state: String,
    pub team_id: Option<String>,
    /// `"<origin>#<commit_seq>"` when the session is a replay fork (TYPE-5).
    pub fork_of: Option<String>,
    pub chain_head: Option<String>,
}

pub struct SessionWal {
    conn: Connection,
}

impl SessionWal {
    /// Opens (creating if needed) the checkpoint store. WAL journal mode per ERD §4.
    /// The `ledger` table is deliberately NOT created here — it belongs to the budget
    /// crate's schema (P1d); opening the ledger in the same database creates it.
    pub fn open(db_path: &Path) -> Result<Self, WalError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| WalError::Malformed(format!("mkdir {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(db_path).map_err(|source| WalError::Open {
            path: db_path.display().to_string(),
            source,
        })?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 session_id  TEXT PRIMARY KEY,
                 run_id      TEXT NOT NULL,
                 seed        INTEGER NOT NULL,
                 plan_hash   TEXT NOT NULL,
                 budget_json TEXT NOT NULL,
                 state       TEXT NOT NULL,
                 team_id     TEXT,
                 fork_of     TEXT,
                 chain_head  TEXT
             );
             CREATE TABLE IF NOT EXISTS checkpoints (
                 session_id    TEXT NOT NULL,
                 commit_seq    INTEGER NOT NULL,
                 node_id       TEXT NOT NULL,
                 envelope_json TEXT NOT NULL,
                 contract_json TEXT,
                 ts            INTEGER NOT NULL,
                 PRIMARY KEY (session_id, commit_seq)
             );",
        )
        .map_err(WalError::Sql)?;
        Ok(Self { conn })
    }

    /// Inserts the sessions row at session start (INIT). Idempotent per session id —
    /// re-running the same id would be a caller bug, so a duplicate is an error
    /// (SQLite PK violation surfaces as WalError::Sql).
    pub fn insert_session(&self, handle: &SessionHandle) -> Result<(), WalError> {
        self.conn.execute(
            "INSERT INTO sessions (session_id, run_id, seed, plan_hash, budget_json, state, team_id, fork_of, chain_head)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                handle.session_id.to_string(),
                handle.run_id.to_string(),
                handle.seed as i64,
                handle.plan_hash.as_str(),
                serde_json::to_string(&handle.budget)
                    .map_err(|e| WalError::Malformed(e.to_string()))?,
                format!("{:?}", handle.state).to_uppercase(),
                handle.team_id.as_ref().map(|t| t.to_string()),
                handle
                    .fork_of
                    .as_ref()
                    .map(|(s, n)| format!("{s}#{n}", s = s, n = n.0)),
                handle.chain_head.as_ref().map(|h| h.as_str()),
            ],
        )?;
        Ok(())
    }

    /// Updates the sessions row's state and (optionally) chain head. Called on every
    /// terminal transition; the state spelling is the SCREAMING tokens.md §2.1 form.
    pub fn update_state(
        &self,
        session_id: &SessionId,
        state: SessionState,
        chain_head: Option<&Sha256Hex>,
    ) -> Result<(), WalError> {
        let changed = self.conn.execute(
            "UPDATE sessions SET state = ?2, chain_head = COALESCE(?3, chain_head)
             WHERE session_id = ?1",
            params![
                session_id.to_string(),
                format!("{state:?}").to_uppercase(),
                chain_head.map(|h| h.as_str()),
            ],
        )?;
        if changed == 0 {
            return Err(WalError::Malformed(format!(
                "no session row {}",
                session_id
            )));
        }
        Ok(())
    }

    /// Records one commit point (post-gate only — see the module invariant). `ts` is
    /// the caller's wall-clock aggregation aid; it never feeds any hash.
    pub fn commit_point(
        &self,
        session_id: &SessionId,
        seq: CommitSeq,
        node: &hesmos_core::NodeId,
        envelope: &Envelope,
        contract: Option<&HandoffContract>,
        ts: i64,
    ) -> Result<(), WalError> {
        let envelope_json =
            serde_json::to_string(envelope).map_err(|e| WalError::Malformed(e.to_string()))?;
        let contract_json = contract
            .map(|c| serde_json::to_string(c).map_err(|e| WalError::Malformed(e.to_string())))
            .transpose()?;
        self.conn.execute(
            "INSERT INTO checkpoints (session_id, commit_seq, node_id, envelope_json, contract_json, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id.to_string(),
                seq.0 as i64,
                node.as_str(),
                envelope_json,
                contract_json,
                ts,
            ],
        )?;
        Ok(())
    }

    /// The commit-point set, ascending — the `--at step N` validity oracle
    /// (USAGE-NOT-COMMIT_POINT listing comes from the same query).
    pub fn commit_seqs(&self, session_id: &SessionId) -> Result<Vec<CommitSeq>, WalError> {
        let mut stmt = self.conn.prepare(
            "SELECT commit_seq FROM checkpoints WHERE session_id = ?1 ORDER BY commit_seq",
        )?;
        let rows = stmt.query_map(params![session_id.to_string()], |r| r.get::<_, i64>(0))?;
        let mut seqs = Vec::new();
        for row in rows {
            seqs.push(CommitSeq(row? as u64));
        }
        Ok(seqs)
    }

    /// All stored commits with their payloads — the fork's response cache source
    /// ("응답 캐시 있으면 바이트 단위" — the committed envelope bytes are reused
    /// verbatim).
    pub fn commits(&self, session_id: &SessionId) -> Result<Vec<StoredCommit>, WalError> {
        let mut stmt = self.conn.prepare(
            "SELECT commit_seq, node_id, envelope_json, contract_json
             FROM checkpoints WHERE session_id = ?1 ORDER BY commit_seq",
        )?;
        let rows = stmt.query_map(params![session_id.to_string()], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut commits = Vec::new();
        for row in rows {
            let (seq, node, envelope_json, contract_json) = row?;
            let envelope: Envelope = serde_json::from_str(&envelope_json)
                .map_err(|e| WalError::Malformed(format!("envelope @#{seq}: {e}")))?;
            let contract = match contract_json {
                Some(json) => Some(
                    serde_json::from_str(&json)
                        .map_err(|e| WalError::Malformed(format!("contract @#{seq}: {e}")))?,
                ),
                None => None,
            };
            commits.push(StoredCommit {
                seq: CommitSeq(seq as u64),
                node: hesmos_core::NodeId::new(node),
                envelope,
                contract,
            });
        }
        Ok(commits)
    }

    /// The sessions row, if the session exists (CLI-2/3/4's 세션 없음 oracle).
    pub fn session_row(&self, session_id: &SessionId) -> Result<Option<SessionRow>, WalError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, run_id, seed, plan_hash, budget_json, state, team_id, fork_of, chain_head
             FROM sessions WHERE session_id = ?1",
        )?;
        let row = stmt
            .query_row(params![session_id.to_string()], |r| {
                Ok(SessionRow {
                    session_id: r.get(0)?,
                    run_id: r.get(1)?,
                    seed: r.get::<_, i64>(2)? as u64,
                    plan_hash: r.get(3)?,
                    budget_json: r.get(4)?,
                    state: r.get(5)?,
                    team_id: r.get(6)?,
                    fork_of: r.get(7)?,
                    chain_head: r.get(8)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Every session row in this database file — the `--team` aggregation source when
    /// callers scan all sessions under the root.
    pub fn all_sessions(&self) -> Result<Vec<SessionRow>, WalError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, run_id, seed, plan_hash, budget_json, state, team_id, fork_of, chain_head
             FROM sessions ORDER BY session_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionRow {
                session_id: r.get(0)?,
                run_id: r.get(1)?,
                seed: r.get::<_, i64>(2)? as u64,
                plan_hash: r.get(3)?,
                budget_json: r.get(4)?,
                state: r.get(5)?,
                team_id: r.get(6)?,
                fork_of: r.get(7)?,
                chain_head: r.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// SessionHandle → SessionRow (the fork path reads typed; write path wants the row).
/// Kept beside the schema so the two never drift.
pub fn session_row_of(handle: &SessionHandle) -> SessionRow {
    SessionRow {
        session_id: handle.session_id.to_string(),
        run_id: handle.run_id.to_string(),
        seed: handle.seed,
        plan_hash: handle.plan_hash.as_str().to_string(),
        budget_json: serde_json::to_string(&handle.budget).expect("envelope serializes"),
        state: format!("{:?}", handle.state).to_uppercase(),
        team_id: handle.team_id.as_ref().map(|t| t.to_string()),
        fork_of: handle
            .fork_of
            .as_ref()
            .map(|(s, n)| format!("{s}#{n}", s = s, n = n.0)),
        chain_head: handle.chain_head.as_ref().map(|h| h.as_str().to_string()),
    }
}

/// Parse helper for the stored fork lineage `"origin#N"`.
pub fn parse_fork_of(raw: &str) -> Option<(SessionId, CommitSeq)> {
    let (id, seq) = raw.split_once('#')?;
    Some((parse_session_id(id)?, CommitSeq(seq.parse().ok()?)))
}

/// ULID string → id newtype. The newtype's Ulid field is private to core, so parsing
/// goes through the u128 constructor (lossless roundtrip).
pub fn parse_session_id(text: &str) -> Option<SessionId> {
    let ulid = ulid::Ulid::from_string(text).ok()?;
    Some(SessionId::from_u128(ulid.into()))
}

/// run_id helper for rebuilding handles from rows.
pub fn parse_run_id(text: &str) -> Option<RunId> {
    let ulid = ulid::Ulid::from_string(text).ok()?;
    Some(RunId::from_u128(ulid.into()))
}

/// team id helper (string newtype).
pub fn parse_team_id(text: &str) -> TeamId {
    TeamId::new(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::{BudgetEnvelope, CorrelationId, EnvelopeId, EnvelopeKind, Payload, SchemaId};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "hesmos-wal-{name}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("mkdir");
            Self(dir.join("checkpoint.db"))
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().expect("parent"));
        }
    }

    fn handle(sid: u128) -> SessionHandle {
        SessionHandle {
            session_id: SessionId::from_u128(sid),
            run_id: RunId::from_u128(sid + 1),
            seed: 42,
            plan_hash: Sha256Hex::parse("a".repeat(64)).expect("hex"),
            budget: BudgetEnvelope {
                session_max_tokens: Some(1000),
                team_max_tokens: None,
                agent_max_tokens: BTreeMap::new(),
                warn_pct: 80,
                suspend_pct: 100,
            },
            state: SessionState::Init,
            team_id: None,
            fork_of: None,
            chain_head: None,
        }
    }

    fn envelope(n: u128) -> Envelope {
        Envelope {
            id: EnvelopeId::from_u128(n),
            from: hesmos_core::NodeId::new("a"),
            to: hesmos_core::NodeId::new("b"),
            kind: EnvelopeKind::Result,
            payload: Payload {
                schema_id: SchemaId::new("out.v1"),
                json: serde_json::json!({ "n": n }),
            },
            correlation_id: CorrelationId::from_u128(n),
            taint: hesmos_core::Taint::Clean,
        }
    }

    /// The commit set is the restore set: inserted points come back ascending and a
    /// roundtrip through the schema preserves the envelope bytes (the fork cache).
    #[test]
    fn commit_points_roundtrip_with_envelope_bytes() {
        let db = TempDb::new("roundtrip");
        let wal = SessionWal::open(&db.0).expect("open");
        let sid = SessionId::from_u128(0xC0);
        wal.insert_session(&handle(0xC0)).expect("insert");

        wal.commit_point(
            &sid,
            CommitSeq(1),
            &hesmos_core::NodeId::new("a"),
            &envelope(1),
            None,
            111,
        )
        .expect("commit 1");
        wal.commit_point(
            &sid,
            CommitSeq(2),
            &hesmos_core::NodeId::new("b"),
            &envelope(2),
            None,
            112,
        )
        .expect("commit 2");

        assert_eq!(
            wal.commit_seqs(&sid).expect("seqs"),
            vec![CommitSeq(1), CommitSeq(2)]
        );
        let commits = wal.commits(&sid).expect("commits");
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[1].envelope, envelope(2), "envelope bytes preserved");
        assert_eq!(commits[0].node.as_str(), "a");
    }

    /// Terminal transitions update the row; fork lineage roundtrips through its
    /// `"origin#N"` spelling.
    #[test]
    fn state_updates_and_fork_lineage_roundtrip() {
        let db = TempDb::new("lineage");
        let wal = SessionWal::open(&db.0).expect("open");
        let mut h = handle(0xC1);
        h.fork_of = Some((SessionId::from_u128(0xB0), CommitSeq(3)));
        h.state = SessionState::Suspended;
        wal.insert_session(&h).expect("insert");

        let row = wal.session_row(&h.session_id).expect("read").expect("row");
        assert_eq!(row.state, "SUSPENDED");
        let (origin, n) = parse_fork_of(row.fork_of.as_deref().expect("fork_of")).expect("pair");
        assert_eq!(origin, SessionId::from_u128(0xB0));
        assert_eq!(n, CommitSeq(3));

        wal.update_state(&h.session_id, SessionState::Completed, None)
            .expect("update");
        let row = wal.session_row(&h.session_id).expect("read").expect("row");
        assert_eq!(row.state, "COMPLETED");

        // Unknown session reads as None (the CLI-2/3 세션 없음 oracle)…
        assert!(
            wal.session_row(&SessionId::from_u128(0x999))
                .expect("read")
                .is_none()
        );
        // …and updating one is a hard error, not a silent no-op.
        assert!(
            wal.update_state(&SessionId::from_u128(0x999), SessionState::Failed, None)
                .is_err()
        );
    }

    /// The commit-point PK rejects duplicates — recording the same seq twice is a bug,
    /// and the store says so instead of silently overwriting evidence.
    #[test]
    fn duplicate_commit_point_is_rejected() {
        let db = TempDb::new("dup");
        let wal = SessionWal::open(&db.0).expect("open");
        let sid = SessionId::from_u128(0xC2);
        wal.insert_session(&handle(0xC2)).expect("insert");
        wal.commit_point(
            &sid,
            CommitSeq(1),
            &hesmos_core::NodeId::new("a"),
            &envelope(1),
            None,
            1,
        )
        .expect("first");
        assert!(
            wal.commit_point(
                &sid,
                CommitSeq(1),
                &hesmos_core::NodeId::new("a"),
                &envelope(1),
                None,
                2
            )
            .is_err()
        );
    }

    /// The `trace replay` fork path rebuilds a handle basis from a stored row: every
    /// parse helper roundtrips its own stored spelling, and session_row_of matches what
    /// the store writes for the same handle.
    #[test]
    fn stored_row_rebuilds_handle_basis() {
        let mut h = handle(0xC3);
        h.team_id = Some(TeamId::new("alpha"));
        let row = session_row_of(&h);

        let sid = parse_session_id(&row.session_id).expect("sid");
        let rid = parse_run_id(&row.run_id).expect("rid");
        let team = parse_team_id(row.team_id.as_deref().expect("team"));
        assert_eq!(sid, h.session_id);
        assert_eq!(rid, h.run_id);
        assert_eq!(team, TeamId::new("alpha"));
        assert_eq!(
            Sha256Hex::parse(&row.plan_hash).expect("hash"),
            h.plan_hash,
            "plan hash roundtrips verbatim (reproduction 4-element)"
        );
        // Malformed ULIDs are rejected, not silently accepted.
        assert!(parse_session_id("not-a-ulid").is_none());
        assert!(parse_fork_of("no-hash-mark").is_none());
    }
}
