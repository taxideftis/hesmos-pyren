"""WP-P2b tests — session_run E2E, callback contract, receipts, replay (T6, SS-16/17).

The native session runs the FULL runner path (compile → waves → gates → callbacks →
WAL → seal) with Python callbacks dispatched through the global registry. Live-plan
identity, budget reconciliation (US-20 AC3), and the §6.3 CLI replay verify live here.
"""

import json
import os
import pathlib
import subprocess
import uuid

import pytest

import hesmos
from hesmos import Budget, Plan, Session
from hesmos.exceptions import FfiError

pytestmark = pytest.mark.skipif(
    not hesmos.NATIVE_AVAILABLE, reason="native hesmos._ffi not built (run: maturin develop)"
)

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]

PLAN_YAML = """\
name: s16-pyrun
pattern: graph
stages:
  - id: research
    profile: {role: researcher, model: __MODEL__}
    input_schema: research.v1
    done_criteria: {items: [notes]}
  - id: draft
    depends: [research]
    profile: {role: writer, model: __MODEL__}
    input_schema: draft.v1
    done_criteria: {items: [draft]}
  - id: verify
    depends: [draft]
    profile: {role: reviewer, model: __MODEL__, gates: [rubric.v1]}
    input_schema: verify.v1
    done_criteria: {items: [verdict]}
"""  # §6.3 shape: no task field (the run() argument is the goal_original source);
# no tool allowlists — the permission gate (SS-19) bounds a node's tools by its
# GRANTOR's profile, and this suite exercises the runner path, not tool grants.

PLAN_WITH_TASK_YAML = (
    "name: conflict\ntask: the plan's own goal\npattern: graph\n"
    "stages:\n  - id: a\n    profile: {role: r, model: m}\n    input_schema: s.v1\n"
)


def make_plan(model: str | None = None) -> Plan:
    ref = model or f"stub-{uuid.uuid4().hex[:12]}"
    return Plan.from_yaml(PLAN_YAML.replace("__MODEL__", ref))


def _single_stage_yaml(model: str) -> str:
    """One node, one wave — for measuring per-node attempt counts exactly."""
    return (
        "name: retry-probe\npattern: graph\n"
        f"stages:\n  - id: a\n    profile: {{role: r, model: {model}}}\n"
        "    input_schema: s.v1\n    done_criteria: {items: [x]}\n"
    )


def ok_reply(content: str = "done", tokens_in: int = 100, tokens_out: int = 20) -> dict:
    """A full LlmReply-shaped dict — the ONLY reply shape the boundary accepts."""
    return {
        "content": content,
        "tool_calls": [],
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "provider_meta": {"provider": "stub"},
    }


@pytest.fixture()
def root(tmp_path, monkeypatch):
    """Fresh mission root per test: session artifacts land under cwd."""
    monkeypatch.chdir(tmp_path)
    return tmp_path


def events_path(session_id: str) -> pathlib.Path:
    return pathlib.Path.cwd() / ".hesmos" / "sessions" / session_id / "events.jsonl"


# --- PY-5 dry_run (schedule verification, zero node executions) ---


def test_dry_run_returns_schedule_receipt(root):
    core = Session(seed=42, budget=Budget(tokens=250_000))
    receipt = core.run(make_plan(), task="Draft a competitive feature report", dry_run=True)
    assert receipt.dry_run is True
    assert receipt.commits == 0
    assert receipt.final_state == "Running"
    assert receipt.waves == [["research"], ["draft"], ["verify"]]
    core.close()


def test_receipt_session_id_and_trace_id_alias_equal(root):
    core = Session()
    receipt = core.run(make_plan(), task="t", dry_run=True)
    # PY-5 note: session_id 1급 / trace_id §6.3 alias — always the same value.
    assert receipt.trace_id == receipt.session_id
    assert receipt.trace_id == core.session_id
    core.close()


def test_session_status_matches_type5_mirror(root):
    core = Session(seed=7, budget=Budget(tokens=1234))
    core.run(make_plan(), task="t", dry_run=True)
    status = core.status()
    # Dual validation: the FFI dict must satisfy the generated mirror (frozen,
    # extra=forbid) — a shape drift fails right here.
    assert status.seed == 7
    assert status.state == "Running"
    # Consensus (a): plan_hash is REQUIRED from plan.compiled on — never omitted.
    assert status.plan_hash and len(status.plan_hash) == 64
    assert status.budget.session_max_tokens == 1234
    core.close()


