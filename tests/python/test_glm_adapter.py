"""P2c tests — GLM adapter, model-plan conformance, usage accounting (T9, SS-17).

Stub-first per the lead's condition: live calls are gated behind HESMOS_GLM_LIVE
and skipped in the default suite (only a minimal live check needs the real endpooint).
"""

import os

import pytest

from hesmos import models
from hesmos.glm import ENDPOINT, MODEL_REF, GlmAdapter, GlmResponseError, model_plan_conformance


def make_request(**overrides) -> models.LlmRequest:
    messages = [
        models.Message(role="system", content="You are the Hesmos research agent."),
        models.Message(role="user", content="start"),
        models.Message(role="assistant", content="ok"),
        models.Message(role="user", content="continue"),
    ]
    fields = dict(
        messages=messages,
        temperature=0.0,
        max_tokens=None,
        tools_schema=[
            models.ToolDef.model_validate(
                {"name": "web.search", "description": "search", "parameters_schema": {"type": "object"}}
            )
        ],
        cache_breakpoints=models.CacheBreakpoints(stable=1, context=3, volatile=4),
    )
    fields.update(overrides)
    return models.LlmRequest.model_validate(fields)


def stub_post(response=None, *, error: Exception | None = None):
    """Stub http post: records every call, always returns `response` (or raises `error`)."""
    calls: list = []

    def post(endpoint, headers, payload, timeout_s):
        calls.append({"endpoint": endpoint, "headers": headers, "payload": payload})
        if error is not None:
            raise error
        return response

    post.calls = calls  # type: ignore[attr-defined]
    return post


OK_BODY = {"content": [], "usage": {"input_tokens": 1, "output_tokens": 1}}


def wire_response(body):
    class R:
        status_code = 200

        def json(self):
            return body

    return R()


def adapter(post) -> GlmAdapter:
    return GlmAdapter(api_key="test-key-xyz", http_post=post)


# --- AC1: model-plan conformance (string level only) ---

def test_model_plan_conformance_passes_runtime_glm():
    assert model_plan_conformance({"runtime": "glm", "models": {"research": "glm-5.3-flash"}})
    assert model_plan_conformance({"runtime": "glm", "model": "glm-5.3-flash"})
    assert model_plan_conformance({"runtime": "glm", "models": "glm-5.3-flash"})


def test_model_plan_conformance_rejects_mismatch_and_unknown_shapes():
    assert not model_plan_conformance({"runtime": "glm", "model": "other-model"})
    assert not model_plan_conformance({"runtime": "openai", "model": "glm-5.3-flash"})
    assert not model_plan_conformance({"runtime": "glm"})  # no model field
    assert not model_plan_conformance({})  # unknown shape -> False, never raise


# --- wire mapping (provider specifics stay in the adapter only) ---

def test_wire_mapping_splits_system_tools_and_messages():
    post = stub_post(wire_response(OK_BODY))
    a = adapter(post)
    a(make_request())
    payload = post.calls[0]["payload"]
    # system moved out of messages; tools mapped with input_schema
    assert payload["model"] == MODEL_REF
    assert payload["system"] == [{"type": "text", "text": "You are the Hesmos research agent.", "cache_control": {"type": "ephemeral"}}]
    assert [m["role"] for m in payload["messages"]] == ["user", "assistant", "user"]
    assert payload["tools"][0]["name"] == "web.search"
    assert payload["tools"][0]["input_schema"] == {"type": "object"}
    assert payload["temperature"] == 0.0


def test_tool_result_maps_to_anthropic_tool_result_block():
    request = make_request(
        messages=[
            models.Message(role="system", content="s"),
            models.Message(role="user", content="go"),
            models.Message(role="tool", content="page text", tool_call_id="tu_1"),
        ],
        cache_breakpoints=models.CacheBreakpoints(stable=1, context=2, volatile=3),
    )
    post = stub_post(wire_response(OK_BODY))
    adapter(post)(request)
    payload = post.calls[0]["payload"]
    tool_message = payload["messages"][-1]
    assert tool_message["role"] == "user"
    assert tool_message["content"][0]["type"] == "tool_result"
    assert tool_message["content"][0]["tool_use_id"] == "tu_1"


def test_cache_control_placed_on_stable_and_context_only_not_volatile():
    post = stub_post(wire_response(OK_BODY))
    adapter(post)(make_request())
    payload = post.calls[0]["payload"]
    # stable: system block has cache_control (checked above); context: message at
    # request index 2 (assistant "ok") carries it; volatile (last) must NOT.
    context_content = payload["messages"][1]["content"]
    assert isinstance(context_content, list) and "cache_control" in context_content[-1]
    volatile = payload["messages"][-1]["content"]
    assert isinstance(volatile, str)  # untouched -> no cache_control possible
    # breakpoint budget: system(1) + tools(1) + context(1) = 3 <= 4
    placed = sum(
        1
        for section in ("system", "tools")
        for item in payload.get(section, [])
        if "cache_control" in item
    ) + sum(
        1
        for m in payload["messages"]
        if isinstance(m["content"], list) and any("cache_control" in c for c in m["content"])
    )
    assert placed == 3


