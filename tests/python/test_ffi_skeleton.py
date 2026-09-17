"""FFI skeleton tests — surface ①②⑨ marshaling (T6, WP-P0c scope).

Requires the native extension (maturin develop); the pure-Python tests run without it.
"""

import pytest

import hesmos
from hesmos.exceptions import CompileError, FfiError

pytestmark = pytest.mark.skipif(
    not hesmos.NATIVE_AVAILABLE, reason="native hesmos._ffi not built (run: maturin develop)"
)

PLAN_YAML = """\
name: report
task: Draft a competitive feature report
stages:
  - id: research
    profile: {model: glm-5.3-flash, tools: [web.search, docs.read]}
  - id: draft
    depends: [research]
    profile: {model: glm-5.3-flash, tools: [write.file]}
  - id: verify
    depends: [draft]
    profile: {model: glm-5.3-flash, gates: [rubric.v1]}
"""


def test_session_open_with_fixed_seed_returns_handle():
    core = hesmos.Session(seed=42, budget=hesmos.Budget(tokens=250_000))
    assert core.seed == 42
    assert core.session_id  # non-empty; core-generated and recorded (PY-1 notes)
    core.close()


def test_session_open_generates_seed_when_absent():
    a = hesmos.Session()
    b = hesmos.Session()
    assert isinstance(a.seed, int) and a.seed >= 0
    assert a.session_id != b.session_id  # unique sessions
    a.close()
    b.close()


def test_session_ids_are_canonical_ulid_strings():
    # P0a relay item 2: ULIDs cross the FFI boundary as 26-char Crockford strings —
    # never arrays or numbers — so canonical bytes stay stable across languages.
    core = hesmos.Session()
    handle = core._handle
    import re

    ulid_string = re.compile(r"^[0-7][0-9A-HJKMNP-TV-Z]{25}$")
    for value in (core.session_id, handle["session_id"], handle["run_id"]):
        assert isinstance(value, str), f"id must be a string, got {type(value)}"
        assert ulid_string.fullmatch(value), f"non-canonical ULID: {value!r}"
    core.close()


def test_session_open_budget_violation_is_ffi_schema():
    # FFI-SCHEMA: boundary rejects before any core state is touched (US-01 AC2).
    with pytest.raises(FfiError) as excinfo:
        hesmos._ffi.session_open(seed=1, budget={"tokens": "not-an-int"}, team_id=None)
    assert excinfo.value.code == "FFI-SCHEMA"


def test_plan_from_yaml_accepts_contract_example():
    plan = hesmos.Plan.from_yaml(PLAN_YAML)
    assert repr(plan).startswith("Plan(stages=3")


def test_plan_from_yaml_invalid_yaml_is_ce01():
    with pytest.raises(CompileError) as excinfo:
        hesmos.Plan.from_yaml("stages: [unclosed")
    assert excinfo.value.code == "CE-01"
    assert excinfo.value.error_class == "Compile"


def test_plan_from_yaml_missing_stage_id_is_ce01():
    with pytest.raises(CompileError) as excinfo:
        hesmos.Plan.from_yaml("stages:\n  - profile: {model: glm-5.3-flash}\n")
    assert excinfo.value.code == "CE-01"


def test_session_close_round_trip():
    handle = hesmos._ffi.session_open(seed=7, budget={"tokens": 1000}, team_id=None)
    assert hesmos._ffi.session_close(handle) is None
    with pytest.raises(FfiError) as excinfo:
        hesmos._ffi.session_close({"no_session_id": True})
    assert excinfo.value.code == "FFI-SCHEMA"