def test_budget_status_shape_unbounded_pre_run(root):
    core = Session()  # no budget → unbounded envelope (frozen defaults)
    core.run(make_plan(), task="t", dry_run=True)
    state = core.budget_status()
    assert state.session_spent == 0
    assert state.session_suspend_limit is None  # uncapped line cannot fire
    core.close()


# --- PY-3 real runs through the callback registry ---


def test_completed_run_with_stub_provider(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    calls: list[dict] = []

    core = Session(seed=42, budget=Budget(tokens=250_000))
    plan = make_plan(model)

    @core.provider(model)
    def call_llm(req: hesmos.LlmRequest) -> dict:
        calls.append(req.model_dump())
        return ok_reply(f"done for {req.messages[-1].content[:10]}")

    receipt = core.run(plan, task="Draft a competitive feature report")
    assert receipt.final_state == "Completed"
    assert receipt.reason is None
    assert receipt.commits == 3
    assert receipt.dry_run is False
    assert receipt.chain_head and len(receipt.chain_head) == 64
    # Every node consulted OUR callback, exactly once (no retry needed).
    assert len(calls) == 3
    request = calls[0]
    assert request["temperature"] is None  # profile leaves it unset
    assert request["cache_breakpoints"] == {"stable": 0, "context": 0, "volatile": 1}
    assert request["messages"][0]["role"] == "user"
    assert "Draft a competitive feature report" in request["messages"][0]["content"]
    core.close()


def test_dry_run_then_real_run_same_session(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=1)
    plan = make_plan(model)

    @core.provider(model)
    def call_llm(req):
        return ok_reply()

    dry = core.run(plan, task="t", dry_run=True)
    real = core.run(plan, task="t")
    assert dry.session_id == real.session_id  # SAME session, no re-open
    assert real.final_state == "Completed"
    core.close()


def test_ac3_ledger_reconciles_llm_call_sums(root):
    # US-20 AC3: ledger sums == llm.call event sums, read from the artifacts.
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=5, budget=Budget(tokens=250_000))
    plan = make_plan(model)

    @core.provider(model)
    def call_llm(req):
        return ok_reply(tokens_in=100, tokens_out=20)

    receipt = core.run(plan, task="t")
    state = core.budget_status()
    assert state.session_spent == 3 * 120  # ledger: 3 nodes × (100+20)

    events = [
        json.loads(line)
        for line in events_path(receipt.session_id).read_text().splitlines()
        if line.strip()
    ]
    llm_calls = [e for e in events if e["kind"] == "llm.call"]
    assert len(llm_calls) == 3
    event_sum = sum(e["attrs"]["tokens_in"] + e["attrs"]["tokens_out"] for e in llm_calls)
    assert event_sum == state.session_spent  # the reconciliation itself
    core.close()


def test_completed_session_seals_trace(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=2)
    plan = make_plan(model)

    @core.provider(model)
    def call_llm(req):
        return ok_reply()

    receipt = core.run(plan, task="t")
    events = [
        json.loads(line)
        for line in events_path(receipt.session_id).read_text().splitlines()
        if line.strip()
    ]
    assert events[-1]["kind"] == "trace.seal"  # COMPLETED →trace seal→ 증거 성립
    core.close()


# --- failure paths: evidence-preserving, mechanically reported ---


def test_unregistered_model_fails_with_provider_failure_receipt(root):
    plan = make_plan("never-registered-model")
    core = Session(seed=3)
    receipt = core.run(plan, task="t")
    # The run's control flow completed — the failure is IN the receipt (SS-16 rule 2:
    # unregistered model_ref = provider failure). Terminal is HALTED (resume = fork),
    # not FAILED: gates fail sessions, provider/loop-guard failures halt them.
    assert receipt.final_state == "Halted"
    assert receipt.reason == "PROVIDER_FAILURE"
    core.close()


