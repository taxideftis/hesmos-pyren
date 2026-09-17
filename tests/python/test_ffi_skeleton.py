"""FFI surface tests — the marshaling + lifecycle basics (T6; P0c skeleton, P2b shapes).

Requires the native extension (maturin develop); the pure-Python tests run without it.
Session lifecycle behavior (real runs, callbacks, replay) lives in test_session_run.py.
"""

import re

import pytest

import hesmos
from hesmos.exceptions import CompileError, FfiError

pytestmark = pytest.mark.skipif(
    not hesmos.NATIVE_AVAILABLE, reason="native hesmos._ffi not built (run: maturin develop)"
)

# The canonical §6.3-shaped plan: full contract surface (input_schema/done_criteria),
# task carried INSIDE the plan this time — the merge rule accepts both spellings.
PLAN_YAML = """\
name: report
task: Draft a competitive feature report
pattern: graph
stages:
  - id: research
    profile: {role: researcher, model: glm-5.3-flash, tools: [web.search, docs.read]}
    input_schema: research.v1
    done_criteria: {items: [notes]}
  - id: draft
    depends: [research]
    profile: {role: writer, model: glm-5.3-flash, tools: [write.file]}
    input_schema: draft.v1
    done_criteria: {items: [draft]}
  - id: verify
    depends: [draft]
    profile: {role: reviewer, model: glm-5.3-flash, gates: [rubric.v1]}
    input_schema: verify.v1
    done_criteria: {items: [verdict]}
"""  # no flow line: the CE-04 grammar is "a -> b, c" (one hop per statement) — the
# depends chains above already carry the graph; tools are runtime gate material and
# never consulted by the compile-only paths this suite exercises.

ULID_STRING = re.compile(r"^[0-7][0-9A-HJKMNP-TV-Z]{25}$")


@pytest.fixture()
def session_root(tmp_path, monkeypatch):
    """Session artifacts live under cwd (mission root); each test gets a fresh one."""
    monkeypatch.chdir(tmp_path)
    return tmp_path


def _open(plan_dict, task=None, seed=7):
    return hesmos._ffi.session_open(seed=seed, budget={"tokens": 1000}, team_id=None, plan=plan_dict, task=task)


def test_session_open_with_fixed_seed_returns_handle(session_root):
    # Deferred-open Session: the seed is visible before any run because the user
    # fixed it; the native open happens on first run() (see test_session_run.py).
    core = hesmos.Session(seed=42, budget=hesmos.Budget(tokens=250_000))
    assert core.seed == 42
    plan = hesmos.Plan.from_yaml(PLAN_YAML)
    receipt = core.run(plan, dry_run=True)
    assert core.seed == 42  # recorded, immutable
    assert receipt.session_id  # non-empty, core-minted
    core.close()


def test_session_open_generates_seed_when_absent(session_root):
    a = hesmos.Session()
    b = hesmos.Session()
    plan = hesmos.Plan.from_yaml(PLAN_YAML)
    ra = a.run(plan, dry_run=True)
    rb = b.run(plan, dry_run=True)
    assert isinstance(a.seed, int) and a.seed >= 0
    assert ra.session_id != rb.session_id  # unique sessions
    a.close()
    b.close()


def test_session_ids_are_canonical_ulid_strings(session_root):
    # P0a relay item 2: ULIDs cross the FFI boundary as 26-char Crockford strings —
    # never arrays or numbers — so canonical bytes stay stable across languages.
    core = hesmos.Session()
    plan = hesmos.Plan.from_yaml(PLAN_YAML)
    receipt = core.run(plan, dry_run=True)
    status = core.status()
    for value in (core.session_id, receipt.session_id, status.session_id, status.run_id):
        assert isinstance(value, str), f"id must be a string, got {type(value)}"
        assert ULID_STRING.fullmatch(value), f"non-canonical ULID: {value!r}"
    core.close()


def test_session_open_budget_violation_is_ffi_schema(session_root):
    # FFI-SCHEMA: boundary rejects before any core state is touched (US-01 AC2).
    plan = hesmos.Plan.from_yaml(PLAN_YAML)
    with pytest.raises(FfiError) as excinfo:
        hesmos._ffi.session_open(seed=1, budget={"tokens": "not-an-int"}, team_id=None, plan=plan._raw)
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
    # from_yaml is the CE-01 (schema-parse) level: task-less §6.3 plans must pass
    # here — full compile verdicts (CE-02..09) surface at session_open instead.
    with pytest.raises(CompileError) as excinfo:
        hesmos.Plan.from_yaml("stages:\n  - profile: {model: glm-5.3-flash}\n")
    assert excinfo.value.code == "CE-01"


def test_task_less_contract_plan_passes_from_yaml():
    # The §6.3 flagship shape: NO task field — the task is the run() argument.
    plan = hesmos.Plan.from_yaml(
        "name: p\npattern: graph\n"
        "stages:\n  - id: a\n    profile: {role: r, model: glm-5.3-flash}\n"
        "    input_schema: s.v1\n"
    )
    assert "task" not in plan._raw


def test_session_close_round_trip(session_root):
    plan = hesmos.Plan.from_yaml(PLAN_YAML)
    handle = _open(plan._raw)
    assert hesmos._ffi.session_close(handle) is None
    assert hesmos._ffi.session_close(handle) is None  # idempotent
    with pytest.raises(FfiError) as excinfo:
        hesmos._ffi.session_close({"no_session_id": True})
    assert excinfo.value.code == "FFI-SCHEMA"
