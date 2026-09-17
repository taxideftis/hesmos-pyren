"""PY-2 hesmos.Plan.from_yaml — YAML interpretation happens in Rust only (표 2).

There is deliberately no Python YAML parser here: parse errors are the Rust
compiler's verdicts relayed as hesmos.CompileError (US-02).

from_yaml is deliberately PRE-compile (CE-01 schema-parse + minimal shape): the
§6.3 contract plan carries no task field — the task enters at core.run() — so a
task-requiring compile could not run here. The FULL verdicts (CE-02..09) surface
at session_open, right after the task merge, and leave no session behind (US-04).
"""

from __future__ import annotations

from typing import Any

from . import _ffi


class Plan:
    """Wrapper over the core-parsed plan; construct via from_yaml."""

    def __init__(self, raw: dict[str, Any]) -> None:
        # Holds the pre-compile DSL dict. Deliberately NOT upgraded to the generated
        # Plan mirror: the mirror requires `task`, but the §6.3 flow passes a task-less
        # plan here — the wire identity that session_run re-checks is THIS raw dict.
        self._raw = raw

    @classmethod
    def from_yaml(cls, text: str) -> "Plan":
        raw: dict[str, Any] = _ffi.plan_from_yaml(text)
        return cls(raw)

    @property
    def raw(self) -> dict[str, Any]:
        """The pre-compile DSL dict (read-only view for helpers like tools.Toolbox)."""
        return self._raw

    def __repr__(self) -> str:  # pragma: no cover - debug convenience
        stages = self._raw.get("stages") or []
        return f"Plan(stages={len(stages)})"
