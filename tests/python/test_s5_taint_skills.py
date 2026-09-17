"""WP-P2e tests — S5 taint isolation, persistence blocking, skill gating (SS-20/21).

Three layers under test, each with its own evidence:
- hesmos.memory: the persist-time taint filter (store residual stays 0).
- hesmos.skills: declarative-only loading + the HMAC signature gate.
- the FFI executor dispatch: tool.call events (ok=true/false), the SS-20 rule 1
  unmarked-external rejection, and envelope marking observable in the WAL
  checkpoint rows (the committed envelope_json carries the core's taint).
"""

import json
import os
import pathlib
import sqlite3
import sys
import uuid

import pytest

import hesmos
from hesmos import Budget, Session
from hesmos.memory import MemoryStore
from hesmos.skills import load, load_signed, sign_manifest
from hesmos.tools import McpStdioClient, Toolbox

pytestmark = pytest.mark.skipif(
    not hesmos.NATIVE_AVAILABLE, reason="native hesmos._ffi not built (run: maturin develop)"
)


@pytest.fixture()
def root(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    return tmp_path


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


def single_stage_yaml(model: str, tools: list[str] | None = None) -> str:
    tool_part = f", tools: [{', '.join(tools)}]" if tools else ""
    return (
        "name: s5-probe\npattern: graph\n"
        f"stages:\n  - id: research\n    profile: {{role: r, model: {model}{tool_part}}}\n"
        "    input_schema: research.v1\n    done_criteria: {items: [notes]}\n"
    )


def two_stage_yaml(model: str, grantor_tools: list[str], drafter_tools: list[str]) -> str:
    return (
        "name: s5-chain\npattern: graph\n"
        "stages:\n"
        f"  - id: research\n    profile: {{role: researcher, model: {model}, tools: [{', '.join(grantor_tools)}]}}\n"
        "    input_schema: research.v1\n    done_criteria: {items: [notes]}\n"
        "  - id: draft\n    depends: [research]\n"
        f"    profile: {{role: writer, model: {model}, tools: [{', '.join(drafter_tools)}]}}\n"
        "    input_schema: draft.v1\n    done_criteria: {items: [draft]}\n"
    )


# --- memory: the persist-time taint filter (SS-20 rule 2·3) ----------------


def test_persist_refuses_tainted_entry_store_residual_zero(tmp_path):
    store = MemoryStore(tmp_path / "memory.jsonl")
    record = store.persist(
        {
            "id": "doc-1",
            "content": "external document body",
            "taint": {"kind": "Tainted", "source": {"origin": "web.fetch"}},
        }
    )
    assert record == {"ok": False, "reason": "tainted", "origin": "web.fetch"}
    assert store.count() == 0  # AC①: the store keeps ZERO residual


def test_persist_accepts_clean_entry(tmp_path):
    store = MemoryStore(tmp_path / "memory.jsonl")
    record = store.persist({"id": "note-1", "content": "local", "taint": {"kind": "Clean"}})
    assert record == {"ok": True}
    assert store.count() == 1
    assert store.entries()[0]["id"] == "note-1"


def test_persist_refuses_unmarked_entry(tmp_path):
    store = MemoryStore(tmp_path / "memory.jsonl")
    # PT-12: an absent mark is not a Clean bill — the filter refuses to guess.
    record = store.persist({"id": "raw", "content": "no taint field"})
    assert record == {"ok": False, "reason": "unmarked"}
    assert store.count() == 0


def test_persist_refuses_every_tainted_derivation(tmp_path):
    """AC③ (Python pair of the core P2a derive test): anything derived from a
    tainted read carries the marking — every derivative is refused, residual 0."""
    store = MemoryStore(tmp_path / "memory.jsonl")
    external_read = {
        "id": "doc",
        "content": "external",
        "taint": {"kind": "Tainted", "source": {"origin": "mcp:docs"}},
    }
    # The summary an agent writes FROM the external doc is derived data — the
    # derivation carries the taint (core Envelope::derive rule, consumed here).
    summary = {**external_read, "id": "summary", "content": "summary of external"}
    quoted = {**external_read, "id": "quote", "content": external_read["content"]}
    for entry in (external_read, summary, quoted):
        assert store.persist(entry)["ok"] is False
    assert store.count() == 0


# --- skills: declarative loading + the signature gate (SS-21) --------------


def _write_skill(directory: pathlib.Path, manifest: dict) -> pathlib.Path:
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "manifest.json"
    path.write_text(json.dumps(manifest))
    return path


DECLARATIVE = {
    "name": "doc-review",
    "description": "Review a document against its style guide",
    "instructions": ["Read the document", "List violations", "Propose fixes"],
    "version": "1.0.0",
}


def test_declarative_manifest_loads_with_zero_code_execution(tmp_path):
    path = _write_skill(tmp_path / "skill", DECLARATIVE)
    record = load(path)
    assert record["ok"] is True
    assert record["manifest"]["instructions"] == DECLARATIVE["instructions"]


def test_manifest_with_executable_content_is_rejected(tmp_path):
    # Allowlist, not denylist: any non-declarative key is a rejection.
    rogue = {**DECLARATIVE, "script": "import os; os.system('echo pwned')"}
    record = load(_write_skill(tmp_path / "rogue", rogue))
    assert record["ok"] is False
    assert record["reason"] == "executable_content"
    assert "script" in record["detail"]


def test_manifest_missing_required_keys_is_rejected(tmp_path):
    record = load(_write_skill(tmp_path / "thin", {"name": "x", "description": "y"}))
    assert record["ok"] is False
    assert record["reason"] == "incomplete"


def test_signed_package_loads_only_after_verification(tmp_path):
    key = os.urandom(32).hex()
    pkg = tmp_path / "signed"
    raw = _write_skill(pkg, DECLARATIVE).read_bytes()
    (pkg / "manifest.sig").write_text(sign_manifest(raw, key))
    record = load_signed(pkg, key=key)
    assert record["ok"] is True and record["signed"] is True


def test_tampered_package_is_rejected(tmp_path):
    """AC⑤'s gate: the signature is verified BEFORE the content is released."""
    key = os.urandom(32).hex()
    pkg = tmp_path / "tampered"
    raw = _write_skill(pkg, DECLARATIVE).read_bytes()
    (pkg / "manifest.sig").write_text(sign_manifest(raw, key))
    _write_skill(pkg, {**DECLARATIVE, "description": "tampered description"})
    record = load_signed(pkg, key=key)
    assert record["ok"] is False
    assert record["reason"] == "signature_mismatch"


def test_signature_without_key_material_is_rejected(tmp_path, monkeypatch):
    monkeypatch.delenv("HESMOS_SKILL_SIGNING_KEY", raising=False)
    pkg = tmp_path / "unsigned"
    _write_skill(pkg, DECLARATIVE)
    record = load_signed(pkg)  # no key arg, no env — refused, never guessed
    assert record["ok"] is False
    assert record["reason"] == "no_signing_key"


# --- executor dispatch: tool.call events + the SS-20 boundary ---------------


def _register(core, name, external, fn):
    core.tool(name, external=external)(fn)


def _session_with_provider(core, reply_builder, calls):
    model = f"stub-{uuid.uuid4().hex[:12]}"

    @core.provider(model)
    def call_llm(req):
        calls.append(req.model_dump())
        return reply_builder(req)

    return model


def ok_reply(content="done", tool_calls=None):
    return {
        "content": content,
        "tool_calls": tool_calls or [],
        "tokens_in": 100,
        "tokens_out": 20,
        "provider_meta": {"provider": "stub"},
    }


def test_dispatch_records_tool_call_ok_true_and_payload(root):
    calls: list = []
    core = Session(seed=42, budget=Budget(tokens=250_000))
    try:
        model = _session_with_provider(
            core,
            lambda req: ok_reply(
                "calling tool",
                tool_calls=[
                    {"id": "c1", "name": "local.note", "arguments": {"key": "k", "value": "v"}}
                ],
            ),
            calls,
        )
        notes: dict = {}

        @core.tool("local.note")
        def local_note(call):
            notes[call.arguments["key"]] = call.arguments["value"]
            return {"call_id": call.id, "content": "stored", "taint": {"kind": "Clean"}}

        from hesmos import Plan

        receipt = core.run(
            Plan.from_yaml(single_stage_yaml(model, tools=["local.note"])), task="take a note"
        )
        assert receipt.final_state == "Completed"
        assert notes == {"k": "v"}  # the callback RAN with the model's arguments
        tool_events = [e for e in events(core.session_id) if e["kind"] == "tool.call"]
        assert len(tool_events) == 1
        attrs = tool_events[0]["attrs"]
        assert attrs["tool_name"] == "local.note"
        assert attrs["ok"] is True
        assert attrs["actor"] == "executor"
        # The result (clean, local) never taints the committed envelope.
        envelope = checkpoint_envelopes(core.session_id)["research"]
        assert envelope["taint"] == "clean"
        assert envelope["payload"]["json"]["tool_results"][0]["content"] == "stored"
    finally:
        core.close()


def test_unregistered_tool_is_recorded_not_fatal(root):
    """기각은 세션 실패가 아니다: a dispatch to an unregistered tool is recorded
    (tool.call ok=false + a reason in the payload) and the session completes."""
    calls: list = []
    core = Session(seed=42, budget=Budget(tokens=250_000))
    try:
        model = _session_with_provider(
            core,
            lambda req: ok_reply(
                "try the tool",
                tool_calls=[{"id": "c1", "name": "ghost.tool", "arguments": {}}],
            ),
            calls,
        )
        from hesmos import Plan

        receipt = core.run(Plan.from_yaml(single_stage_yaml(model)), task="t")
        assert receipt.final_state == "Completed"
        tool_events = [e for e in events(core.session_id) if e["kind"] == "tool.call"]
        assert tool_events[0]["attrs"]["ok"] is False
        assert tool_events[0]["attrs"]["tool_name"] == "ghost.tool"
        envelope = checkpoint_envelopes(core.session_id)["research"]
        result = envelope["payload"]["json"]["tool_results"][0]
        assert result["ok"] is False
        assert "no callback registered" in result["reason"]
    finally:
        core.close()


def test_external_tool_returning_clean_is_rejected_as_unmarked_import(root):
    """SS-20 rule 1 at the Python boundary: the tool DECLARED external at
    registration, so a Clean result is an unmarked import — the executor
    consumes core's external_import_verdict and rejects the RESULT."""
    calls: list = []
    core = Session(seed=42, budget=Budget(tokens=250_000))
    try:
        model = _session_with_provider(
            core,
            lambda req: ok_reply(
                "fetch",
                tool_calls=[{"id": "c1", "name": "broken.fetch", "arguments": {"url": "u"}}],
            ),
            calls,
        )

        @core.tool("broken.fetch", external=True)
        def broken_fetch(call):  # external but forgets to mark — the bug this catches
            return {"call_id": call.id, "content": "doc", "taint": {"kind": "Clean"}}

        from hesmos import Plan

        receipt = core.run(Plan.from_yaml(single_stage_yaml(model, tools=["broken.fetch"])), task="t")
        assert receipt.final_state == "Completed"
        envelope = checkpoint_envelopes(core.session_id)["research"]
        result = envelope["payload"]["json"]["tool_results"][0]
        assert result["ok"] is False
        assert "unmarked external import" in result["reason"]
        # And the session output stays Clean — the rejected result never flowed.
        assert envelope["taint"] == "clean"
    finally:
        core.close()


def test_tainted_result_marks_every_downstream_envelope(root):
    """AC①'s envelope half: an accepted Tainted result declares its origin; the
    runner marks the output envelope (the sanctioned Clean→Tainted path) and the
    derivation carries it — visible in the committed checkpoint envelope."""
    calls: list = []
    core = Session(seed=42, budget=Budget(tokens=250_000))
    try:
        model = _session_with_provider(
            core,
            lambda req: ok_reply(
                "fetch",
                tool_calls=[{"id": "c1", "name": "web.fetch", "arguments": {"url": "u"}}],
            ),
            calls,
        )

        @core.tool("web.fetch", external=True)
        def web_fetch(call):
            return {
                "call_id": call.id,
                "content": "external document",
                "taint": {"kind": "Tainted", "source": {"origin": "web.fetch"}},
            }

        from hesmos import Plan

        receipt = core.run(
            Plan.from_yaml(two_stage_yaml(model, ["web.fetch"], ["web.fetch"])), task="t"
        )
        assert receipt.final_state == "Completed"
        envelopes = checkpoint_envelopes(core.session_id)
        # research marked at the boundary; draft DERIVED from it stays tainted (S5).
        assert envelopes["research"]["taint"] == {
            "tainted": {"source": {"origin": "web.fetch"}}
        }
        assert envelopes["draft"]["taint"] == {"tainted": {"source": {"origin": "web.fetch"}}}
    finally:
        core.close()


def test_skill_rejection_through_a_session_tool_records_ok_false(root):
    """AC⑤'s trace half: a skill loader that hits a rejection raises inside the
    tool callback; the executor records tool.call(ok=false, tool_name) — the
    W3-confirmed single recording path."""
    calls: list = []
    rogue_dir = root / "rogue-skill"
    _write_skill(rogue_dir, {**DECLARATIVE, "entry_point": "pwn.py"})
    core = Session(seed=42, budget=Budget(tokens=250_000))
    try:
        model = _session_with_provider(
            core,
            lambda req: ok_reply(
                "load skill",
                tool_calls=[
                    {"id": "c1", "name": "skill.load", "arguments": {"path": str(rogue_dir)}}
                ],
            ),
            calls,
        )

        @core.tool("skill.load")
        def load_skill(call):
            record = load(call.arguments["path"])
            if not record["ok"]:
                raise RuntimeError(f"skill rejected: {record['reason']}")
            return {"call_id": call.id, "content": "loaded", "taint": {"kind": "Clean"}}

        from hesmos import Plan

        receipt = core.run(
            Plan.from_yaml(two_stage_yaml(model, ["skill.load"], ["skill.load"])), task="t"
        )
        assert receipt.final_state == "Completed"  # a recorded rejection ≠ session failure
        tool_events = [e for e in events(core.session_id) if e["kind"] == "tool.call"]
        assert tool_events
        assert tool_events[0]["attrs"] == {
            **tool_events[0]["attrs"],
            "tool_name": "skill.load",
            "ok": False,
            "actor": "executor",
        }
    finally:
        core.close()


# --- MCP stdio client + permission gate pre-verification (AC⑦) --------------

MCP_STUB = """
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    if req.get("method") == "initialize":
        reply = {"jsonrpc": "2.0", "id": req["id"], "result": {
            "protocolVersion": req["params"]["protocolVersion"], "capabilities": {},
            "serverInfo": {"name": "stub-docs"}}}
    elif req.get("method") == "tools/call":
        reply = {"jsonrpc": "2.0", "id": req["id"], "result": {"content": [
            {"type": "text", "text": "doc-" + req["params"]["arguments"]["q"]}]}}
    else:
        continue
    sys.stdout.write(json.dumps(reply) + "\\n")
    sys.stdout.flush()
"""


def test_mcp_client_marks_every_result_tainted_with_server_origin():
    client = McpStdioClient([sys.executable, "-c", MCP_STUB])
    with client:
        assert client.start() == "stub-docs"
        result = client.call_tool("fetch_docs", {"q": "hesmos"})
    assert result["taint"] == {"kind": "Tainted", "source": {"origin": "mcp:stub-docs"}}
    assert result["content"] == "doc-hesmos"
    client.close()


def test_mcp_tool_through_session_taints_the_envelope(root):
    """Full external loop: model → tool_call → MCP server → Tainted result →
    the committed envelope carries the mcp: origin."""
    calls: list = []
    core = Session(seed=42, budget=Budget(tokens=250_000))
    try:
        model = _session_with_provider(
            core,
            lambda req: ok_reply(
                "search",
                tool_calls=[{"id": "c1", "name": "docs.search", "arguments": {"q": "x"}}],
            ),
            calls,
        )
        client = McpStdioClient([sys.executable, "-c", MCP_STUB])
        client.start()

        @core.tool("docs.search", external=True)
        def docs_search(call):
            return client.call_tool("fetch_docs", call.arguments)

        from hesmos import Plan

        receipt = core.run(
            Plan.from_yaml(single_stage_yaml(model, tools=["docs.search"])), task="t"
        )
        assert receipt.final_state == "Completed"
        client.close()
        envelope = checkpoint_envelopes(core.session_id)["research"]
        assert envelope["taint"] == {"tainted": {"source": {"origin": "mcp:stub-docs"}}}
    finally:
        core.close()


def test_permission_gate_blocks_tool_before_any_callback_runs(root):
    """AC⑦: the gate is the PRE check — a receiver whose toolset exceeds its
    grantor's authority is privilege amplification; the session closes FAILED
    (a receipt, like every terminal state) and the tool callback NEVER runs."""
    from hesmos import Plan

    invocations: list = []
    core = Session(seed=42, budget=Budget(tokens=250_000))
    try:
        model = _session_with_provider(core, lambda req: ok_reply("t"), invocations)
        fired: list = []

        @core.tool("secret.tool")
        def secret_tool(call):
            fired.append(call)
            return {"call_id": call.id, "content": "x", "taint": {"kind": "Clean"}}

        receipt = core.run(
            Plan.from_yaml(two_stage_yaml(model, [], ["secret.tool"])), task="t"
        )
        assert receipt.final_state == "Failed"  # gate reject is a terminal receipt
        gate_fails = [
            e
            for e in events(core.session_id)
            if e["kind"] == "gate.fail" and e["attrs"].get("gate_id") == "permission"
        ]
        assert gate_fails and gate_fails[0]["attrs"]["reason_code"] == "GATE_REJECT"
        assert fired == []  # pre-verification: nothing ever reached the callback
        assert receipt.chain_head  # the failure itself is sealed evidence
    finally:
        core.close()


def test_toolbox_refuses_tools_outside_the_plan_allowlist(root):
    """Registration-side convenience sharing the gate's allowlist data: a tool no
    node's profile carries is a wiring error, surfaced at registration time."""
    from hesmos import Plan

    plan = Plan.from_yaml(single_stage_yaml("m", tools=["local.note"]))
    toolbox = Toolbox(core=None, plan=plan)
    assert toolbox.allowlist == {"local.note"}
    with pytest.raises(ValueError, match="not in the plan's tool allowlist"):
        toolbox.register("ghost.tool", lambda call: None)
