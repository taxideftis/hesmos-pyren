"""WP-P3b tests — the optional LLM judge (US-26, SS-22 judge부).

Three evidence layers, matching the ACs:
- AC1: a judged node's POST gate events carry judge.prompt_hash / judge.temperature /
  judge.model_version (W3-5 optional attrs), and the judge's tokens are folded into
  the turn's llm.call accounting (SS-17 rule 3 — every LLM call counts).
- AC2: drift is visible by attr comparison — re-registering a judge with a different
  prompt/temperature changes the recorded attrs of LATER sessions only (the registry
  snapshot happens at session OPEN).
- AC3: a session without a judge produces zero judge.* attrs and zero judge calls.
  A judge FAILURE (exception or unrecordable verdict) degrades to rules-only: the
  payload records judge_error, the gates carry no judge attrs, the session completes.
"""

import hashlib
import json
import os
import pathlib
import sqlite3
import subprocess
import uuid

import pytest

import hesmos
from hesmos import Budget, Plan, Session
from hesmos.eval_judge import JudgeConfig, VERDICT_SCORE, parse_verdict

pytestmark = pytest.mark.skipif(
    not hesmos.NATIVE_AVAILABLE, reason="native hesmos._ffi not built (run: maturin develop)"
)

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]

JUDGE_PROMPT = "Judge the output against the report rubric. Reply PASS, CONCERNS or FAIL."


