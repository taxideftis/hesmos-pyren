//! EventLog — the default PORT-1 sink: one append-only jsonl hash chain per session.
//!
//! Storage is `.hesmos/sessions/<session_id>/events.jsonl` (data-model-erd §4), one
//! canonical JSON event per line. ACs realized here (SS-02):
//! 1. append-only — this type has no modify/delete API; the log is the single source of
//!    truth and derived views must rebuild from it, never store alongside.
//! 2. chain-linked — every event embeds `prev_hash`; `verify` recomputes the whole chain
//!    and reports the FIRST fault (tamper detection), not a boolean.
//! 3. taxonomy-enforced — foreign kinds / missing required attrs cannot enter: the type
//!    system refuses them at `PendingEvent::new`, and the file parser refuses them at
//!    load (a hand-written foreign line is a `TraceError::InvalidLine`).
//!
//! Hash scope (ADR-0006): `hash = sha256(canonical(prev_hash, seq, kind, node, attrs))`.
//! `ts` is stored for human inspection but is NOT a hash input — wall-clock data would
//! make S1 byte reproduction impossible. Consequence, intended and audit-accepted:
//! modifying only `ts` goes undetected by the chain; the chain attests the ordered
//! content fields, nothing else. The doc comment on [`TraceError::HashMismatch`] and the
//! T2 test #6 pin this behavior as deliberate.
//!
//! Single writer per session (PORT-1 invariant): one `EventLog` per session_id; the
//! interior `Mutex` serializes appends within the process, and cross-process order is
//! the runner's commit serialization (WP-P1c/P1e), never a log-level concern.
//!
//! ponytail: `open` re-scans and fully verifies the existing file to rebuild the chain
//! head — O(log size) per open. Upgrade trigger: sessions whose logs exceed ~1e5 events
//! (then: tail-only scan + lazy full verify on first `verify` call).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use hesmos_core::{
    EventSink, PendingEvent, SessionId, Sha256Hex, TraceEvent, canonical_bytes, chain_hash,
};

/// `prev_hash` of event #0. All-zeros is the conventional genesis marker; it is a valid
/// `Sha256Hex`, so no special-casing is needed downstream.
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Trace-layer failures. The `Chain*` variants are faults found while scanning a stored
/// log; each carries the position and both values so the CLI can point at the exact
/// event instead of saying "corrupt".
#[derive(Debug, Error)]
pub enum TraceError {
    #[error("trace i/o: {0}")]
    Io(#[from] std::io::Error),
    /// First event whose stored hash disagrees with recomputation. Reported at the
    /// earliest offender only — everything after a broken link is untrusted anyway.
    #[error("chain tamper: event #{seq} stored hash {stored} != recomputed {computed}")]
    HashMismatch {
        seq: u64,
        stored: Sha256Hex,
        computed: Sha256Hex,
    },
    /// Seq is 0-based and gap-free; any skip is a splice (deleted/inserted event).
    #[error("chain seq gap: expected #{expected}, found #{found}")]
    SeqGap { expected: u64, found: u64 },
    /// `prev_hash` does not match the previous event's hash (or genesis at #0).
    #[error("chain broken link: event #{seq} prev_hash {stored_prev} != expected {expected_prev}")]
    BrokenLink {
        seq: u64,
        stored_prev: Sha256Hex,
        expected_prev: Sha256Hex,
    },
    /// A line that is not a valid TYPE-3 event: foreign (taxonomy-external) kind,
    /// schema violation, or malformed JSON. SS-02 rule 3: refusal is correct behavior.
    #[error("line {line}: not a valid TYPE-3 event (foreign kind or schema violation)")]
    InvalidLine { line: usize },
}

/// Mutable chain state guarded by the log's mutex.
struct LogState {
    file: File,
    /// Next seq to assign — 0-based, monotonic, gap-free by construction.
    next_seq: u64,
    /// Hash of the last appended event (genesis at open on an empty log).
    prev_hash: Sha256Hex,
    path: PathBuf,
}

/// The default EventSink (PORT-1). Clone-free shared use behind `&self`.
pub struct EventLog {
    state: Mutex<LogState>,
}

impl EventLog {
    /// Opens (or resumes) the session's event log under `<root>/.hesmos/sessions/<id>/`.
    ///
    /// An existing log is fully scanned and verified first: appending onto a tampered
    /// chain would contaminate all future evidence, so any fault refuses the open. A
    /// missing log starts a fresh chain from [`GENESIS_PREV_HASH`].
    pub fn open(root: &Path, session_id: &SessionId) -> Result<Self, TraceError> {
        let dir = root
            .join(".hesmos")
            .join("sessions")
            .join(session_id.to_string());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("events.jsonl");

        let (next_seq, prev_hash) = if path.exists() {
            let (count, head) = scan(&path)?;
            (count, head)
        } else {
            (
                0,
                Sha256Hex::parse(GENESIS_PREV_HASH).expect("genesis is valid hex"),
            )
        };

        let file = OpenOptions::new().append(true).create(true).open(&path)?;
        Ok(Self {
            state: Mutex::new(LogState {
                file,
                next_seq,
                prev_hash,
                path,
            }),
        })
    }

