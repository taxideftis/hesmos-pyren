//! PORT-2 BathosEngine — the only contact surface with the bathos engine (P8, 표 4).
//!
//! Five CLI families, nine methods, nothing more (SS-23 rule 1). The implementing
//! adapter (WP-P1e, `hesmos-orchestrator::platform`) shells out to the bathos CLI — it
//! never touches engine internals or storage. Report payloads are passed through as raw
//! JSON on purpose: re-interpreting bathos output here would recreate a second source of
//! truth for engine-owned assets, which P8 forbids.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ids::Sha256Hex;

/// bathos exit code + E-* code, passed through un-reinterpreted (exceptions.md §5: the
/// adapter only wraps; Hesmos codes never replace bathos codes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformError {
    pub bathos_exit: i32,
    pub bathos_code: Option<String>,
}

/// Raw report body from a bathos CLI call. Kept opaque by design — see module doc.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RawReport(pub serde_json::Value);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateReport {
    pub ok: bool,
    pub raw: RawReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GateRecordId(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRecord {
    pub id: GateRecordId,
    /// PASS | CONCERNS | FAIL — the bathos vocabulary, never a Hesmos invention.
    pub verdict: String,
    pub critical: u32,
    pub raw: RawReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateReport {
    pub raw: RawReport,
}

/// `audit_append` payload — exactly the trace seal's chain head (SS-03 rule 1: Hesmos
/// keeps no second audit chain).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditPayload {
    pub chain_head_hash: Sha256Hex,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyReport {
    pub ok: bool,
    pub raw: RawReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelReport {
    pub ok: bool,
    pub raw: RawReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaveRef {
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaveReport {
    pub raw: RawReport,
}

/// PORT-2 (T11 contract, James's design). Surface is frozen: state init/validate ·
/// gate verdict/show · audit append/verify · model validate · wave activate/show.
pub trait BathosEngine: Send + Sync {
    fn state_init(&self, root: &Path) -> Result<StateReport, PlatformError>;
    fn state_validate(&self) -> Result<StateReport, PlatformError>;
    fn gate_verdict(&self, record: GateRecord) -> Result<(), PlatformError>;
    fn gate_show(&self, id: &GateRecordId) -> Result<GateReport, PlatformError>;
    /// Submits the trace seal (chain head only).
    fn audit_append(&self, payload: AuditPayload) -> Result<(), PlatformError>;
    /// Failure here leaves the session evidence-invalid → exit 30 (SS-03 rule 2).
    fn audit_verify(&self) -> Result<VerifyReport, PlatformError>;
    /// E-MODEL-MIX detection lives in bathos; Hesmos must not re-implement it (SS-17 rule 2).
    fn model_validate(&self) -> Result<ModelReport, PlatformError>;
    fn wave_activate(&self, wave: WaveRef) -> Result<(), PlatformError>;
    fn wave_show(&self) -> Result<WaveReport, PlatformError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit payload round-trips and refuses extra fields — the seal surface is exactly
    /// the chain head.
    #[test]
    fn audit_payload_is_chain_head_only() {
        let payload = AuditPayload {
            chain_head_hash: Sha256Hex::parse("c".repeat(64)).expect("hex"),
        };
        let bytes = crate::canonical_bytes(&payload);
        assert_eq!(
            String::from_utf8(bytes.clone()).expect("utf8"),
            format!("{{\"chain_head_hash\":\"{}\"}}", "c".repeat(64))
        );
        let back: AuditPayload = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(back, payload);
    }

    /// PlatformError passes bathos codes through verbatim (no Hesmos re-mapping).
    #[test]
    fn platform_error_passthrough() {
        let err = PlatformError {
            bathos_exit: 2,
            bathos_code: Some("E-MODEL-MIX".into()),
        };
        let json = serde_json::to_string(&err).expect("json");
        assert!(json.contains("E-MODEL-MIX"));
    }
}
