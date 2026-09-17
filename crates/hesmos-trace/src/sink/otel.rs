//! SS-25 / ADR-0007 — the opt-in OTel export adapter (`hesmos-trace::sink::otel`).
//!
//! Ownership boundary (ADR-0007): Hesmos EXPORTS events to a bathos-owned OTel
//! collector; collecting, aggregating and dashboards are bathos infrastructure and
//! are NOT reimplemented here (P8). This module is a thin adapter only: one Hesmos
//! event becomes one OTel log record (kind vocabulary, node, attributes verbatim).
//! Events are deliberately NOT forced into spans — ADR-0003#3-(a) rejected the
//! "OTel spans only" reading because spans alone cannot reconstruct a run; export
//! here is an ADDITIONAL sink next to the local hash chain, never a replacement.
//!
//! Default OFF (SS-25 rule 1): this module compiles only under the `otel` feature,
//! and even a feature build exports nothing unless `HESMOS_OTEL_ENDPOINT` is set.
//! An opt-out deployment sends zero bytes (CF-18) and prints nothing (ui-spec
//! §8.6 — silence is correct for the default). Transport is OTLP/HTTP-protobuf
//! (the crate's non-deprecated default); the collector address points at bathos.

use hesmos_core::{EventSink, PendingEvent, Sha256Hex, TraceEvent};
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry_otlp::{LogExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;

/// The opt-in switch. Absent env → no exporter is ever constructed.
pub const ENDPOINT_ENV: &str = "HESMOS_OTEL_ENDPOINT";

/// The exporter identity reported to the collector.
const SERVICE_NAME: &str = "hesmos";
const LOGGER_NAME: &str = "hesmos.trace";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtelConfig {
    pub endpoint: String,
}

/// Reads the opt-in switch. `None` = telemetry stays off — the ONLY sanctioned
/// default (an env opt-OUT model is forbidden by SS-25 rule 1). A blank value is
/// not an opt-in: an exporter pointed nowhere would just manufacture failures.
pub fn config_from_env() -> Option<OtelConfig> {
    std::env::var(ENDPOINT_ENV)
        .ok()
        .map(|endpoint| endpoint.trim().to_string())
        .filter(|endpoint| !endpoint.is_empty())
        .map(|endpoint| OtelConfig { endpoint })
}

/// The export adapter. Constructing one IS the opt-in act: no `OtelExporter` in
/// scope, no send path exists at runtime.
pub struct OtelExporter {
    logger: opentelemetry_sdk::logs::SdkLogger,
    provider: SdkLoggerProvider,
}

impl OtelExporter {
    /// Builds the OTLP log exporter + provider pointed at the bathos collector.
    /// Connection-config errors surface here, at the explicit opt-in moment —
    /// never as a silent drop later.
    pub fn connect(config: &OtelConfig) -> Result<Self, String> {
        let exporter = LogExporter::builder()
            .with_http()
            .with_endpoint(&config.endpoint)
            .build()
            .map_err(|e| format!("OTel exporter build 실패 ({}): {e}", config.endpoint))?;
        let provider = SdkLoggerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                Resource::builder_empty()
                    .with_service_name(SERVICE_NAME)
                    .build(),
            )
            .build();
        let logger = provider.logger(LOGGER_NAME);
        Ok(Self { logger, provider })
    }

    /// Exports ONE confirmed event as one OTel log record. Attributes travel
    /// verbatim (the event schema is the wire schema — no reinterpretation, the
    /// same rule platform.rs applies to bathos output).
    pub fn export(&self, event: &TraceEvent) {
        let mut record = self.logger.create_log_record();
        record.set_body(AnyValue::from(event.kind.as_vocab()));
        record.set_severity_number(event_severity(event));
        record.add_attribute("hesmos.event.kind", event.kind.as_vocab());
        if let Some(node) = &event.node {
            // add_attribute requires 'static — pass an owned String, not a borrow.
            record.add_attribute("hesmos.event.node", node.as_str().to_string());
        }
        if let Ok(serde_json::Value::Object(map)) = serde_json::to_value(&event.attrs) {
            for (key, value) in map {
                record.add_attribute(key, json_to_any(value));
            }
        }
        self.logger.emit(record);
    }

    /// Flushes buffered records to the collector. The batch processor owns timing;
    /// a short-lived CLI process must call this before exit or the tail of the
    /// export is lost.
    pub fn flush(&self) {
        let _ = self.provider.force_flush();
    }
}

/// Severity: fails are WARN-grade at the collector, everything else INFO. A display
/// convention only — the kind attribute remains the authoritative signal.
fn event_severity(event: &TraceEvent) -> Severity {
    match event.kind {
        hesmos_core::EventKind::GateFail => Severity::Warn,
        _ => Severity::Info,
    }
}