def test_provider_exception_surfaces_mechanically(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=4)
    plan = make_plan(model)

    @core.provider(model)
    def broken(req):
        raise ValueError("provider exploded")

    receipt = core.run(plan, task="t")
    assert receipt.final_state == "Halted"
    assert receipt.reason == "PROVIDER_FAILURE"
    core.close()
    # Mechanical facts (class name + message) survive into the failure record —
    # consensus (b): the wrapper reports what happened, the adapter maps the code.
    events = [
        json.loads(line)
        for line in events_path(receipt.session_id).read_text().splitlines()
        if line.strip()
    ]
    aborts = [
        e
        for e in events
        if e["kind"] == "session.close" and e["attrs"].get("final_state") == "HALTED"
    ]
    assert aborts, "session.close must record the failed terminal"


def test_callback_signature_mismatch_is_mechanical_failure(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=6)
    plan = make_plan(model)

    @core.provider(model)
    def wrong_arity(req, extra):  # contract: exactly one argument
        return ok_reply()

    receipt = core.run(plan, task="t")
    assert receipt.final_state == "Halted"
    assert receipt.reason == "PROVIDER_FAILURE"
    core.close()


def test_missing_reply_tokens_fails_session(root):
    # SS-17 rule 3: LlmReply.tokens_in/out are the llm.call accounting source —
    # a reply without them must fail the session, never zero-fill.
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=8)
    plan = make_plan(model)

    @core.provider(model)
    def no_tokens(req):
        return {"content": "ok", "tool_calls": [], "provider_meta": {}}

    receipt = core.run(plan, task="t")
    assert receipt.final_state == "Halted"
    assert receipt.reason == "PROVIDER_FAILURE"
    core.close()


def test_transient_provider_failure_retries_then_completes(root):
    # exceptions.md §4: the CORE owns bounded retry — a transient callback failure
    # re-enters the attempt loop and the node completes. The retry count is the
    # runner's (policy.bounded_retry); this measures it through the FFI path on a
    # single-node plan so the invocation count is unambiguous.
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=33)
    plan = Plan.from_yaml(_single_stage_yaml(model))
    calls = {"n": 0}

    @core.provider(model)
    def flaky(req):
        calls["n"] += 1
        if calls["n"] == 1:
            raise ValueError("first attempt blows up")
        return ok_reply()

    receipt = core.run(plan, task="t")
    assert receipt.final_state == "Completed"
    assert calls["n"] == 2  # 1 initial attempt + 1 retry
    core.close()


def test_provider_failure_exhausts_bounded_retry_then_halts(root):
    # Exhaustion: policy default bounded_retry = 2 → at most 1 initial + 2 retries,
    # then the session halts with PROVIDER_FAILURE (never an infinite loop).
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=34)
    plan = Plan.from_yaml(_single_stage_yaml(model))
    calls = {"n": 0}

    @core.provider(model)
    def always_fails(req):
        calls["n"] += 1
        raise ValueError("permanent outage")

    receipt = core.run(plan, task="t")
    assert receipt.final_state == "Halted"
    assert receipt.reason == "PROVIDER_FAILURE"
    assert calls["n"] == 3
    core.close()


# --- boundary rejections (FFI-*) ---


def test_full_compile_verdicts_surface_at_open(root):
    # CE-02 at session_open: from_yaml is pre-compile (§6.3 task-less plan must pass
    # it), so the full compiler runs after the task merge — and leaves NO session.
    from hesmos.exceptions import CompileError

    ghost = Plan.from_yaml(
        "name: bad\npattern: graph\n"
        "stages:\n  - id: a\n    profile: {role: r, model: m}\n    input_schema: s.v1\n"
        "    done_criteria: {items: [x]}\n"
        "    depends: [ghost]\n"
    )
    core = Session(seed=21)
    with pytest.raises(CompileError) as excinfo:
        core.run(ghost, task="t")
    assert excinfo.value.code == "CE-02"
    core.close()
    assert not (root / ".hesmos").exists()  # no artifacts behind (US-04 AC2)


def test_run_twice_rejected_ffi_state(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=9)
    plan = make_plan(model)

    @core.provider(model)
    def call_llm(req):
        return ok_reply()

    assert core.run(plan, task="t").final_state == "Completed"
    with pytest.raises(FfiError) as excinfo:
        core.run(plan, task="t")
    assert excinfo.value.code == "FFI-STATE"
    core.close()


def test_run_with_different_plan_rejected_ffi_state(root):
    core = Session(seed=10)
    opened = make_plan()
    other = make_plan()
    core.run(opened, task="t", dry_run=True)
    with pytest.raises(FfiError) as excinfo:
        core.run(other, task="t", dry_run=True)
    assert excinfo.value.code == "FFI-STATE"
    core.close()


