//! Python↔serde boundary for the FFI-1 surface (marshal-only per D-3).
//!
//! Wire convention (mirrors the header of the generated hesmos/models.py):
//! - structs: JSON objects with the Rust field names;
//! - payload enums: internally tagged `{"kind": "<Variant>", ...}` (core serde emits
//!   externally tagged — conversion to the internal tag happens here, in the single
//!   marshal module owned by the FFI both-ends owner, boundary rule 2);
//! - fieldless enums: plain strings.
//!
//! Errors raised into Python are the PY-6 classes from `hesmos.exceptions` — one
//! exception hierarchy, never a Rust-side duplicate (PY-6 verify: single parent chain).

use std::collections::HashMap;

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyType};

/// Raise a PY-6 exception class by name with the contract's positional order
/// (error_class, code, session_id, node_id, message, hint).
pub fn raise_hesmos(
    py: Python<'_>,
    class_name: &str,
    error_class: &str,
    code: &str,
    message: String,
    hint: &str,
) -> PyErr {
    let exc_type = match py
        .import("hesmos.exceptions")
        .and_then(|m| m.getattr(class_name))
        .and_then(|t| t.cast_into::<PyType>().map_err(PyErr::from))
    {
        Ok(t) => t,
        // Only reachable if the pure-Python layer is broken — surface it verbatim.
        Err(e) => return e,
    };
    PyErr::from_type(
        exc_type,
        (
            error_class.to_string(),
            code.to_string(),
            None::<String>,
            None::<String>,
            message,
            hint.to_string(),
        ),
    )
}

/// FFI-SCHEMA: dual-validation failure at the boundary — core state stays untouched
/// (exceptions.md §7, US-01 AC2).
pub fn schema_violation(py: Python<'_>, message: String) -> PyErr {
    raise_hesmos(py, "FfiError", "Ffi", "FFI-SCHEMA", message, "check the payload against hesmos.models")
}

/// Convert a Python object into a JSON value. Total over None/bool/int/float/str/
/// dict/list; anything else is an FFI-SCHEMA rejection (type-level, before semantics).
pub fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    if obj.is_none() {
        return Ok(serde_json::Value::Null);
    }
    if obj.is_instance_of::<PyBool>() {
        return Ok(serde_json::Value::Bool(obj.extract::<bool>()?));
    }
    // Int before float: bool is an int subtype in Python, hence the explicit Bool arm above.
    if obj.is_instance_of::<PyInt>() {
        return match obj.extract::<u64>() {
            Ok(i) => Ok(serde_json::Value::from(i)),
            Err(_) => obj
                .extract::<i64>()
                .map(serde_json::Value::from)
                .map_err(|_| PyTypeError::new_err("integer out of JSON range")),
        };
    }
    if obj.is_instance_of::<PyFloat>() {
        return Ok(serde_json::Value::from(obj.extract::<f64>()?));
    }
    if obj.is_instance_of::<PyString>() {
        return Ok(serde_json::Value::from(obj.extract::<String>()?));
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            map.insert(key, py_to_value(&v)?);
        }
        return Ok(serde_json::Value::Object(map));
    }
    if let Ok(list) = obj.cast::<PyList>() {
        let mut items = Vec::with_capacity(list.len());
        for item in list.iter() {
            items.push(py_to_value(&item)?);
        }
        return Ok(serde_json::Value::Array(items));
    }
    Err(PyTypeError::new_err(format!(
        "unsupported boundary type: {}",
        obj.get_type().name()?
    )))
}

/// Convert a JSON value back into Python objects (dicts/lists/primitives only —
/// the shapes the generated mirror validates).
pub fn value_to_py<'py>(py: Python<'py>, value: &serde_json::Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        serde_json::Value::Null => py.None().into_bound(py),
        serde_json::Value::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_u64() {
                PyInt::new(py, i).into_any()
            } else if let Some(i) = n.as_i64() {
                PyInt::new(py, i).into_any()
            } else {
                PyFloat::new(py, n.as_f64().unwrap_or(f64::NAN)).into_any()
            }
        }
        serde_json::Value::String(s) => PyString::new(py, s).into_any(),
        serde_json::Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(value_to_py(py, item)?)?;
            }
            list.into_any()
        }
        serde_json::Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            dict.into_any()
        }
    })
}

/// Registry bookkeeping used by the skeleton until WP-P2b wires callbacks in.
pub type CallbackMap = HashMap<String, Py<PyAny>>;
