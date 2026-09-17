"""PY-1/PY-5 Session — the FFI session wrapper with deferred native open.

The native session_open performs the FULL open (compile → WAL → events → Running)
and therefore requires the plan and the task (backend.md "P1e 후속" consensus (a):
plan_hash is a required TYPE-5 field, so a handle cannot exist before the plan).
The §6.3 flow constructs `hesmos.Session(seed=…, budget=…)` FIRST and only passes
the plan at `core.run(plan, task=…)` — so the Session defers the FFI open to the
first `run()` call. Consequence: a Session that never runs leaves NO artifacts on
disk (nothing was opened), and `session_id` is only readable after the first run.

Session holds no orchestration state of its own (SS-16 rule 1): everything mutable
lives core-side behind the FFI-1 surface.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from . import _ffi, models
from .exceptions import FfiError


class Budget:
    """Session budget spec in the CLI-1 --budget form: `{"tokens": int}`.

    cost_usd is deliberately NOT part of this spec — the FFI boundary rejects it
    exactly like the CLI-4 grammar (token-centric metering, ERD §7; a silently
    dropped cost would be a swallowed input). The envelope itself is frozen
    core-side at session open (SS-15 rule 1) — this object only carries the
    user-facing spec across the boundary.
    """

    def __init__(self, tokens: int) -> None:
        self.spec: dict[str, Any] = {"tokens": tokens}

    def __repr__(self) -> str:  # pragma: no cover - debug convenience
        return f"Budget({self.spec!r})"


class Session:
    """Entry point: `hesmos.Session(seed=42, budget=hesmos.Budget(tokens=250_000))`.

    `system_prompt=` (optional, SS-18) gives the session a byte-stable system
    prompt: its hash is frozen core-side at open and verified EVERY turn — a
    breach halts the session immediately with no retry (S3). Omitting it is the
    model-neutral default (no prompt, no invariant to verify).
    """

    def __init__(
        self,
        seed: int | None = None,
        budget: Budget | None = None,
        team_id: str | None = None,
        system_prompt: str | None = None,
    ) -> None:
        self._seed = seed
        self._budget_spec = budget.spec if budget is not None else None
        self._team_id = team_id
        self._system_prompt = system_prompt
        self._native: Any | None = None
        self._receipt: models.RunReceipt | None = None
        self._closed = False

    # --- PY-3/PY-4 registration decorators (global registry — FFI-1 surface) ---

    def provider(self, model_ref: str) -> Callable[[Callable[[Any], Any]], Callable[[Any], Any]]:
        """`@core.provider("glm-5.3-flash")` — register the LLM callback (PY-3).

        The callback receives a validated hesmos.LlmRequest and must return a full
        LlmReply (content / tool_calls / tokens_in / tokens_out / provider_meta) —
        missing tokens fail the session (SS-17 rule 3). The wrapper validates both
        directions against the generated mirrors: the request so callbacks get a
        typed object (the mirror IS the wire contract, PORT-2), the reply so a
        malformed one fails with a pydantic error at Python's edge instead of
        crossing the FFI as raw JSON. Signature mismatches and exceptions are
        reported mechanically by the FFI wrapper and converted to PROVIDER_FAILURE
        by the composition adapter (backend.md consensus (b)).
        """

        def register(fn: Callable[[Any], Any]) -> Callable[[Any], Any]:
            # The FFI only ever sees `bridge` (always callable), so the user-side
            # callable check must happen HERE to keep rejecting non-callables.
            if not callable(fn):
                raise FfiError(
                    "Ffi",
                    "FFI-CONTRACT",
                    None,
                    None,
                    "provider callback is not callable",
                    "register a function taking one LlmRequest argument",
                )

            def bridge(req: dict[str, Any]) -> dict[str, Any]:
                reply = fn(models.LlmRequest.model_validate(req))
                return models.LlmReply.model_validate(reply).model_dump()

            _ffi.register_provider(model_ref, bridge)
            return fn

        return register

    def tool(
        self, name: str, *, external: bool = False
    ) -> Callable[[Callable[[Any], Any]], Callable[[Any], Any]]:
        """`@core.tool("write.file", external=False)` — register a tool callback (PY-4).

        The callback receives a validated hesmos.ToolCall and must return a full
        ToolResult (call_id / content / taint) — an unmarked result cannot cross
        the boundary (PT-12: the mirror requires the taint field, so omitting it
        fails validation at Python's edge and the dispatch records the rejection).
        `external=True` is the tool layer's declaration that this tool reads data
        from OUTSIDE the session's trust boundary (web fetch, MCP server, ...);
        the executor feeds it into core's external_import_verdict (SS-20 rule 1)
        and rejects an external tool that returns a Clean result. Dispatch runs
        only for tool_calls the permission gate already passed (SS-19 rule 3).
        """

        def register(fn: Callable[[Any], Any]) -> Callable[[Any], Any]:
            # Mirror bridge, same shape as the provider bridge (PORT-2): the
            # callback gets a typed ToolCall, the FFI gets a validated ToolResult
            # dict. A pydantic ValidationError raised here is reported mechanically
            # by the wrapper and becomes a recorded rejection, never a crash.
            def bridge(call: dict[str, Any]) -> dict[str, Any]:
                result = fn(models.ToolCall.model_validate(call))
                return models.ToolResult.model_validate(result).model_dump()

            _ffi.register_tool(name, bridge, external)
            return fn

        return register

    def judge(
        self,
        config: Any,
        *,
        name: str = "default",
    ) -> Callable[[Callable[[Any], Any]], Callable[[Any], Any]]:
        """`@core.judge(config)` — register the OPTIONAL quality judge (US-26).

        Decorates an ordinary LLM callable (LlmRequest -> LlmReply — the P2c GLM
        adapter path, ADR-0005: no second model path). The hesmos.eval_judge
        bridge wraps it into the output-judging callable the executor invokes
        after a node's reply; the recorded judge.* attrs (prompt_hash /
        temperature / model_version) come from the config, so a session without
        this registration stays rules-only with zero judge evidence (AC3).

        Sessions snapshot the judge at OPEN: registering (or replacing) a judge
        after a session opened affects only future sessions — which is exactly
        the AC2 drift-comparison flow (two evals, two configs, attr diff).
        """

        def register(fn: Callable[[Any], Any]) -> Callable[[Any], Any]:
            if not callable(fn):
                raise FfiError(
                    "Ffi",
                    "FFI-CONTRACT",
                    None,
                    None,
                    "judge callback is not callable",
                    "register a function taking one LlmRequest argument (provider shape)",
                )
            from . import eval_judge  # local import keeps the cycle-free surface

            _ffi.register_judge(
                name,
                eval_judge.make_bridge(config, fn),
                config.prompt_hash,
                config.temperature,
                config.model_version,
            )
            return fn

        return register

    # --- PY-5 core.run ---

    def run(
        self,
        plan: Any,
        task: str | None = None,
        dry_run: bool = False,
    ) -> models.RunReceipt:
        """`core.run(plan, task=…, dry_run=False) -> RunReceipt` (PY-5).

        Opens the native session on first call (plan+task become the immutable
        identity) and executes the plan through the Python callbacks. The plan
        object must be the SAME one on every call (plan_hash is immutable after
        open — SS-04 rule 6; a different plan is FFI-STATE). dry_run=True verifies
        the wave schedule with zero node executions. A failed session is a
        receipt (final_state/reason), not an exception — the run's control flow
        completed. A closed Session stays closed: run() after close() is FFI-STATE
        (close is terminal core-side; a new session is a new Session object).
        """
        if self._closed:
            raise FfiError(
                "Ffi",
                "FFI-STATE",
                None,
                None,
                "session is closed — run() after close() is a state violation",
                "create a new hesmos.Session for a new session",
            )
        if self._native is None:
            self._native = _ffi.session_open(
                seed=self._seed,
                budget=self._budget_spec,
                team_id=self._team_id,
                system_prompt=self._system_prompt,
                plan=plan._raw,
                task=task,
            )
        raw = _ffi.session_run(self._native, plan._raw, task, dry_run)
        self._receipt = models.RunReceipt.model_validate(raw)
        return self._receipt

    # --- introspection (FFI-1 ⑥⑦) ---

    @property
    def session_id(self) -> str:
        """1급 필드 — the CLI-2/CLI-3 <session_id> (§5.7 naming convergence)."""
        return self._require_native().session_id

    @property
    def seed(self) -> int | None:
        """The session seed. User-specified seeds are visible before the first run;
        a generated seed only exists once the native session is open (it is minted
        core-side and recorded on session.open — PY-1 note)."""
        if self._seed is not None:
            return self._seed
        return self._require_native().seed

    def status(self) -> models.SessionHandle:
        """Full TYPE-5 snapshot — plan_hash ALWAYS present (consensus (a))."""
        native = self._require_native()
        return models.SessionHandle.model_validate(_ffi.session_status(native))

    def budget_status(self) -> models.BudgetState:
        """BudgetState snapshot; after a real run `session_spent` is the reconciled
        ledger total (US-20 AC3: ledger sums == llm.call event sums)."""
        native = self._require_native()
        return models.BudgetState.model_validate(_ffi.budget_status(native))

    def build_messages(self, stage: dict[str, Any], snapshot: dict[str, Any]) -> list[models.Message]:
        """PY-7 surface (skeleton): one user Message carrying task + snapshot.

        ponytail: the 3-layer breakpoint builder (stable/context/volatile
        placement, SS-18 rule 5) is WP-P2d and swaps this interior; the
        signature and the Message shape are already the contract surface.
        """
        native = self._require_native()
        return [
            models.Message.model_validate(m)
            for m in _ffi.build_messages(native, stage, snapshot)
        ]

    # --- lifecycle ---

    def close(self) -> None:
        """Idempotent. A never-opened Session has nothing to close (no artifacts
        were created); a dry-run-only Session retires as Running in its WAL — the
        same resumable state a suspended CLI session leaves."""
        if self._native is not None:
            _ffi.session_close(self._native)
            self._native = None
        self._closed = True

    def _require_native(self) -> Any:
        if self._native is None:
            raise FfiError(
                "Ffi",
                "FFI-STATE",
                None,
                None,
                "session is not open yet — open happens on the first run() call",
                "call core.run(plan, task=…) first (deferred open keeps §6.3 intact)",
            )
        return self._native