    /// The fallible append path. The WAL/runner (WP-P1e) uses this and treats `Err` as
    /// "no commit" — an uncommitted event is safe (AP-7: only commit records are truth).
    ///
    /// Order of operations is fixed: seq assign → chain hash (no `ts`) → canonical line
    /// append → state advance. A failed write leaves the in-memory head stale on
    /// purpose; the next `open` re-scans from disk, which is authoritative.
    pub fn try_append(&self, e: PendingEvent) -> Result<TraceEvent, TraceError> {
        let mut st = self.state.lock().expect("event log mutex poisoned");
        let seq = st.next_seq;
        let hash = chain_hash(&st.prev_hash, seq, e.kind, e.node.as_ref(), &e.attrs);
        let event = TraceEvent {
            seq,
            kind: e.kind,
            node: e.node,
            attrs: e.attrs,
            prev_hash: st.prev_hash.clone(),
            hash: hash.clone(),
            ts: now_ms(),
        };
        let mut line = canonical_bytes(&event);
        line.push(b'\n');
        st.file.write_all(&line)?;
        st.file.flush()?;
        st.next_seq = seq + 1;
        st.prev_hash = hash;
        Ok(event)
    }

    /// Full recomputation of the stored chain (AC2). Returns the number of events
    /// verified, or the FIRST fault with its position and both hash values.
    ///
    /// Reads from disk on every call — the file, not memory, is the evidence.
    pub fn verify(&self) -> Result<u64, TraceError> {
        let path = self
            .state
            .lock()
            .expect("event log mutex poisoned")
            .path
            .clone();
        let (count, _) = scan(&path)?;
        Ok(count)
    }

    /// The display/inspection read path (CLI-2): parses every stored event AND checks
    /// the chain, but unlike `open`/`verify` it does not throw the readable prefix away
    /// when a fault is found. A tampered trace must still render its summary for
    /// investigation (ui-spec §4.4 — "요약 렌더 유지 + 상단 배지 + exit 30"), so the
    /// fault travels WITH the parsed events instead of replacing them.
    ///
    /// The `path` variant is the standalone form for consumers that hold no `EventLog`
    /// (read-only inspection of another session's log).
    pub fn load(&self) -> LoadOutcome {
        let path = self
            .state
            .lock()
            .expect("event log mutex poisoned")
            .path
            .clone();
        load_path(&path)
    }

    /// Test/inspection accessor: the log's file path.
    #[cfg(test)]
    fn path(&self) -> PathBuf {
        self.state
            .lock()
            .expect("event log mutex poisoned")
            .path
            .clone()
    }
}

impl EventSink for EventLog {
    /// Infallible by contract (PORT-1). An append failure means the evidence chain can
    /// no longer be maintained, which is fatal to the session — so this crashes rather
    /// than silently dropping an event (a swallowed error is the forbidden failure
    /// mode). Graceful degradation is `try_append`, used by the WAL transaction.
    fn emit(&self, e: PendingEvent) -> TraceEvent {
        self.try_append(e)
            .expect("trace append failure is fatal to the evidence chain")
    }
}

/// What [`EventLog::load`] / [`load_path`] found.
///
/// On a chain fault the parse STOPS at the faulted event: everything after a break
/// could be attacker-forged, so rendering it adds no trustworthy information — the
/// prefix up to and including the fault is exactly the investigation material
/// (ui-spec §4.4: 요약 렌더 유지 + 상단 배지).
#[derive(Debug)]
pub enum LoadOutcome {
    /// Chain intact — the events are verified evidence.
    Ok(Vec<TraceEvent>),
    /// The parsed prefix INCLUDING the faulted event, plus the first fault.
    Tampered {
        events: Vec<TraceEvent>,
        fault: TraceError,
    },
    /// The log could not be read at all (I/O) — no events to show.
    Io(std::io::Error),
}

/// Read-only inspection of a log file (CLI-2/CLI-4): parse + verify in one pass,
/// keeping the parsed prefix on failure. This is the standalone form for consumers
/// that hold no `EventLog` — crucially, it never goes through [`EventLog::open`],
/// whose eager scan exists to protect APPENDS and therefore refuses a tampered file
/// outright (an inspector must still render the readable prefix with its badge).
pub fn load_path(path: &Path) -> LoadOutcome {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return LoadOutcome::Io(e),
    };
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut prev = match Sha256Hex::parse(GENESIS_PREV_HASH) {
        Ok(h) => h,
        Err(_) => unreachable!("genesis is valid hex"),
    };