def test_run_with_different_task_rejected_ffi_state(root):
    core = Session(seed=11)
    plan = make_plan()
    core.run(plan, task="first goal", dry_run=True)
    with pytest.raises(FfiError) as excinfo:
        core.run(plan, task="a different goal", dry_run=True)
    assert excinfo.value.code == "FFI-STATE"
    core.close()


def test_plan_task_conflict_is_ffi_schema(root):
    plan_with_task = Plan.from_yaml(PLAN_WITH_TASK_YAML)  # carries its own task
    core = Session(seed=12)
    with pytest.raises(FfiError) as excinfo:
        core.run(plan_with_task, task="Draft a competitive feature report X")
    assert excinfo.value.code == "FFI-SCHEMA"
    core.close()


def test_cost_usd_rejected_ffi_schema(root):
    plan = make_plan()
    core = Session(seed=13, budget=None)
    with pytest.raises(FfiError) as excinfo:
        import hesmos._ffi as ffi

        ffi.session_open(seed=1, budget={"tokens": 10, "cost_usd": 0.5}, team_id=None, plan=plan._raw, task="t")
    assert excinfo.value.code == "FFI-SCHEMA"
    core.close()


def test_duplicate_provider_registration_is_ffi_contract(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=14)

    @core.provider(model)
    def first(req):
        return ok_reply()

    with pytest.raises(FfiError) as excinfo:
        _second = core.provider(model)(first)
    assert excinfo.value.code == "FFI-CONTRACT"
    core.close()


def test_non_callable_provider_is_ffi_contract(root):
    core = Session(seed=15)
    with pytest.raises(FfiError) as excinfo:
        core.provider("not-callable-ref")("just a string")
    assert excinfo.value.code == "FFI-CONTRACT"
    core.close()


def test_close_then_run_is_ffi_state(root):
    core = Session(seed=16)
    plan = make_plan()
    core.run(plan, task="t", dry_run=True)
    core.close()
    with pytest.raises(FfiError) as excinfo:
        core.run(plan, task="t", dry_run=True)
    assert excinfo.value.code == "FFI-STATE"


def test_build_messages_returns_valid_message(root):
    core = Session(seed=17)
    plan = make_plan()
    core.run(plan, task="the session goal", dry_run=True)
    messages = core.build_messages({"id": "research"}, {"node_input": {"k": "v"}})
    assert len(messages) == 1
    assert messages[0].role == "user"
    assert "the session goal" in messages[0].content
    core.close()


# --- session-start model validate (PORT-2, SS-17 — exceptions.md §5) ---


def _write_bathos_stub(root: pathlib.Path, body: str) -> pathlib.Path:
    """A stub `bathos` executable (p1e pattern): answers `model validate` only."""
    stub = root / "bathos-stub"
    stub.write_text("#!/bin/bash\n" + body)
    stub.chmod(0o755)
    return stub


def test_session_start_model_validate_passes(root, monkeypatch):
    stub = _write_bathos_stub(
        root, 'if [ "$1 $2" = "model validate" ]; then echo \'{"ok":true}\'; exit 0; fi\nexit 3\n'
    )
    monkeypatch.setenv("HESMOS_BATHOS", str(stub))
    core = Session(seed=30)
    receipt = core.run(make_plan(), task="t", dry_run=True)
    assert receipt.dry_run is True
    core.close()


def test_e_model_mix_rejects_session_start(root, monkeypatch):
    # A non-zero exit from `model validate` IS the E-MODEL-MIX verdict — the code
    # crosses VERBATIM (exceptions.md §5: never converted to a Hesmos code).
    stub = _write_bathos_stub(
        root,
        'if [ "$1 $2" = "model validate" ]; then echo \'{"runtime":"openai"}\'; exit 2; fi\nexit 3\n',
    )
    monkeypatch.setenv("HESMOS_BATHOS", str(stub))
    core = Session(seed=31)
    with pytest.raises(hesmos.HesmosError) as excinfo:
        core.run(make_plan(), task="t", dry_run=True)
    assert excinfo.value.code == "E-MODEL-MIX"
    core.close()
    assert not (root / ".hesmos").exists()  # rejected start leaves no artifacts


