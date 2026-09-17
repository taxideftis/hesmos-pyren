"""P2d tests — 3-layer placement, deferred gate, turn metering (PY-7, SS-18).

The builder is pure Python over generated mirrors: no core import, no hash
verification (core-side per PY-7 invariant). Each AC maps to a named test.
"""

import json

import pydantic
import pytest

from hesmos import models
from hesmos.prompt import (
    BuiltPrompt,
    ChangeRequest,
    DeferredChangeGate,
    DuplicateTokenMeter,
    PromptBuilder,
)

SYSTEM = "You are the Hesmos research agent."

SNAPSHOT = models.SnapshotSlice.model_validate(
    {
        "goal_original": "Draft a competitive feature report",
        "prev_contract_summary": "none",
        "node_input": {"schema_id": "s", "json": {"q": "cache"}},
        "shared_knowledge": [{"k": "pricing"}],
    }
)


def tool_def(name: str) -> models.ToolDef:
    return models.ToolDef.model_validate(
        {"name": name, "description": f"{name} tool", "parameters_schema": {"type": "object"}}
    )


def builder() -> PromptBuilder:
    return PromptBuilder(system_prompt=SYSTEM, tool_defs=[tool_def("web.search")])


def test_three_layers_placed_in_order_ac1():
    built = builder().build("research", SNAPSHOT, history=[])
    assert [m.role for m in built.messages] == ["system", "user"]
    bp = built.cache_breakpoints
    # Breakpoints are exclusive prefix lengths, strictly increasing.
    assert bp.stable == 1
    assert bp.context == 1
    assert bp.volatile == len(built.messages)
    assert bp.stable <= bp.context <= bp.volatile


def test_context_layer_carries_history_between_stable_and_volatile():
    history = (
        models.Message(role="user", content="start"),
        models.Message(role="assistant", content="ok"),
    )
    built = builder().build("research", SNAPSHOT, history=history)
    assert [m.content for m in built.messages] == [
        SYSTEM,
        "start",
        "ok",
        built.messages[-1].content,
    ]
    assert built.cache_breakpoints.stable == 1
    assert built.cache_breakpoints.context == 3  # stable + history


def test_placement_is_deterministic():
    b = builder()
    a1 = b.build("research", SNAPSHOT, history=())
    a2 = PromptBuilder(system_prompt=SYSTEM, tool_defs=[tool_def("web.search")]).build(
        "research", SNAPSHOT, history=()
    )
    assert a1.messages == a2.messages
    assert a1.cache_breakpoints == a2.cache_breakpoints
    # Same builder, same inputs -> same bytes (no hidden state in the volatile layer).
    assert b.build("research", SNAPSHOT, history=()).messages == a1.messages


def test_stable_layer_sealed_after_first_build_ac2_invariant():
    b = builder()
    b.build("research", SNAPSHOT)
    assert b.sealed
    with pytest.raises(AttributeError):
        b.system_prompt = "mutated prompt"
    with pytest.raises(AttributeError):
        b.tool_defs = ()


def test_stable_prefix_byte_stable_across_turns():
    b = builder()
    t1 = b.build("research", SNAPSHOT, history=())
    history = (models.Message(role="user", content="start"),)
    t2 = b.build("draft", SNAPSHOT, history=history)
    # Stable layer content is identical byte-for-byte across turns.
    assert t1.messages[0] == t2.messages[0]
    assert t1.messages[0].content == SYSTEM


def test_volatile_renders_deterministically_with_sorted_json():
    built = builder().build("research", SNAPSHOT, task_input="do X")
    content = built.messages[-1].content
    assert "stage: research" in content
    assert "task: do X" in content
    assert json.dumps(SNAPSHOT.node_input.model_dump(by_alias=True), sort_keys=True, separators=(",", ":"), ensure_ascii=False) in content


def test_tentative_assumptions_marked_not_presented_as_fact():
    built = builder().build(
        "research",
        SNAPSHOT,
        assumptions=[
            {"text": "competitor ships v2", "confirmed": True},
            {"text": "market grows 10%", "confirmed": False},
        ],
    )
    content = built.messages[-1].content
    assert "assumption: competitor ships v2" in content
    assert "assumption: [TENTATIVE] market grows 10%" in content


def test_user_breakpoint_injection_is_structurally_impossible_ss18_rule5():
    # No user-facing breakpoint parameter exists, and the frozen mirror forbids
    # extra fields — a cache_control-style hint cannot even be constructed.
    with pytest.raises(pydantic.ValidationError):
        models.Message.model_validate(
            {"role": "user", "content": "hi", "cache_control": {"type": "ephemeral"}}
        )
    built = builder().build("research", SNAPSHOT)
    assert all(not hasattr(m, "cache_control") for m in built.messages)


def test_to_llm_request_carries_breakpoints_for_core_validation():
    built = builder().build("research", SNAPSHOT)
    request = built.to_llm_request(temperature=0.0)
    assert isinstance(request, models.LlmRequest)
    assert request.cache_breakpoints == built.cache_breakpoints
    assert request.tools_schema == list(built.tools_schema)


def test_apply_change_defaults_deferred_us22_ac2():
    gate = DeferredChangeGate()
    outcome = gate.apply_change(ChangeRequest(kind="tool", target="write.file"))
    assert outcome == "deferred"
    assert gate.drain_pending() == [ChangeRequest(kind="tool", target="write.file")]


def test_apply_change_now_is_immediate_us22_ac3():
    gate = DeferredChangeGate()
    outcome = gate.apply_change(ChangeRequest(kind="skill", target="sql.v2"), now=True)
    assert outcome == "immediate"
    assert gate.drain_pending() == []  # never queued


def test_compression_is_the_only_immediate_exception_branch_ac4():
    gate = DeferredChangeGate()
    outcome = gate.apply_change(ChangeRequest(kind="compression", target="context"))
    assert outcome == "immediate"
    assert gate.compression_applied == [ChangeRequest(kind="compression", target="context")]
    assert gate.drain_pending() == []


def test_meter_records_every_turn_and_detects_with_injected_threshold_ac5():
    # Fixture threshold — NOT a product baseline (AC5 forbids inventing the number;
    # detection defaults to disabled until W6 sets a real baseline).
    meter = DuplicateTokenMeter(threshold=100)
    r1 = meter.observe(turn=1, tokens_in=1500, cache_read_tokens=1450)
    r2 = meter.observe(turn=2, tokens_in=1500, cache_read_tokens=0)
    assert (r1.duplicate_tokens, r1.exceeded) == (50, False)
    assert (r2.duplicate_tokens, r2.exceeded) == (1500, True)
    assert [rec.turn for rec in meter.records] == [1, 2]


def test_meter_detection_disabled_without_threshold():
    meter = DuplicateTokenMeter()
    record = meter.observe(turn=1, tokens_in=10**6, cache_read_tokens=0)
    assert record.exceeded is False and record.threshold is None


def test_built_prompt_has_no_hash_python_never_verifies():
    # PY-7 invariant: verification is core-side — the builder output exposes layers
    # for the core to hash, and no hash-like field exists in the Python result.
    built = builder().build("research", SNAPSHOT)
    assert isinstance(built, BuiltPrompt)
    assert not any("hash" in field.lower() for field in built._fields)