    for (i, line) in reader.lines().enumerate() {
        // The next expected seq IS the count of accepted events so far — one counter,
        // no drift between the two.
        let expected_seq = events.len() as u64;
        let line = match line {
            Ok(l) => l,
            Err(e) => return LoadOutcome::Io(e),
        };
        let event: TraceEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => {
                return LoadOutcome::Tampered {
                    events,
                    fault: TraceError::InvalidLine { line: i + 1 },
                };
            }
        };
        // Chain checks mirror `scan` exactly; on the first fault we stop trusting and
        // return what parsed. The event that failed the check is still included (it is
        // the one the badge points at).
        let fault = if event.seq != expected_seq {
            Some(TraceError::SeqGap {
                expected: expected_seq,
                found: event.seq,
            })
        } else if event.prev_hash != prev {
            Some(TraceError::BrokenLink {
                seq: event.seq,
                stored_prev: event.prev_hash.clone(),
                expected_prev: prev.clone(),
            })
        } else {
            let computed = chain_hash(
                &event.prev_hash,
                event.seq,
                event.kind,
                event.node.as_ref(),
                &event.attrs,
            );
            (computed != event.hash).then(|| TraceError::HashMismatch {
                seq: event.seq,
                stored: event.hash.clone(),
                computed,
            })
        };
        events.push(event);
        if let Some(fault) = fault {
            return LoadOutcome::Tampered { events, fault };
        }
        prev = events.last().expect("just pushed").hash.clone();
    }
    LoadOutcome::Ok(events)
}

/// Recomputes the whole chain from the file. Returns (event count, head hash) or the
/// first fault. Shared by `open` (strict resume) and `verify` (post-hoc audit).
fn scan(path: &Path) -> Result<(u64, Sha256Hex), TraceError> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut expected_seq = 0u64;
    let mut prev = Sha256Hex::parse(GENESIS_PREV_HASH).expect("genesis is valid hex");

    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        let event: TraceEvent =
            serde_json::from_str(&line).map_err(|_| TraceError::InvalidLine { line: i + 1 })?;
        if event.seq != expected_seq {
            return Err(TraceError::SeqGap {
                expected: expected_seq,
                found: event.seq,
            });
        }
        if event.prev_hash != prev {
            return Err(TraceError::BrokenLink {
                seq: event.seq,
                stored_prev: event.prev_hash.clone(),
                expected_prev: prev.clone(),
            });
        }
        let computed = chain_hash(
            &event.prev_hash,
            event.seq,
            event.kind,
            event.node.as_ref(),
            &event.attrs,
        );
        if computed != event.hash {
            return Err(TraceError::HashMismatch {
                seq: event.seq,
                stored: event.hash.clone(),
                computed,
            });
        }
        prev = event.hash;
        expected_seq += 1;
    }
    Ok((expected_seq, prev))
}

