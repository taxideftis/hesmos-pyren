//! Provider/tool callback registration (PY-3/PY-4, PT-10 inversion).
//!
//! The registry is GLOBAL (`register_provider`/`register_tool` carry no session
//! handle — FFI-1): decorators run at module import time, before any session exists.
//! Ownership stays simple because execution is GIL-serialised: `Py<PyAny>` is
//! Send+Sync, and the Mutex exists for pyo3's static requirements, not real races.
//!
//! The call path is deliberately MECHANICAL (backend.md "P1e 후속" consensus b): the
//! wrapper reports only what happened — an exception with its class name and message,
//! or a reply that does not match the LlmReply contract shape. It never interprets
//! (no timeout guesses, no retry advice); converting a failure into a runner
//! ReasonCode is the composition adapter's single mapping table (composition.rs).

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use pyo3::prelude::*;
use pyo3::types::PyAny;

use crate::marshal;

/// A mechanically observed callback failure: what happened, never what it means.
/// `kind` is the Python exception class name, or `LlmReplySchema` when the reply
/// crossed the boundary but did not match the PY-3 contract shape (a pseudo-class
/// marker — there is no Python exception for a wire-shape violation).
#[derive(Debug, Clone)]
pub struct CallbackFailure {
    pub kind: String,
    pub message: String,
}

#[derive(Default)]
struct Registry {
    /// model_ref (e.g. "glm-5.3-flash") -> callable(LlmRequest-dict) -> LlmReply-dict
    providers: HashMap<String, Py<PyAny>>,
    /// tool name (e.g. "write.file") -> callable(ToolCall-dict) -> ToolResult-dict.
    /// Dispatched by the executor (WP-P2e) for every gate-passed tool_call.
    tools: HashMap<String, Py<PyAny>>,
    /// Tools the tool layer declared as reading EXTERNAL data (WP-P2e registration
    /// flag). The executor feeds this declaration into core's
    /// `external_import_verdict` (SS-20 rule 1): an external tool returning a Clean
    /// ToolResult is an unmarked import and the result is rejected.
    external_tools: HashSet<String>,
    /// judge name (W5: "default") -> the registered judge (US-26). Re-registration
    /// REPLACES: judge config evolution between evals is the drift-tracking use case
    /// (AC2), and each session snapshots the judge at OPEN, so running sessions are
    /// unaffected by a later replacement.
    judges: HashMap<String, JudgeRegistration>,
}

