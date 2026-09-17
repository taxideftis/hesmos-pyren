"""Prompt builder — PY-7 (SS-18 cache invariants, Python side).

Placement only: this module arranges messages into the stable/context/volatile
layers and places cache breakpoints. System-prompt hash VERIFICATION belongs to
the core (WP-P2a guard/cache.rs) and is deliberately not reimplemented here —
two sources of truth for verification is exactly the failure mode PY-7 forbids.

Everything here is provider-neutral; mapping the layer boundaries onto GLM/Anthropic
`cache_control` blocks is the WP-P2c adapter's job.
"""

from __future__ import annotations

import json
from typing import Any, Iterable, Literal, NamedTuple, Sequence

from hesmos import models

__all__ = [
    "BuiltPrompt",
    "ChangeKind",
    "ChangeRequest",
    "DeferredChangeGate",
    "DuplicateTokenMeter",
    "PromptBuilder",
    "TurnMeterRecord",
]

ChangeKind = Literal["tool", "skill", "memory", "compression"]

# Frozen after the first build (SS-18 rule 1): the stable layer must be byte-stable
# for the whole session, so the builder refuses config changes instead of trusting
# callers not to mutate. Read-only properties with no setters = no seam to break.
class PromptBuilder:
    """Session-bound builder holding the byte-stable stable-layer sources."""

    def __init__(self, *, system_prompt: str, tool_defs: Sequence[models.ToolDef] = ()) -> None:
        self._system_prompt = system_prompt
        self._tool_defs = tuple(tool_defs)
        self._sealed = False

    @property
    def system_prompt(self) -> str:
        return self._system_prompt

    @property
    def tool_defs(self) -> tuple[models.ToolDef, ...]:
        return self._tool_defs

    @property
    def sealed(self) -> bool:
        return self._sealed

    def __setattr__(self, name: str, value: Any) -> None:
        # Rejects post-init mutation of the stable sources (system_prompt/tool_defs)
        # and any attempt to swap internal state mid-session.
        if getattr(self, "_sealed", False) and name != "_sealed":
            raise AttributeError(
                "PromptBuilder is sealed after the first build (SS-18 rule 1: the "
                "stable layer is byte-stable for the session) — start a new session "
                "instead; deferred changes land via DeferredChangeGate"
            )
        object.__setattr__(self, name, value)

    def build(
        self,
        stage: str,
        snapshot: models.SnapshotSlice,
        *,
        history: Sequence[models.Message] = (),
        assumptions: Iterable[dict[str, Any]] = (),
        task_input: str = "",
    ) -> BuiltPrompt:
        """Assemble one turn's messages and place the 3-layer breakpoints.

        Pure function of the arguments plus the sealed config: identical inputs
        produce identical output (deterministic rendering — sorted-key JSON, stable
        line order). `history` is the context layer the caller passes per turn;
        nothing here mutates it.
        """
        self._sealed = True

        stable_messages: list[models.Message] = [
            models.Message(role="system", content=self._system_prompt)
        ]
        context_messages: list[models.Message] = list(history)
        volatile_messages: list[models.Message] = [
            models.Message(
                role="user",
                content=_render_volatile(stage, snapshot, assumptions, task_input),
            )
        ]

        messages = stable_messages + context_messages + volatile_messages
        # Breakpoints are exclusive prefix lengths into `messages`. volatile is the
        # uncached tail: marking its end documents the boundary without caching it.
        cache_breakpoints = models.CacheBreakpoints(
            stable=len(stable_messages),
            context=len(stable_messages) + len(context_messages),
            volatile=len(messages),
        )
        return BuiltPrompt(
            messages=tuple(messages),
            cache_breakpoints=cache_breakpoints,
            tools_schema=self._tool_defs,
        )


class BuiltPrompt(NamedTuple):
    """Builder output — the fields LlmRequest needs, plus the placed breakpoints.

    Deliberately carries NO hash: verification is core-side (PY-7 invariant).
    Message/CacheBreakpoints are frozen mirrors, so callers cannot inject cache
    boundaries — models use extra="forbid" and no breakpoint field exists.
    """

    messages: tuple[models.Message, ...]
    cache_breakpoints: models.CacheBreakpoints
    tools_schema: tuple[models.ToolDef, ...]

    def to_llm_request(
        self, *, temperature: float | None = None, max_tokens: int | None = None
    ) -> models.LlmRequest:
        return models.LlmRequest(
            messages=list(self.messages),
            temperature=temperature,
            max_tokens=max_tokens,
            tools_schema=list(self.tools_schema),
            cache_breakpoints=self.cache_breakpoints,
        )


