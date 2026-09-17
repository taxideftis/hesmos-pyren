//! Additional PORT-1 sinks. The default sink (the hash-chain EventLog) lives in
//! `log`; this module holds OPTIONAL export-only sinks.
//!
//! Currently: `otel` (SS-25 / ADR-0007) — compiled only under the `otel` feature,
//! so the default build contains no remote send path at all (CF-18: opt-out means
//! zero sends, structurally).

#[cfg(feature = "otel")]
pub mod otel;
