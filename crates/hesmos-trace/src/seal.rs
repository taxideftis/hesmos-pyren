//! trace.seal — closing the evidence chain and joining it to the bathos audit (SS-03).
//!
//! Order of facts, fixed here:
//! 1. the chain is verified one last time (a tampered log refuses to seal — you cannot
//!    attest evidence you cannot read);
//! 2. a `trace.seal` event is appended whose `chain_head_hash` attribute names the head
//!    BEFORE the seal event itself (the attr must be computable before append, and it
//!    is exactly the value SS-03 rule 1 hands to the audit: "audit_append는 trace.seal의
//!    chain_head_hash만 실어 보낸다");
//! 3. the same value is submitted through PORT-2 `audit_append`, immediately followed by
//!    `audit_verify` — a failed verify leaves the session evidence-invalid (exit 30,
//!    SS-03 rule 2), never "sealed anyway".
//!
//! Re-sealing is USAGE-STATE (exceptions.md §6): the seal event is the terminal marker
//! of a completed session, and a second one would attest a chain that already attested
//! itself. The CLI maps [`SealError::AlreadySealed`] to exit 2.
//!
//! `engine: None` is the P1 "bathos absent" path (ponytail, recorded in backend.md):
//! the local seal event is still written — evidence exists — but the audit join is
//! deferred and `audit_verified` comes back `None`, which the CLI renders as
//! `bathos_audit=unavailable`. Absence of the engine is not a verify FAILURE; conflating
//! the two would fake a Platform error the engine never produced.

use thiserror::Error;

use hesmos_core::{
    AuditPayload, BathosEngine, EventAttrs, EventKind, EventSink, PendingEvent, Sha256Hex,
};

use crate::log::{EventLog, GENESIS_PREV_HASH, LoadOutcome, TraceError};

