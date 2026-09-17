"""GLM adapter — PY-3 callback implementation (SS-17, ADR-0005).

The single primary backend is GLM-5.3 flash via the Anthropic-compatible
endpoint. This module is the ONLY place provider specifics exist: wire field
names, cache_control placement, and endpoint quirks are absorbed here so the
core and prompt/ stay provider-neutral (PT-6 adapter, SS-26 rule 2).

What is deliberately ABSENT (each enforced by a test):
  - no retry loop: failures raise once; the core applies bounded retry
    (exceptions.md §2/§4 — FFI-CALLBACK -> PROVIDER_FAILURE)
  - no model-mix detection: bathos E-MODEL-MIX is the only judge (P8 — no
    duplicated truth); hesmos passes the bathos exit through (exit 30)
  - no ledger writes: llm.call accounting flows from LlmReply.tokens_in/out
    through the core's EventSink (SS-17 rule 3 — single source)

Key handling: the API key arrives via the environment (bathos key store or
caller-injected). It is NEVER written to files, logs, or error messages here.
"""

from __future__ import annotations

import os
import time
from typing import Any, Callable, Mapping

import httpx

from hesmos import models

__all__ = ["GlmAdapter", "model_plan_conformance"]

ENDPOINT = "https://api.z.ai/api/anthropic"
MODEL_REF = "glm-5.3-flash"
_API_KEY_ENV = "HESMOS_GLM_API_KEY"
_ANTHROPIC_VERSION = "2023-06-01"
# Anthropic-compatible caching allows up to 4 breakpoints; the 3-layer scheme
# uses at most 3 (tools, stable/system, last context message).
_CACHE_CONTROL = {"type": "ephemeral"}


def model_plan_conformance(model_plan: Mapping[str, Any], expected_model: str = MODEL_REF) -> bool:
    """String-level model-plan conformance (SS-17 rule 1 — that is ALL we own).

    The bathos model validate (E-MODEL-MIX) runs upstream at session start via
    PORT-2; this helper only answers "does the plan still name our single
    backend". The model-plan.json schema is bathos-owned — unknown shapes
    conformance to False, never raise, so the runner decides policy.
    """
    if model_plan.get("runtime") != "glm":
        return False
    models_in_plan = model_plan.get("models")
    if isinstance(models_in_plan, Mapping):
        return all(model == expected_model for model in models_in_plan.values())
    if isinstance(models_in_plan, str):
        return models_in_plan == expected_model
    return model_plan.get("model") == expected_model


class GlmResponseError(RuntimeError):
    """Failed/malformed provider response — becomes FFI-CALLBACK->PROVIDER_FAILURE.

    Messages must never contain the API key (they may carry response snippets).
    """