/// A registered judge: the Python bridge callable plus the recording metadata. The
/// metadata is fixed at REGISTRATION time (prompt_hash from the config's prompt
/// text, temperature, model_version) — every judge.* attr on a gate event derives
/// from here, so a recorded judgment can never lack its record (SS-22 rule 3).
pub struct JudgeRegistration {
    /// callable(node_output: str) -> {"verdict": str, "score": f64,
    ///                                "tokens_in": u64, "tokens_out": u64}
    /// — the eval_judge bridge builds the judge LlmRequest itself (P2c adapter
    /// path) and parses the verdict; the FFI only accounts and records.
    pub callback: Py<PyAny>,
    pub prompt_hash: String,
    pub temperature: f64,
    pub model_version: String,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

/// Registers a provider callback; a duplicate model_ref is an FFI-CONTRACT violation
/// (one model, one callback — silent override would hide a wiring bug).
pub fn register_provider(py: Python<'_>, model_ref: &str, callback: Py<PyAny>) -> PyResult<()> {
    register(py, |r| &mut r.providers, "provider", model_ref, callback)
}

/// `external=true` declares the tool reads data from outside the session's trust
/// boundary (web fetch, MCP server, ...). Registration-time metadata — the tool
/// author knows this, the executor cannot infer it from a result's shape.
pub fn register_tool(
    py: Python<'_>,
    name: &str,
    callback: Py<PyAny>,
    external: bool,
) -> PyResult<()> {
    register(py, |r| &mut r.tools, "tool", name, callback)?;
    if external {
        REGISTRY
            .lock()
            .expect("callback registry poisoned")
            .external_tools
            .insert(name.to_string());
    }
    Ok(())
}

fn register(
    py: Python<'_>,
    select: impl FnOnce(&mut Registry) -> &mut HashMap<String, Py<PyAny>>,
    kind: &str,
    key: &str,
    callback: Py<PyAny>,
) -> PyResult<()> {
    if key.trim().is_empty() {
        return Err(marshal::contract_violation(
            py,
            format!("{kind} registration key must be a non-empty string"),
        ));
    }
    if !callback.bind(py).is_callable() {
        return Err(marshal::contract_violation(
            py,
            format!("{kind} callback must be callable (PY-3/PY-4 signature)"),
        ));
    }
    let mut registry = REGISTRY.lock().expect("callback registry poisoned");
    let table = select(&mut registry);
    if table.contains_key(key) {
        return Err(marshal::contract_violation(
            py,
            format!("{kind} `{key}` is already registered — duplicate registration"),
        ));
    }
    table.insert(key.to_string(), callback);
    Ok(())
}

/// Looks a provider up by model_ref. `None` = unregistered model — the adapter turns
/// this into an evidence-preserving StageFailure (bounded retry, then abort).
/// `clone_ref` needs the GIL token (pyo3 0.28: `Py` is not plain-`Clone`).
pub fn lookup_provider(py: Python<'_>, model_ref: &str) -> Option<Py<PyAny>> {
    REGISTRY
        .lock()
        .expect("callback registry poisoned")
        .providers
        .get(model_ref)
        .map(|callback| callback.clone_ref(py))
}

/// Looks a tool callback up by name (WP-P2e dispatch). `None` = unregistered tool —
/// the executor records the rejection as tool.call(ok=false); it never synthesises
/// a result.
pub fn lookup_tool(py: Python<'_>, name: &str) -> Option<Py<PyAny>> {
    REGISTRY
        .lock()
        .expect("callback registry poisoned")
        .tools
        .get(name)
        .map(|callback| callback.clone_ref(py))
}

/// The tool layer's registration-time externalness declaration for `name`
/// (SS-20 rule 1 — the executor consumes it, never re-judges permissions).
pub fn is_external_tool(name: &str) -> bool {
    REGISTRY
        .lock()
        .expect("callback registry poisoned")
        .external_tools
        .contains(name)
}

/// Registers the optional quality judge (US-26). Re-registration REPLACES the
/// previous judge under the same name — see the `judges` field note: open-time
/// snapshotting is what keeps a replacement from leaking into running sessions.
pub fn register_judge(
    py: Python<'_>,
    name: &str,
    registration: JudgeRegistration,
) -> PyResult<()> {
    if name.trim().is_empty() {
        return Err(marshal::contract_violation(
            py,
            "judge registration key must be a non-empty string".into(),
        ));
    }
    if !registration.callback.bind(py).is_callable() {
        return Err(marshal::contract_violation(
            py,
            "judge callback must be callable (bridge(output: str) -> dict)".into(),
        ));
    }
    if registration.prompt_hash.len() != 64 {
        return Err(marshal::contract_violation(
            py,
            "judge prompt_hash must be a 64-char sha256 hex string (SS-22 rule 3 — an \
             unhashable prompt cannot produce a drift-comparable record)"
                .into(),
        ));
    }
    REGISTRY
        .lock()
        .expect("callback registry poisoned")
        .judges
        .insert(name.to_string(), registration);
    Ok(())
}

/// Removes the judge registered under `name` (FFI-1 additive, test isolation).
/// The registry is process-global while judges are keyed by a fixed name, so a
/// suite that registers judges needs a deterministic way back to the rules-only
/// state (US-26 AC3); running sessions are unaffected either way (open-time
/// snapshot). Returns whether a registration was removed.
pub fn clear_judge(name: &str) -> bool {
    REGISTRY
        .lock()
        .expect("callback registry poisoned")
        .judges
        .remove(name)
        .is_some()
}

/// The judge registered under `name` (W5 uses "default"), snapshotted by the
/// executor at session OPEN — later replacements affect only future sessions.
/// `clone_ref` needs the GIL token (pyo3 0.28: `Py` is not plain-`Clone`).
pub fn lookup_judge(py: Python<'_>, name: &str) -> Option<JudgeRegistration> {
    REGISTRY
        .lock()
        .expect("callback registry poisoned")
        .judges
        .get(name)
        .map(|registration| JudgeRegistration {
            callback: registration.callback.clone_ref(py),
            prompt_hash: registration.prompt_hash.clone(),
            temperature: registration.temperature,
            model_version: registration.model_version.clone(),
        })
}

/// Calls a registered callback with one JSON argument and returns the JSON reply.
/// Every failure is reported mechanically — the caller (composition adapter) owns
/// the single ReasonCode mapping (consensus b).
pub fn call(
    py: Python<'_>,
    callback: &Py<PyAny>,
    arg: &serde_json::Value,
) -> Result<serde_json::Value, CallbackFailure> {
    let py_arg = marshal::value_to_py(py, arg).map_err(|e| CallbackFailure {
        kind: "TypeError".into(),
        message: format!("request not marshalable to Python: {e}"),
    })?;
    match callback.bind(py).call1((py_arg,)) {
        Ok(result) => marshal::py_to_value(result.as_any()).map_err(|e| CallbackFailure {
            kind: "TypeError".into(),
            message: format!("reply not JSON-marshalable: {e}"),
        }),
        // The exception itself IS the fact: class name + message, structured.
        Err(err) => Err(CallbackFailure {
            kind: err
                .get_type(py)
                .name()
                .ok()
                .and_then(|name| name.to_str().ok().map(str::to_string))
                .unwrap_or_else(|| "<unknown>".into()),
            message: err.value(py).to_string(),
        }),
    }
}