def _render_volatile(
    stage: str,
    snapshot: models.SnapshotSlice,
    assumptions: Iterable[dict[str, Any]],
    task_input: str,
) -> str:
    """Render the volatile layer deterministically (same inputs -> same bytes).

    Assumptions with confirmed=False get a [TENTATIVE] marker (US-10 AC3) so the
    builder never presents provisional items as settled fact.
    """
    lines = [f"stage: {stage}"]
    if task_input:
        lines.append(f"task: {task_input}")
    lines.append(f"goal: {snapshot.goal_original}")
    # Sorted keys, fixed separators: JSON text must not depend on dict insertion order.
    lines.append("node_input: " + json.dumps(snapshot.node_input.model_dump(by_alias=True), sort_keys=True, separators=(",", ":"), ensure_ascii=False))
    for assumption in assumptions:
        text = str(assumption.get("text", ""))
        prefix = "" if assumption.get("confirmed") is True else "[TENTATIVE] "
        lines.append(f"assumption: {prefix}{text}")
    if snapshot.shared_knowledge:
        lines.append(
            "shared_knowledge: "
            + json.dumps(snapshot.shared_knowledge, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
        )
    return "\n".join(lines)


class ChangeRequest(NamedTuple):
    """One deferred-able change command (SS-18 rule 3)."""

    kind: ChangeKind
    target: str
    payload: Any = None


class DeferredChangeGate:
    """Routes tool/skill/memory changes: deferred by default, --now opt-in.

    Compression is the ONLY immediate-by-default kind (SS-18 rule 4). Its event
    recording is intentionally absent in W5 — compression itself is unscheduled
    (lead-confirmed 2026-09-17); the recording path is decided when compression
    lands, possibly with a TYPE-3 revision. The gate marks compression turns so
    that future wiring has a single seam to hook.
    """

    def __init__(self) -> None:
        self._pending: list[ChangeRequest] = []
        self.compression_applied: list[ChangeRequest] = []

    def apply_change(self, change: ChangeRequest, *, now: bool = False) -> Literal["immediate", "deferred"]:
        if change.kind == "compression":
            # Unique immediate exception — no --now needed. Recording is the one
            # missing piece (see class docstring), never a silent pass-through.
            self.compression_applied.append(change)
            return "immediate"
        if now:
            return "immediate"
        # Insertion order preserved — next-session pickup is deterministic.
        self._pending.append(change)
        return "deferred"

    def drain_pending(self) -> list[ChangeRequest]:
        """Hand deferred changes to the next session's builder config (P2b wires this)."""
        pending, self._pending = self._pending, []
        return pending


class TurnMeterRecord(NamedTuple):
    turn: int
    duplicate_tokens: int
    threshold: int | None
    exceeded: bool


class DuplicateTokenMeter:
    """Per-turn duplicate-token measurement and over-threshold detection (US-21 AC5).

    duplicate_tokens = tokens_in - cache_read_tokens: tokens the provider re-processed
    despite the cache discipline. The threshold has NO product default — no baseline
    number exists upstream (AC5), so detection stays disabled until a threshold is
    configured (tests inject fixture values; W6 sets the real baseline via a
    baseline-revision procedure). Inventing a default here would fabricate a limit.
    """

    def __init__(self, *, threshold: int | None = None) -> None:
        self.threshold = threshold
        self.records: list[TurnMeterRecord] = []

    def observe(
        self, *, turn: int, tokens_in: int, cache_read_tokens: int = 0
    ) -> TurnMeterRecord:
        duplicate_tokens = tokens_in - cache_read_tokens
        exceeded = self.threshold is not None and duplicate_tokens > self.threshold
        record = TurnMeterRecord(
            turn=turn,
            duplicate_tokens=duplicate_tokens,
            threshold=self.threshold,
            exceeded=exceeded,
        )
        self.records.append(record)
        return record
