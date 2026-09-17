//! hesmos-ffi — the FFI-1 immutable surface (api-contracts FFI-1).
//!
//! This is the ONLY Python→core path (SS-16 rule 1: no bypass exists). P0c exposes
//! the skeleton subset: ① session_open ② plan_from_yaml ⑨ session_close; the full
//! nine-function surface completes at WP-P2b. Surface additions require an
//! api-contracts revision (forbidden: arbitrary Rust exposure, internal structures).
//!
//! P0c seam: core schema consumption (hesmos-core types, session lifecycle, event
//! emission) activates when WP-P0a lands. Until then the handle is constructed here
//! with contract-shaped values so the marshaling layer, error taxonomy, and Python
//! wrappers are provable in isolation (build-plan §3 P0: "session_open·plan_from_
//! yaml·session_close 마셜링 + pydantic 미러 생성기까지").

mod callbacks;
mod marshal;

use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyModule};

/// FFI-1 ① session_open(seed?, budget?, team_id?) -> SessionHandle-shaped dict.
///
/// seed=None generates the seed here and records it in the handle (PY-1 note — same
/// rule as CLI-1 --seed: OS entropy, then immutable). budget is the CLI-1 spec form
/// {tokens[, cost_usd]}; the fixed envelope is core-side (SS-15 rule 1).
#[pyfunction]
fn session_open<'py>(
    py: Python<'py>,
    seed: Option<u64>,
    budget: Option<&Bound<'py, PyDict>>,
    team_id: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let budget_value = match budget {
        Some(b) => {
            let v = marshal::py_to_value(b.as_any())?;
            validate_budget_spec(py, &v)?;
            Some(v)
        }
        None => None,
    };

    // Session ids are unique per session/run — uniqueness (not reproducibility) is
    // required here; trace reproduction keys off plan_hash+seed (ADR-0006).
    let session_id = ulid::Ulid::generate().to_string();
    let run_id = ulid::Ulid::generate().to_string();
    // ponytail: seed entropy borrowed from ULID random bytes; replace with a dedicated
    // rng only if seed quality is ever questioned (seed is recorded, not secret).
    let seed = seed.unwrap_or_else(new_seed_from_ulid);

    let out = PyDict::new(py);
    out.set_item("session_id", session_id)?;
    out.set_item("run_id", run_id)?;
    out.set_item("seed", seed)?;
    out.set_item("budget", match budget_value {
        Some(v) => marshal::value_to_py(py, &v)?,
        None => py.None().into_bound(py),
    })?;
    out.set_item("team_id", team_id)?;
    out.set_item("state", "Init")?;
    // No plan_hash yet: the plan enters at session_run (PY-5) and plan.compiled
    // produces it (TYPE-5) — reconcile the open-time handle shape with core at P0a.
    Ok(out)
}

fn new_seed_from_ulid() -> u64 {
    let bytes = ulid::Ulid::generate().to_bytes();
    u64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
}

/// Boundary check for the budget spec (trust boundary — FFI-SCHEMA on violation).
/// Spec form only: {"tokens": int>=0} with optional {"cost_usd": number>=0}.
fn validate_budget_spec(py: Python<'_>, value: &serde_json::Value) -> PyResult<()> {
    let Some(obj) = value.as_object() else {
        return Err(marshal::schema_violation(py, "budget must be an object {tokens[, cost_usd]}".into()));
    };
    match obj.get("tokens") {
        Some(serde_json::Value::Number(n)) if n.as_u64().is_some() => {}
        _ => {
            return Err(marshal::schema_violation(
                py,
                "budget.tokens must be a non-negative integer".into(),
            ))
        }
    }
    if let Some(cost) = obj.get("cost_usd") {
        if !cost.is_number() {
            return Err(marshal::schema_violation(py, "budget.cost_usd must be a number".into()));
        }
    }
    Ok(())
}

/// FFI-1 ② plan_from_yaml(text) -> plan dict | CompileError(CE-01).
///
/// P0c scope: CE-01 schema-parse level only (story: "컴파일(WP-P1a) 전까지 스키마
/// 파싱(CE-01 수준)만 유효") — YAML syntax plus the minimal top-level plan surface.
/// Unknown node refs, cycles, and the rest of CE-02..09 arrive with the core compiler.
#[pyfunction]
fn plan_from_yaml<'py>(py: Python<'py>, text: &str) -> PyResult<Bound<'py, PyAny>> {
    let value: serde_json::Value = serde_yaml_ng::from_str(text).map_err(|e| {
        marshal::raise_hesmos(
            py,
            "CompileError",
            "Compile",
            "CE-01",
            format!("plan YAML schema parse failed: {e}"),
            "fix the plan file (CE-01 — location included in the message)",
        )
    })?;
    let Some(obj) = value.as_object() else {
        return Err(ce01(py, "plan must be a YAML mapping"));
    };
    let Some(stages) = obj.get("stages") else {
        return Err(ce01(py, "plan.stages is required"));
    };
    let Some(stage_list) = stages.as_array() else {
        return Err(ce01(py, "plan.stages must be a list"));
    };
    for stage in stage_list {
        let Some(stage_obj) = stage.as_object() else {
            return Err(ce01(py, "every plan.stages entry must be a mapping"));
        };
        if !matches!(stage_obj.get("id"), Some(serde_json::Value::String(_))) {
            return Err(ce01(py, "every stage requires a string id"));
        }
    }
    marshal::value_to_py(py, &value)
}

fn ce01(py: Python<'_>, message: &str) -> PyErr {
    marshal::raise_hesmos(
        py,
        "CompileError",
        "Compile",
        "CE-01",
        message.to_string(),
        "fix the plan file — full compile checks (CE-02..09) land with WP-P1a",
    )
}

/// FFI-1 ⑨ session_close(handle) -> None.
///
/// Skeleton validates the boundary shape only; the real close emits session.close
/// via the core EventSink (P6 — single event path) once core is linked (P0a/P2b).
#[pyfunction]
fn session_close(py: Python<'_>, handle: &Bound<'_, PyAny>) -> PyResult<()> {
    let value = marshal::py_to_value(handle)?;
    match value.get("session_id") {
        Some(serde_json::Value::String(_)) => Ok(()),
        _ => Err(marshal::schema_violation(
            py,
            "handle must carry a string session_id".into(),
        )),
    }
}

#[pymodule]
fn _ffi(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(session_open, module)?)?;
    module.add_function(wrap_pyfunction!(plan_from_yaml, module)?)?;
    module.add_function(wrap_pyfunction!(session_close, module)?)?;
    Ok(())
}