/// Attr JSON → OTel value: the recorded JSON types map losslessly onto the shapes
/// OTel accepts (logs carry i64, so u64 narrows with a saturating guard); objects
/// and arrays degrade to their canonical text — read-only structure at the collector.
fn json_to_any(value: serde_json::Value) -> AnyValue {
    match value {
        serde_json::Value::String(s) => AnyValue::from(s),
        serde_json::Value::Bool(b) => AnyValue::from(b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(AnyValue::from)
            .unwrap_or_else(|| AnyValue::from(n.to_string())),
        other => AnyValue::from(other.to_string()),
    }
}

/// The standalone PORT-1 face (story: "EventSink의 또 다른 impl"). Useful when a
/// deployment exports WITHOUT keeping a local chain — the collector is then the
/// only persistence, which bathos's ownership of collection sanctions.
///
/// The returned [`TraceEvent`] is deliberately NOT chain-linked (seq 0, genesis
/// hashes): this sink owns no chain, and fabricating link fields would create
/// chain-shaped bytes that could be mistaken for evidence. The composition root's
/// fanout (hesmos `composition::progress_sink`) discards this return value and
/// chains through the EventLog instead.
impl EventSink for OtelExporter {
    fn emit(&self, e: PendingEvent) -> TraceEvent {
        let neutral = Sha256Hex::parse(crate::log::GENESIS_PREV_HASH).expect("genesis hex");
        let event = TraceEvent {
            seq: 0,
            kind: e.kind,
            node: e.node,
            prev_hash: neutral.clone(),
            hash: neutral,
            attrs: e.attrs,
            ts: 0,
        };
        self.export(&event);
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::{EventAttrs, EventKind, NodeId};
    use opentelemetry_sdk::logs::InMemoryLogExporterBuilder;

    fn gate_fail() -> PendingEvent {
        PendingEvent::new(
            EventKind::GateFail,
            Some(NodeId::new("draft")),
            EventAttrs::new()
                .set("gate_id", "permission")
                .set("reason_code", "GATE_REJECT")
                .set("score", 0.0f32),
        )
        .expect("valid attrs")
    }

    /// The opt-in switch: no env → None (the SS-25 default), blank env → None (a
    /// valueless opt-in would just point the exporter nowhere).
    ///
    /// Env is process-global and tests run multithreaded: the static mutex
    /// serializes this test's mutations (edition 2024 marks set_var unsafe —
    /// the unsafe blocks are the price of the documented contract).
    #[test]
    fn config_requires_an_explicit_endpoint() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        unsafe { std::env::remove_var(ENDPOINT_ENV) };
        assert!(
            config_from_env().is_none(),
            "default OFF — no env, no export"
        );
        unsafe { std::env::set_var(ENDPOINT_ENV, "   ") };
        assert!(
            config_from_env().is_none(),
            "blank endpoint is not an opt-in"
        );
        unsafe { std::env::set_var(ENDPOINT_ENV, "http://127.0.0.1:4318") };
        let cfg = config_from_env().expect("set");
        assert_eq!(cfg.endpoint, "http://127.0.0.1:4318");
        unsafe { std::env::remove_var(ENDPOINT_ENV) };
    }

    /// The export mapping: every emitted event reaches the exporter as exactly one
    /// record carrying the kind vocabulary and the event's own attributes. The
    /// in-memory exporter replaces the OTLP transport — the network is the
    /// exporter crate's concern (and the bathos collector's).
    #[test]
    fn emit_exports_one_record_per_event() {
        let memory = InMemoryLogExporterBuilder::default().build();
        let provider = SdkLoggerProvider::builder()
            .with_simple_exporter(memory.clone())
            .build();
        let exporter = OtelExporter {
            logger: provider.logger(LOGGER_NAME),
            provider,
        };

        let chained = exporter.emit(gate_fail());
        exporter.flush();

        // The EventSink face returns a deliberately NON-chained event (this sink
        // owns no chain — see the impl doc): neutral hashes, seq 0.
        assert_eq!(chained.seq, 0);

        let records = memory.get_emitted_logs().expect("records");
        assert_eq!(records.len(), 1, "one event → exactly one exported record");
        let record = &records[0].record;
        let attrs: Vec<(String, String)> = record
            .attributes_iter()
            .map(|(k, v)| (k.as_str().to_string(), format!("{v:?}")))
            .collect();
        assert!(
            attrs
                .iter()
                .any(|(k, v)| k == "hesmos.event.kind" && v.contains("gate.fail")),
            "kind vocabulary exported: {attrs:?}"
        );
        assert!(
            attrs
                .iter()
                .any(|(k, v)| k == "hesmos.event.node" && v.contains("draft")),
            "node exported: {attrs:?}"
        );
        assert!(
            attrs
                .iter()
                .any(|(k, v)| k == "gate_id" && v.contains("permission")),
            "event attrs travel verbatim: {attrs:?}"
        );
        assert!(
            attrs
                .iter()
                .any(|(k, v)| k == "reason_code" && v.contains("GATE_REJECT")),
            "reason code exported: {attrs:?}"
        );
    }
}