def test_absent_bathos_engine_proceeds_unverified(root, monkeypatch):
    # No engine = no verdict. Substituting our own mix check would be the forbidden
    # re-implementation (SS-17 rule 2), so an absent engine is a degraded open.
    monkeypatch.setenv("HESMOS_BATHOS", str(root / "no-such-engine"))
    core = Session(seed=32)
    receipt = core.run(make_plan(), task="t", dry_run=True)
    assert receipt.dry_run is True
    core.close()


# --- S3 joint (SS-18 Python side): frozen system prompt, per-turn core verification ---
# The VIOLATION half of the pair (tamper → immediate halt, zero retries, exit 20) is
# structurally unreachable from the Python contract surface — the prompt is frozen at
# open and the builder is sealed, so a mid-session change has no legal path; that
# impossibility IS the invariant. The tamper scenario runs through the runner seam in
# the Rust pair test (P2a: tests/integration_rust t12_cache_violation_halts_immediately_
# zero_retries_exit20) — this suite owns the frozen-STABLE end-to-end path.


PROMPT = "You are a careful analyst. Answer only from the given snapshot."


def test_s3_frozen_prompt_stable_across_turns(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=35, system_prompt=PROMPT)
    plan = make_plan(model)
    seen: list[dict] = []

    @core.provider(model)
    def call_llm(req: hesmos.LlmRequest) -> dict:
        seen.append(req.model_dump())
        return ok_reply()

    receipt = core.run(plan, task="t")
    assert receipt.final_state == "Completed"
    assert len(seen) == 3
    for request in seen:
        # Every turn carries the byte-identical stable layer first (SS-18 rule 1)...
        assert request["messages"][0]["role"] == "system"
        assert request["messages"][0]["content"] == PROMPT
        assert request["messages"][1]["role"] == "user"
        # ...and the builder-equivalent placement: stable=1, context=0 (no history
        # in the single-turn shape), volatile = the uncached tail end.
        assert request["cache_breakpoints"] == {"stable": 1, "context": 0, "volatile": 2}
    core.close()


def test_s3_unfrozen_session_has_no_stable_layer(root):
    # Model-neutral default: no system_prompt → no invariant frozen, the request
    # stays a single uncached user message (the W5 echo world cache.rs models).
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=36)
    plan = make_plan(model)
    seen: list[dict] = []

    @core.provider(model)
    def call_llm(req: hesmos.LlmRequest) -> dict:
        seen.append(req.model_dump())
        return ok_reply()

    receipt = core.run(plan, task="t")
    assert receipt.final_state == "Completed"
    for request in seen:
        assert request["messages"][0]["role"] == "user"
        assert request["cache_breakpoints"] == {"stable": 0, "context": 0, "volatile": 1}
    core.close()


# --- §6.3 E2E: receipt.trace_id → hesmos trace replay (PY-5 verify, US-19 AC2) ---


@pytest.fixture(scope="session")
def hesmos_bin():
    """The CLI binary for the replay leg — HESMOS_BIN overrides, else build once."""
    override = os.environ.get("HESMOS_BIN")
    if override:
        return override
    subprocess.run(
        ["cargo", "build", "-q", "-p", "hesmos"],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
    )
    return str(REPO_ROOT / "target" / "debug" / "hesmos")


def test_e2e_replay_from_receipt_trace_id(root, hesmos_bin):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=42, budget=Budget(tokens=250_000))
    plan = make_plan(model)

    @core.provider(model)
    def call_llm(req):
        return ok_reply()

    receipt = core.run(plan, task="Draft a competitive feature report")
    assert receipt.final_state == "Completed"
    core.close()

    # Fork at the LAST commit: every node reuses the origin's cached responses
    # byte-for-byte — no LLM calls — and the CLI verifies the origin chain.
    result = subprocess.run(
        [hesmos_bin, "trace", "replay", receipt.trace_id, "--at", "3"],
        capture_output=True,
        text=True,
        cwd=pathlib.Path.cwd(),
        timeout=120,
    )
    assert result.returncode == 0, f"replay failed: {result.stdout}\n{result.stderr}"
    # The fork mints a NEW session id with fork_of lineage back to the origin.
    assert "fork_of" in result.stdout
    assert receipt.trace_id in result.stdout or receipt.session_id in result.stdout