class GlmAdapter:
    """Callable provider adapter: models.LlmRequest -> models.LlmReply.

    `http_post` is injectable for stub tests; the default performs exactly ONE
    synchronous httpx POST per call (no async — single-process session model;
    no retry — core owns bounded retry).
    """

    def __init__(
        self,
        *,
        api_key: str | None = None,
        model: str = MODEL_REF,
        endpoint: str = ENDPOINT,
        timeout_s: float = 60.0,
        default_max_tokens: int = 4096,  # transport floor; the wire field is required
        http_post: Callable[..., Any] | None = None,
    ) -> None:
        # Fail fast on a missing key BEFORE any HTTP attempt — a late failure
        # would surface as a mysterious PROVIDER_FAILURE mid-session.
        key = api_key or os.environ.get(_API_KEY_ENV)
        if not key:
            raise RuntimeError(
                f"GLM API key missing — set {_API_KEY_ENV} (never in files; "
                "stdin/--file only)"
            )
        self._key = key
        self._model = model
        self._endpoint = endpoint
        self._timeout_s = timeout_s
        self._default_max_tokens = default_max_tokens
        self._http_post = http_post if http_post is not None else self._default_post

    def model_ref(self) -> str:
        return self._model

    @staticmethod
    def _default_post(
        endpoint: str, headers: dict, payload: dict, timeout_s: float
    ) -> httpx.Response:
        return httpx.post(endpoint, headers=headers, json=payload, timeout=timeout_s)

    def __call__(self, request: models.LlmRequest) -> models.LlmReply:
        started = time.monotonic()
        response = self._http_post(
            self._endpoint,
            {
                "x-api-key": self._key,
                "anthropic-version": _ANTHROPIC_VERSION,
                "content-type": "application/json",
            },
            self._to_wire(request),
            self._timeout_s,
        )
        latency_ms = int((time.monotonic() - started) * 1000)
        return self._from_wire(self._expect_json(response), latency_ms)

    # --- request mapping (provider-neutral -> Anthropic-compatible wire) ---

    def _to_wire(self, request: models.LlmRequest) -> dict[str, Any]:
        bp = request.cache_breakpoints
        system_blocks: list[dict[str, Any]] = []
        chat_messages: list[dict[str, Any]] = []
        # request.messages index -> position in chat_messages (system messages are
        # moved to the `system` field, so wire indexes shift); breakpoint targets
        # are prefix lengths in request.messages order and must be translated.
        chat_index_of: dict[int, int] = {}

        for index, message in enumerate(request.messages):
            if message.role == "system":
                block: dict[str, Any] = {"type": "text", "text": message.content}
                if index + 1 == bp.stable:
                    # Stable prefix ends inside the system prompt -> cache it.
                    block["cache_control"] = dict(_CACHE_CONTROL)
                system_blocks.append(block)
            else:
                chat_index_of[index] = len(chat_messages)
                chat_messages.append(self._message_to_wire(message))

        # Context breakpoint rides the LAST context message; volatile (the current
        # turn) gets none — it changes every turn by design.
        if bp.stable < bp.context <= len(request.messages):
            target = chat_index_of.get(bp.context - 1)
            if target is not None:
                content = chat_messages[target]["content"]
                if isinstance(content, str):
                    chat_messages[target]["content"] = [
                        {"type": "text", "text": content, "cache_control": dict(_CACHE_CONTROL)}
                    ]
                elif isinstance(content, list) and content:
                    content[-1]["cache_control"] = dict(_CACHE_CONTROL)

        tools = [
            {
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters_schema,
                # Tools belong to the stable layer; caching them costs one of the
                # <=4 breakpoints and stops re-sending schemas on every call.
                "cache_control": dict(_CACHE_CONTROL),
            }
            for tool in request.tools_schema
        ]

        payload: dict[str, Any] = {
            "model": self._model,
            "max_tokens": request.max_tokens if request.max_tokens is not None else self._default_max_tokens,
            "messages": chat_messages,
        }
        if request.temperature is not None:
            payload["temperature"] = request.temperature
        if system_blocks:
            payload["system"] = system_blocks
        if tools:
            payload["tools"] = tools
        return payload

    @staticmethod
    def _message_to_wire(message: models.Message) -> dict[str, Any]:
        if message.role == "tool":
            # Anthropic models tool results as user-turn tool_result blocks; the
            # mirror's tool_call_id carries the provider's tool_use id.
            return {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": message.tool_call_id,
                        "content": message.content,
                    }
                ],
            }
        return {"role": message.role, "content": message.content}

    # --- response mapping (wire -> provider-neutral LlmReply) ---

    def _expect_json(self, response: Any) -> dict[str, Any]:
        status = getattr(response, "status_code", None)
        if status != 200:
            text = getattr(response, "text", "")
            snippet = text[:200] if isinstance(text, str) else ""
            # Provider error bodies sometimes echo the caller's key — scrub it so
            # the exception never becomes a key-leak path (it may be logged).
            snippet = snippet.replace(self._key, "***")
            raise GlmResponseError(f"GLM HTTP {status}: {snippet}")
        try:
            return response.json()
        except (ValueError, AttributeError) as error:
            raise GlmResponseError(f"GLM non-JSON response: {error}") from error

    def _from_wire(self, body: dict[str, Any], latency_ms: int) -> models.LlmReply:
        usage = body.get("usage") or {}
        input_tokens = usage.get("input_tokens")
        output_tokens = usage.get("output_tokens")
        # Accounting contract (PY-3 invariant): tokens_in/out MUST be present —
        # a reply without them would silently break llm.call accounting, so this
        # raises instead of defaulting (no swallow; the core fails the session).
        if input_tokens is None or output_tokens is None:
            raise GlmResponseError(
                "GLM response missing usage tokens — accounting would be violated"
            )
        # Full billed input = uncached + cache-read + cache-write tokens.
        tokens_in = (
            int(input_tokens)
            + int(usage.get("cache_read_input_tokens") or 0)
            + int(usage.get("cache_creation_input_tokens") or 0)
        )

        content_parts: list[str] = []
        tool_calls: list[models.ToolCall] = []
        for block in body.get("content") or []:
            kind = block.get("type")
            if kind == "text":
                content_parts.append(block.get("text", ""))
            elif kind == "tool_use":
                tool_calls.append(
                    models.ToolCall(
                        id=block.get("id", ""),
                        name=block.get("name", ""),
                        arguments=block.get("input", {}),
                    )
                )

        provider_meta = {
            "provider": "glm",
            "model": self._model,
            "endpoint": self._endpoint,
            "latency_ms": latency_ms,
            "usage_raw": usage,
        }
        return models.LlmReply(
            content="".join(content_parts),
            tool_calls=tool_calls,
            tokens_in=tokens_in,
            tokens_out=int(output_tokens),
            provider_meta=provider_meta,
        )