def single_stage_yaml(model: str) -> str:
    """One node WITH a post rubric gate — the seat where judge attrs surface.

    `gates:` inside the profile is the contract-fragment spelling (compile.rs) for
    post gates; the rubric also always runs as a baseline post gate (runner.rs).
    """
    return (
        "name: judge-probe\npattern: graph\n"
        f"stages:\n  - id: research\n    profile: {{role: r, model: {model}, gates: [rubric.v1]}}\n"
        "    input_schema: research.v1\n    done_criteria: {items: [notes]}\n"
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


def judge_reply(content: str = "PASS — well formed", tokens_in: int = 7, tokens_out: int = 3) -> dict:
    return ok_reply(content=content, tokens_in=tokens_in, tokens_out=tokens_out)


def judge_config(**overrides) -> JudgeConfig:
    kwargs = {
        "model_ref": "glm-5.3-flash",
        "prompt": JUDGE_PROMPT,
        "temperature": 0.0,
        "model_version": "rubric-v3",
    }
    kwargs.update(overrides)
    return JudgeConfig(**kwargs)


@pytest.fixture()
def root(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    return tmp_path


@pytest.fixture(autouse=True)
def isolated_judge_registry():
    """Start AND end every test rules-only: the judge registry is process-global
    and the session path snapshots the "default" judge, so a leftover
    registration from another test would fake judge evidence (AC3 asserts its
    absence). clear_judge is the deterministic reset (FFI-1 ⑥-c)."""
    hesmos._ffi.clear_judge("default")
    yield
    hesmos._ffi.clear_judge("default")


def events(session_id: str) -> list[dict]:
    path = pathlib.Path.cwd() / ".hesmos" / "sessions" / session_id / "events.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def checkpoint_envelopes(session_id: str) -> dict[str, dict]:
    """node_id → committed envelope (parsed) from the WAL checkpoint db."""
    db = pathlib.Path.cwd() / ".hesmos" / "sessions" / session_id / "checkpoint.db"
    con = sqlite3.connect(db)
    try:
        rows = con.execute("SELECT node_id, envelope_json FROM checkpoints").fetchall()
    finally:
        con.close()
    return {node: json.loads(raw) for node, raw in rows}


def gate_attrs(event_list: list[dict]) -> list[dict]:
    """The attrs dicts of ALL gate.pass/gate.fail events (for absence assertions)."""
    return [e["attrs"] for e in event_list if e["kind"] in ("gate.pass", "gate.fail")]


def gate_attrs_by_phase(event_list: list[dict], node: str) -> tuple[list[dict], list[dict]]:
    """(pre_attrs, post_attrs) for one node — the phase split is the llm.call
    position: POST gates run AFTER the node's reply (where a judge record can
    exist), PRE gates before it (where judge attrs are FORBIDDEN — the judgment
    is about the OUTPUT, so a pre-gate carrying one would be a phase leak)."""
    pre: list[dict] = []
    post: list[dict] = []
    seen_llm = False
    for e in event_list:
        if e.get("node") != node:
            continue
        if e["kind"] == "llm.call":
            seen_llm = True
        elif e["kind"] in ("gate.pass", "gate.fail"):
            (post if seen_llm else pre).append(e["attrs"])
    return pre, post


# --- JudgeConfig + verdict vocabulary (the registration-time contract) ------


def test_judge_config_requires_fixed_temperature_and_identity():
    assert judge_config().prompt_hash == hashlib.sha256(JUDGE_PROMPT.encode("utf-8")).hexdigest()
    with pytest.raises(ValueError, match="temperature"):
        judge_config(temperature=None)  # None is NOT defaulted — it is refused
    with pytest.raises(ValueError, match="temperature"):
        judge_config(temperature="0.0")  # a string is not a sampling decision
    with pytest.raises(ValueError, match="prompt"):
        judge_config(prompt="   ")
    with pytest.raises(ValueError, match="model_version"):
        judge_config(model_version="")


def test_parse_verdict_accepts_only_the_three_level_vocabulary():
    assert parse_verdict("PASS — output complete") == "PASS"
    assert parse_verdict("concerns. section 2 lacks refs") == "CONCERNS"
    assert parse_verdict("FAIL: no deliverable") == "FAIL"
    assert VERDICT_SCORE == {"PASS": 1.0, "CONCERNS": 0.5, "FAIL": 0.0}
    with pytest.raises(ValueError, match="PASS, CONCERNS, FAIL"):
        parse_verdict("looks good to me")  # an unrecordable verdict refuses to guess


# --- AC1: judge meta recorded as gate-event attrs + token accounting --------


def test_ac1_judge_attrs_recorded_on_post_gate_events(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=42)
    plan = Plan.from_yaml(single_stage_yaml(model))

    @core.provider(model)
    def call_llm(req):
        return ok_reply()

    @core.judge(judge_config())
    def judge_llm(req):
        return judge_reply()

    receipt = core.run(plan, task="Draft a report")
    assert receipt.final_state == "Completed"
    core.close()

    config = judge_config()
    pre, post = gate_attrs_by_phase(events(receipt.session_id), "research")
    assert pre and post  # both phases ran — the split is observable
    for attrs in post:
        # EVERY post gate of the judged node records the same judgment (the
        # ledger entry is per-node, so rubric-baseline and rubric.v1 agree).
        assert attrs["judge.prompt_hash"] == config.prompt_hash
        assert attrs["judge.temperature"] == 0.0
        assert attrs["judge.model_version"] == "rubric-v3"
    for attrs in pre:
        assert not any(key.startswith("judge.") for key in attrs)  # phase split holds


def test_ac1_judge_tokens_folded_into_llm_call_accounting(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=9, budget=Budget(tokens=250_000))
    plan = Plan.from_yaml(single_stage_yaml(model))

    @core.provider(model)
    def call_llm(req):
        return ok_reply(tokens_in=100, tokens_out=20)

    @core.judge(judge_config())
    def judge_llm(req):
        return judge_reply(tokens_in=7, tokens_out=3)

    receipt = core.run(plan, task="t")
    spent = core.budget_status().session_spent  # before close (a closed session is FFI-STATE)
    core.close()

    llm_calls = [e for e in events(receipt.session_id) if e["kind"] == "llm.call"]
    assert len(llm_calls) == 1
    # ONE llm.call row per turn carrying agent + judge tokens (SS-17 rule 3):
    # the judge is an LLM call, so hiding it from the ledger would undercount.
    assert llm_calls[0]["attrs"]["tokens_in"] == 107
    assert llm_calls[0]["attrs"]["tokens_out"] == 23
    assert spent == 130  # ledger == events, judge included


def test_judge_bridge_builds_request_from_config_not_caller(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=3)
    plan = Plan.from_yaml(single_stage_yaml(model))
    seen = []

    @core.provider(model)
    def call_llm(req):
        return ok_reply(content="the draft body")

    @core.judge(judge_config())
    def judge_llm(req):
        seen.append(req.model_dump())
        return judge_reply()

    core.run(plan, task="t")
    core.close()

    assert len(seen) == 1  # the judge ran exactly once — after the node's reply
    request = seen[0]
    assert request["temperature"] == 0.0  # the CONFIG's sampling, byte-identical per eval
    assert request["max_tokens"] == 512
    assert request["messages"][0]["content"].endswith("OUTPUT:\nthe draft body")


# --- AC2: drift identifiable via attrs --------------------------------------


def test_ac2_reregistered_judge_changes_attrs_of_later_sessions_only(root):
    model_a = f"stub-{uuid.uuid4().hex[:12]}"
    model_b = f"stub-{uuid.uuid4().hex[:12]}"
    config_a = judge_config()  # temperature 0.0, prompt v3
    config_b = judge_config(temperature=0.7, prompt="Judge against rubric v4.")  # drift

    core_a = Session(seed=1)
    plan_a = Plan.from_yaml(single_stage_yaml(model_a))

    @core_a.provider(model_a)
    def call_llm_a(req):
        return ok_reply()

    @core_a.judge(config_a)
    def judge_a(req):
        return judge_reply()

    receipt_a = core_a.run(plan_a, task="t")
    core_a.close()

    # Registering config_b REPLACES the registration; the already-recorded session
    # A keeps its attrs (they were written at its gates), and only session B —
    # which snapshots the registry at OPEN — sees the new judge identity.
    core_b = Session(seed=1)
    plan_b = Plan.from_yaml(single_stage_yaml(model_b))

    @core_b.provider(model_b)
    def call_llm_b(req):
        return ok_reply()

    @core_b.judge(config_b)
    def judge_b(req):
        return judge_reply()

    receipt_b = core_b.run(plan_b, task="t")
    core_b.close()

    _, post_a = gate_attrs_by_phase(events(receipt_a.session_id), "research")
    assert {a["judge.temperature"] for a in post_a} == {0.0}
    assert {a["judge.prompt_hash"] for a in post_a} == {config_a.prompt_hash}
    _, post_b = gate_attrs_by_phase(events(receipt_b.session_id), "research")
    assert {a["judge.temperature"] for a in post_b} == {0.7}
    assert {a["judge.prompt_hash"] for a in post_b} == {config_b.prompt_hash}
    assert config_a.prompt_hash != config_b.prompt_hash  # the drift pair, comparable


# --- AC3: optional by contract; failure degrades to rules-only --------------


def test_ac3_no_judge_zero_attrs_and_zero_judge_calls(root):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=5)
    plan = Plan.from_yaml(single_stage_yaml(model))

    @core.provider(model)
    def call_llm(req):
        return ok_reply()

    receipt = core.run(plan, task="t")
    assert receipt.final_state == "Completed"
    core.close()

    assert gate_attrs(events(receipt.session_id))  # gates ran...
    for attrs in gate_attrs(events(receipt.session_id)):
        assert not any(key.startswith("judge.") for key in attrs)  # ...with zero judge evidence


@pytest.mark.parametrize("broken", ["raise", "unrecordable"])
def test_judge_failure_degrades_to_rules_only_session_completes(root, broken):
    model = f"stub-{uuid.uuid4().hex[:12]}"
    core = Session(seed=6)
    plan = Plan.from_yaml(single_stage_yaml(model))

    @core.provider(model)
    def call_llm(req):
        return ok_reply()

    @core.judge(judge_config())
    def judge_llm(req):
        if broken == "raise":
            raise RuntimeError("judge backend down")
        return judge_reply(content="looks good to me")  # not a recordable verdict

    receipt = core.run(plan, task="t")
    # Unlike a provider failure (HALT), the judge is OPTIONAL: the session's rules
    # verdict stands and the failure is EVIDENCE (payload judge_error), not a crash.
    assert receipt.final_state == "Completed"
    core.close()

    # No judge attrs anywhere — an unrecorded judgment must not look like a pass.
    for attrs in gate_attrs(events(receipt.session_id)):
        assert not any(key.startswith("judge.") for key in attrs)
    envelope = checkpoint_envelopes(receipt.session_id)["research"]
    assert "judge_error" in envelope["payload"]["json"]
    assert "judge" in envelope["payload"]["json"]["judge_error"].lower()


# --- S8 pairing: the suite file pins the current harness schema -------------


@pytest.fixture(scope="session")
def hesmos_bin():
    """The CLI binary — HESMOS_BIN overrides, else build once (same as T6 suite)."""
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


def test_suite_report_pins_harness_schema_and_unapproved_exit(hesmos_bin, root):
    """eval/suites/report.yaml must parse against the CURRENT harness
    (deny_unknown_fields: name + cases[{id, session, at?, budget?}]) and exit 3
    unapproved — the judge seat stays OUT of the schema until the harness grows
    its bridge (W6); this pin fails loudly if either side drifts silently."""
    suite = REPO_ROOT / "eval" / "suites" / "report.yaml"
    result = subprocess.run(
        [hesmos_bin, "eval", str(suite), "--json"],
        capture_output=True,
        text=True,
        cwd=pathlib.Path.cwd(),
        timeout=120,
    )
    assert result.returncode == 3, f"expected unapproved exit 3: {result.stdout}\n{result.stderr}"
    report = json.loads(result.stdout)
    assert report["suite"] == "report"
    assert {case["status"] for case in report["results"]} == {"error"}  # no golden → exit 3