#[derive(Debug, Error)]
pub enum SealError {
    #[error("cannot seal: {0}")]
    Chain(#[from] TraceError),
    #[error("session is already sealed (USAGE-STATE)")]
    AlreadySealed,
    #[error("bathos audit submission failed: exit {bathos_exit}")]
    Audit {
        bathos_exit: i32,
        bathos_code: Option<String>,
    },
}

/// The seal result: the audited head and what the audit said about it.
/// `audit_verified: None` = no bathos engine was available (join deferred, P1).
#[derive(Debug, Clone, PartialEq)]
pub struct SealOutcome {
    pub chain_head: Sha256Hex,
    pub audit_verified: Option<bool>,
}

/// Seals the session's chain and joins it to bathos audit when an engine is available.
pub fn seal(log: &EventLog, engine: Option<&dyn BathosEngine>) -> Result<SealOutcome, SealError> {
    let events = match log.load() {
        LoadOutcome::Ok(events) => events,
        LoadOutcome::Tampered { fault, .. } => return Err(SealError::Chain(fault)),
        LoadOutcome::Io(e) => return Err(SealError::Chain(TraceError::Io(e))),
    };
    if events
        .last()
        .is_some_and(|e| e.kind == EventKind::TraceSeal)
    {
        return Err(SealError::AlreadySealed);
    }
    // An eventless session seals to the genesis marker (degenerate but representable —
    // the head of an empty chain IS genesis).
    let head = events
        .last()
        .map(|e| e.hash.clone())
        .unwrap_or_else(|| Sha256Hex::parse(GENESIS_PREV_HASH).expect("genesis is valid hex"));

    log.emit(
        PendingEvent::new(
            EventKind::TraceSeal,
            None,
            EventAttrs::new().set("chain_head_hash", head.as_str()),
        )
        .expect("trace.seal carries its one required attr"),
    );

    let audit_verified = match engine {
        None => None,
        Some(engine) => {
            engine
                .audit_append(AuditPayload {
                    chain_head_hash: head.clone(),
                })
                .map_err(|p| SealError::Audit {
                    bathos_exit: p.bathos_exit,
                    bathos_code: p.bathos_code,
                })?;
            let report = engine.audit_verify().map_err(|p| SealError::Audit {
                bathos_exit: p.bathos_exit,
                bathos_code: p.bathos_code,
            })?;
            // ok:false = bathos found its own audit chain broken — the session's
            // evidence can not be joined, which is the exit-30 shape, not a pass.
            if !report.ok {
                return Err(SealError::Audit {
                    bathos_exit: 1,
                    bathos_code: Some("E-AUDIT-TAMPER".into()),
                });
            }
            Some(true)
        }
    };

    Ok(SealOutcome {
        chain_head: head,
        audit_verified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    use hesmos_core::{
        GateRecord, GateRecordId, GateReport, ModelReport, PlatformError, RawReport, SessionId,
        StateReport, VerifyReport, WaveRef, WaveReport,
    };

    struct TempRoot(std::path::PathBuf);
    impl TempRoot {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "hesmos-trace-seal-{name}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
        fn log(&self, sid: u128) -> EventLog {
            EventLog::open(&self.0, &SessionId::from_u128(sid)).expect("open")
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Audit double recording every submitted head; `verify_ok` decides what
    /// `audit_verify` reports.
    struct FakeAudit {
        appended: Mutex<Vec<Sha256Hex>>,
        verify_ok: bool,
    }
    impl FakeAudit {
        fn new(verify_ok: bool) -> Self {
            Self {
                appended: Mutex::new(Vec::new()),
                verify_ok,
            }
        }
    }
    impl BathosEngine for FakeAudit {
        fn state_init(&self, _root: &Path) -> Result<StateReport, PlatformError> {
            unimplemented!()
        }
        fn state_validate(&self) -> Result<StateReport, PlatformError> {
            unimplemented!()
        }
        fn gate_verdict(&self, _record: GateRecord) -> Result<(), PlatformError> {
            unimplemented!()
        }
        fn gate_show(&self, _id: &GateRecordId) -> Result<GateReport, PlatformError> {
            unimplemented!()
        }
        fn audit_append(&self, payload: AuditPayload) -> Result<(), PlatformError> {
            self.appended
                .lock()
                .expect("mutex")
                .push(payload.chain_head_hash);
            Ok(())
        }
        fn audit_verify(&self) -> Result<VerifyReport, PlatformError> {
            Ok(VerifyReport {
                ok: self.verify_ok,
                raw: RawReport::default(),
            })
        }
        fn model_validate(&self) -> Result<ModelReport, PlatformError> {
            unimplemented!()
        }
        fn wave_activate(&self, _wave: WaveRef) -> Result<(), PlatformError> {
            unimplemented!()
        }
        fn wave_show(&self) -> Result<WaveReport, PlatformError> {
            unimplemented!()
        }
    }

    fn open_event(log: &EventLog, sid: u128) {
        log.emit(
            PendingEvent::new(
                EventKind::SessionOpen,
                None,
                EventAttrs::new()
                    .set("session_id", SessionId::from_u128(sid).to_string())
                    .set("seed", 42u64)
                    .set("budget", "tokens=250000"),
            )
            .expect("valid"),
        );
    }

    /// SS-03 rule 1, end to end: the seal event's attr, the audit payload, and the
    /// returned head are one and the same value; a second seal is USAGE-STATE.
    #[test]
    fn seal_records_head_and_audits_it() {
        let root = TempRoot::new("ok");
        let log = root.log(0xB0);
        open_event(&log, 0xB0);
        let audit = FakeAudit::new(true);

        let outcome = seal(&log, Some(&audit)).expect("seal");
        assert_eq!(outcome.audit_verified, Some(true));
        assert_eq!(
            std::slice::from_ref(&outcome.chain_head),
            audit.appended.lock().expect("mutex").as_slice(),
            "exactly the seal head is submitted"
        );

        let events = match log.load() {
            LoadOutcome::Ok(events) => events,
            LoadOutcome::Tampered { fault, .. } => panic!("tampered: {fault}"),
            LoadOutcome::Io(e) => panic!("io: {e}"),
        };
        let seal_event = events.last().expect("seal appended");
        assert_eq!(seal_event.kind, EventKind::TraceSeal);
        assert_eq!(
            seal_event.attrs.get_str("chain_head_hash"),
            Some(outcome.chain_head.as_str()),
            "attr names the pre-seal head"
        );
        // And the head itself is the second-to-last event (the seal's predecessor).
        assert_eq!(events[events.len() - 2].hash, outcome.chain_head);

        // Re-seal refuses: USAGE-STATE (exceptions §6).
        assert!(matches!(
            seal(&log, Some(&audit)),
            Err(SealError::AlreadySealed)
        ));
    }

    /// A bathos verify failure must fail the seal (exit 30 shape) — never "sealed
    /// anyway" with broken evidence.
    #[test]
    fn failed_audit_verify_fails_the_seal() {
        let root = TempRoot::new("verify-fail");
        let log = root.log(0xB1);
        open_event(&log, 0xB1);
        let audit = FakeAudit::new(false);
        assert!(matches!(
            seal(&log, Some(&audit)),
            Err(SealError::Audit { bathos_exit: 1, .. })
        ));
    }

    /// Engine absent → local seal stands, audit join deferred (`None`), P1 path.
    #[test]
    fn engine_absent_defers_the_audit_join() {
        let root = TempRoot::new("no-engine");
        let log = root.log(0xB2);
        open_event(&log, 0xB2);
        let outcome = seal(&log, None).expect("seal");
        assert_eq!(outcome.audit_verified, None);
    }

    /// A tampered chain refuses to seal — you cannot attest evidence you cannot read.
    #[test]
    fn tampered_chain_refuses_to_seal() {
        let root = TempRoot::new("tampered");
        let log = root.log(0xB3);
        open_event(&log, 0xB3);
        // Tamper via direct file rewrite (the only way — the log has no mutation API):
        // change the seed attr so the stored hash no longer recomputes.
        let path = root
            .0
            .join(".hesmos")
            .join("sessions")
            .join(SessionId::from_u128(0xB3).to_string())
            .join("events.jsonl");
        let content = std::fs::read_to_string(&path).expect("read");
        let tampered = content.replace("\"seed\":42", "\"seed\":43");
        std::fs::write(&path, tampered).expect("rewrite");
        assert!(matches!(seal(&log, None), Err(SealError::Chain(_))));
    }
}
