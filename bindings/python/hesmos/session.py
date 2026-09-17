"""PY-1 Session/Budget — the FFI session handle wrapper (api-contracts PY-1).

Session holds no orchestration state of its own (SS-16 rule 1): it wraps the native
session handle, and everything mutable lives core-side behind the FFI-1 surface.
`seed=None` lets the core generate and record the seed — same rule as CLI-1 --seed.
"""

from __future__ import annotations

from typing import Any

from . import _ffi


class Budget:
    """Session budget spec in the CLI-1 --budget form (tokens[, cost_usd]).

    The envelope itself is fixed core-side at session open (SS-15 rule 1) — this
    object only carries the user-facing spec across the boundary.
    """

    def __init__(self, tokens: int, cost_usd: float | None = None) -> None:
        self.spec: dict[str, Any] = {"tokens": tokens}
        if cost_usd is not None:
            self.spec["cost_usd"] = cost_usd

    def __repr__(self) -> str:  # pragma: no cover - debug convenience
        return f"Budget({self.spec!r})"


class Session:
    """Entry point: `hesmos.Session(seed=42, budget=hesmos.Budget(tokens=250_000))`."""

    def __init__(
        self,
        seed: int | None = None,
        budget: Budget | None = None,
        team_id: str | None = None,
    ) -> None:
        raw = _ffi.session_open(
            seed=seed,
            budget=budget.spec if budget is not None else None,
            team_id=team_id,
        )
        # Frozen view of the core-side handle; Python never owns the state itself.
        self._handle: dict[str, Any] = raw

    @property
    def session_id(self) -> str:
        return self._handle["session_id"]

    @property
    def seed(self) -> int:
        return self._handle["seed"]

    def close(self) -> None:
        _ffi.session_close(self._handle)