/// Wall-clock ms for the stored-only `ts` field. Deliberately excluded from the chain
/// hash (see module doc): reproduction must not depend on when an event was written.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::{EventAttrs, EventKind, NodeId};

    /// Temporary per-test root: `<tmp>/.hesmos/sessions/<id>/events.jsonl`.
    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("hesmos-trace-t2-{name}-{}", ulid_nonce()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
        fn log(&self, sid: u128) -> EventLog {
            EventLog::open(&self.0, &SessionId::from_u128(sid)).expect("open fresh log")
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Non-cryptographic per-test nonce so parallel test runs never share a directory.
    fn ulid_nonce() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    }

    fn session_open(sid: u128) -> PendingEvent {
        PendingEvent::new(
            EventKind::SessionOpen,
            None,
            EventAttrs::new()
                .set("session_id", SessionId::from_u128(sid).to_string())
                .set("seed", 42u64)
                .set("budget", "tokens=250000"),
        )
        .expect("valid attrs")
    }

    fn gate_fail(score: f32) -> PendingEvent {
        PendingEvent::new(
            EventKind::GateFail,
            Some(NodeId::new("draft")),
            EventAttrs::new()
                .set("gate_id", "rubric.v1")
                .set("reason_code", "GATE_REJECT")
                .set("score", score),
        )
        .expect("valid attrs")
    }

    /// T2 ① — a normal chain of N events verifies end to end.
    #[test]
    fn normal_chain_verifies() {
        let root = TempRoot::new("normal");
        let log = root.log(0xA0);
        for s in [0.9f32, 0.8, 0.7] {
            log.emit(gate_fail(s));
        }
        let emitted = log.emit(session_open(0xA0)); // seq 3 — kinds may interleave
        assert_eq!(emitted.seq, 3);
        assert_eq!(log.verify().expect("chain is intact"), 4);
    }

    /// T2 ② — rewriting an event's attrs in the file breaks recomputation at exactly
    /// that event (first-fault reporting, position + both hashes).
    #[test]
    fn tampered_attrs_detected_at_first_mismatch() {
        let root = TempRoot::new("tamper");
        let log = root.log(0xA1);
        for s in [0.9f32, 0.8, 0.7] {
            log.emit(gate_fail(s));
        }
        let path = log.path();

        // Tamper: event #1 (second line) gets its score silently changed.
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .expect("read log")
            .lines()
            .map(String::from)
            .collect();
        let mut value: serde_json::Value =
            serde_json::from_str(&lines[1]).expect("parse event line");
        value["attrs"]["score"] = serde_json::json!(1.0);
        lines[1] = value.to_string();
        std::fs::write(&path, lines.join("\n") + "\n").expect("rewrite log");

        let err = log.verify().expect_err("tamper must be detected");
        match err {
            TraceError::HashMismatch { seq, .. } => assert_eq!(seq, 1, "first mismatch #N"),
            other => panic!("expected HashMismatch, got {other:?}"),
        }
        // And the tampered log refuses further appends (open re-scans strictly).
        assert!(EventLog::open(&root.0, &SessionId::from_u128(0xA1)).is_err());
    }

    /// T2 ③ — a taxonomy-external kind in the file is refused at load/verify (SS-02
    /// rule 3: refusal is the correct behavior for off-vocabulary events).
    #[test]
    fn foreign_kind_line_is_invalid() {
        let root = TempRoot::new("foreign");
        let log = root.log(0xA2);
        log.emit(session_open(0xA2));
        let path = log.path();

        let foreign = r#"{"seq":1,"kind":"swarm.explode","node":null,"attrs":{},"prev_hash":"00","hash":"00","ts":0}"#;
        let mut content = std::fs::read_to_string(&path).expect("read");
        content.push_str(foreign);
        content.push('\n');
        std::fs::write(&path, content).expect("append foreign line");

        let err = log.verify().expect_err("foreign kind must be refused");
        assert!(
            matches!(err, TraceError::InvalidLine { line: 2 }),
            "unexpected: {err:?}"
        );
        // Type-level refusal too: EventKind cannot represent the foreign kind at all.
        assert!(serde_json::from_str::<EventKind>("\"swarm.explode\"").is_err());
    }

    /// T2 ④ — GateFail without its required attrs never becomes an event: the
    /// PendingEvent boundary refuses it, so nothing reaches the log.
    #[test]
    fn gate_fail_missing_attrs_refused_before_append() {
        let short = EventAttrs::new().set("gate_id", "rubric.v1"); // no reason_code/score
        let err = PendingEvent::new(EventKind::GateFail, Some(NodeId::new("draft")), short)
            .expect_err("must refuse");
        assert!(matches!(err, hesmos_core::SchemaError::MissingAttrs { .. }));

        // The log stays empty — refusal happened before any append path exists.
        let root = TempRoot::new("missingattrs");
        let log = root.log(0xA3);
        log.emit(session_open(0xA3));
        assert_eq!(log.verify().expect("intact"), 1);
    }

    /// T2 ⑤ — a spliced event (seq skip) is detected as a gap at its position.
    #[test]
    fn seq_gap_detected() {
        let root = TempRoot::new("gap");
        let log = root.log(0xA4);
        log.emit(session_open(0xA4));
        let path = log.path();

        // Craft a structurally valid event at seq 7 (correct link/hash) and splice it in.
        let prev = {
            let lines: Vec<String> = std::fs::read_to_string(&path)
                .expect("read")
                .lines()
                .map(String::from)
                .collect();
            let last: TraceEvent = serde_json::from_str(&lines[0]).expect("parse");
            last.hash
        };
        let attrs = EventAttrs::new().set("session_id", "spliced");
        let spliced = TraceEvent {
            seq: 7,
            kind: EventKind::SessionClose,
            node: None,
            prev_hash: prev.clone(),
            hash: chain_hash(&prev, 7, EventKind::SessionClose, None, &attrs),
            attrs: attrs.clone(),
            ts: 0,
        };
        let mut line = canonical_bytes(&spliced);
        line.push(b'\n');
        {
            let mut f = OpenOptions::new().append(true).open(&path).expect("append");
            f.write_all(&line).expect("write splice");
        }

        let err = log.verify().expect_err("gap must be detected");
        assert!(
            matches!(
                err,
                TraceError::SeqGap {
                    expected: 1,
                    found: 7
                }
            ),
            "unexpected: {err:?}"
        );
    }

    /// T2 ⑥ — `ts` is NOT a hash input (ADR-0006): rewriting timestamps passes
    /// verification. This is the intended behavior, pinned so nobody "fixes" it.
    #[test]
    fn ts_change_passes_verification_by_design() {
        let root = TempRoot::new("ts");
        let log = root.log(0xA5);
        for s in [0.9f32, 0.8] {
            log.emit(gate_fail(s));
        }
        let path = log.path();

        let rewritten: Vec<String> = std::fs::read_to_string(&path)
            .expect("read")
            .lines()
            .map(|l| {
                let mut v: serde_json::Value = serde_json::from_str(l).expect("parse");
                v["ts"] = serde_json::json!(1_234_567_890u64);
                v.to_string()
            })
            .collect();
        std::fs::write(&path, rewritten.join("\n") + "\n").expect("rewrite");

        assert_eq!(log.verify().expect("ts is outside the chain, by design"), 2);
    }

    /// Single-writer discipline: concurrent `try_append` from threads yields a gap-free
    /// chain — the mutex serializes, seqs stay dense, verification passes.
    #[test]
    fn concurrent_appends_stay_gap_free() {
        let root = TempRoot::new("concurrent");
        let log = std::sync::Arc::new(root.log(0xA6));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let log = log.clone();
                std::thread::spawn(move || {
                    log.emit(gate_fail(0.5 - i as f32 / 100.0));
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread");
        }
        assert_eq!(log.verify().expect("intact"), 8, "seqs 0..8 with no gaps");
    }

    /// `load` on an intact chain returns every event as verified evidence.
    #[test]
    fn load_returns_all_events_when_intact() {
        let root = TempRoot::new("load-ok");
        let log = root.log(0xA7);
        log.emit(session_open(0xA7));
        log.emit(gate_fail(0.9));
        match log.load() {
            LoadOutcome::Ok(events) => {
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].kind, EventKind::SessionOpen);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// `load` on a tampered chain keeps the parsed events (display material, ui-spec
    /// §4.4) AND names the first fault — the CLI-2 badge needs both.
    #[test]
    fn load_keeps_events_alongside_the_fault() {
        let root = TempRoot::new("load-tamper");
        let log = root.log(0xA8);
        log.emit(gate_fail(0.9));
        log.emit(gate_fail(0.8));
        let path = log.path();
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .expect("read log")
            .lines()
            .map(String::from)
            .collect();
        let mut value: serde_json::Value =
            serde_json::from_str(&lines[1]).expect("parse event line");
        value["attrs"]["score"] = serde_json::json!(0.1);
        lines[1] = value.to_string();
        std::fs::write(&path, lines.join("\n") + "\n").expect("rewrite log");

        match log.load() {
            LoadOutcome::Tampered { events, fault } => {
                assert_eq!(events.len(), 2, "the offending event is still shown");
                assert!(matches!(fault, TraceError::HashMismatch { seq: 1, .. }));
            }
            other => panic!("expected Tampered, got {other:?}"),
        }
    }
}