# --- usage accounting (AC3 source — LlmReply.tokens_in/out always filled) ---

def test_reply_tokens_and_meta_filled_from_usage():
    body = {
        "content": [{"type": "text", "text": "done"}],
        "usage": {
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_read_input_tokens": 300,
            "cache_creation_input_tokens": 50,
        },
    }
    reply = adapter(stub_post(wire_response(body)))(make_request())
    # Full billed input = uncached + read + write.
    assert reply.tokens_in == 450
    assert reply.tokens_out == 20
    assert reply.provider_meta["provider"] == "glm"
    assert reply.provider_meta["model"] == MODEL_REF
    assert reply.provider_meta["endpoint"] == ENDPOINT
    assert isinstance(reply.provider_meta["latency_ms"], int)
    assert reply.provider_meta["usage_raw"]["input_tokens"] == 100


def test_missing_tokens_raises_no_swallow_ss17_rule3():
    body = {"content": [{"type": "text", "text": "done"}], "usage": {"output_tokens": 5}}
    with pytest.raises(GlmResponseError, match="missing usage tokens"):
        adapter(stub_post(wire_response(body)))(make_request())


def test_tool_use_blocks_map_to_tool_calls():
    body = {
        "content": [
            {"type": "text", "text": "checking"},
            {"type": "tool_use", "id": "tu_9", "name": "web.search", "input": {"q": "cache"}},
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5},
    }
    reply = adapter(stub_post(wire_response(body)))(make_request())
    assert reply.content == "checking"
    assert reply.tool_calls == [
        models.ToolCall(id="tu_9", name="web.search", arguments={"q": "cache"})
    ]


# --- failure paths: single call, no retry, key hygiene ---

def test_http_error_raises_once_and_does_not_retry():
    class R:
        status_code = 503
        text = "upstream down"

    post = stub_post(response=R())
    with pytest.raises(GlmResponseError, match="GLM HTTP 503"):
        adapter(post)(make_request())
    assert len(post.calls) == 1  # exactly one HTTP attempt — core owns retry


def test_transport_exception_propagates_after_single_attempt():
    post = stub_post(error=ConnectionError("boom"))
    with pytest.raises(ConnectionError):
        adapter(post)(make_request())
    assert len(post.calls) == 1


def test_error_messages_never_contain_the_key():
    class R:
        status_code = 401
        text = 'auth failed for key "test-key-xyz"'

    with pytest.raises(GlmResponseError) as excinfo:
        adapter(stub_post(response=R()))(make_request())
    assert "test-key-xyz" not in str(excinfo.value)


def test_missing_key_fails_fast_without_http(monkeypatch):
    monkeypatch.delenv("HESMOS_GLM_API_KEY", raising=False)
    post = stub_post([])
    with pytest.raises(RuntimeError, match="HESMOS_GLM_API_KEY"):
        GlmAdapter(http_post=post)
    assert post.calls == []


# --- absence tests: what the adapter must NOT contain (P8 / exceptions §2) ---

def test_no_model_mix_exit_or_ledger_surface_in_namespace():
    # P8: bathos E-MODEL-MIX is the only judge — the adapter exposes no detection,
    # exit-code, or ledger surface. (Textual scans would false-positive on the
    # docstrings that DOCUMENT the absence; namespace checks cannot.)
    import hesmos.glm.adapter as adapter_module

    public = [n for n in dir(adapter_module) if not n.startswith("_")]
    assert not any(
        "mix" in n.lower() or "exit" in n.lower() or n.startswith("detect") for n in public
    )
    assert not hasattr(adapter_module, "ledger")
    assert not hasattr(adapter_module, "event_sink")
    assert not hasattr(GlmAdapter, "retry") and not hasattr(GlmAdapter, "max_retries")


# --- live smoke (gated): run only with key via env + HESMOS_GLM_LIVE=1 ---

@pytest.mark.skipif(
    os.environ.get("HESMOS_GLM_LIVE") != "1" or not os.environ.get("HESMOS_GLM_API_KEY"),
    reason="live GLM smoke requires HESMOS_GLM_LIVE=1 and the key via env/stdin",
)
def test_live_smoke_minimal_call():
    # Minimal T9 live check: single call, temp 0, model glm-5.3-flash, tokens filled.
    request = make_request(
        messages=[models.Message(role="user", content="Reply with the single word: ok")],
        tools_schema=[],
        cache_breakpoints=models.CacheBreakpoints(stable=0, context=0, volatile=1),
    )
    reply = GlmAdapter()(request)
    assert reply.tokens_in > 0 and reply.tokens_out > 0
    assert reply.provider_meta["model"] == MODEL_REF
