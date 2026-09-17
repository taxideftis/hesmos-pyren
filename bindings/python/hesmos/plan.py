"""PY-2 hesmos.Plan.from_yaml — YAML interpretation happens in Rust only (표 2).

There is deliberately no Python YAML parser here: parse/reference errors are the
Rust compiler's verdicts relayed as hesmos.CompileError (US-02).

P0c scope: from_yaml returns the *pre-compile DSL surface* (stages/depends/profile
as written in the plan YAML, CE-01-level checks only). The compiled Plan struct with
resolved StageSpec defaults exists once WP-P1a joins — mirror validation switches on
then (see _model seam below).
"""

from __future__ import annotations

from typing import Any

from . import _ffi


class Plan:
    """Wrapper over the core-parsed plan; construct via from_yaml."""

    def __init__(self, raw: dict[str, Any]) -> None:
        # ponytail: holds the pre-compile DSL dict; swap to the generated Plan mirror
        # (TypeAdapter(PlanModel).validate_python) when WP-P1a's compile lands and
        # plan_from_yaml returns the compiled Plan struct — trigger: P1a completion.
        self._raw = raw

    @classmethod
    def from_yaml(cls, text: str) -> "Plan":
        raw: dict[str, Any] = _ffi.plan_from_yaml(text)
        return cls(raw)

    def __repr__(self) -> str:  # pragma: no cover - debug convenience
        stages = self._raw.get("stages") or []
        return f"Plan(stages={len(stages)})"
