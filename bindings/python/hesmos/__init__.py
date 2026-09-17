"""hesmos — Python layer over the deterministic Rust core (PY-1..8 public surface).

P0c exposes the PY-1/PY-2 skeleton (Session/Budget/Plan) plus the PY-6 exception
hierarchy and the generated pydantic mirrors (hesmos.models). The native module
hesmos._ffi is built by maturin from crates/hesmos-ffi; all boundary data crosses
the FFI-1 immutable surface only (SS-16 rule 1 — no FFI bypass path exists).
"""

from __future__ import annotations

try:  # native extension built by maturin; absent before `maturin develop`
    from . import _ffi

    NATIVE_AVAILABLE = True
except ImportError:  # pragma: no cover - exercised only pre-build
    _ffi = None  # type: ignore[assignment]
    NATIVE_AVAILABLE = False

from .exceptions import CompileError, FfiError, HesmosError, ReasonError
from .models import BudgetEnvelope, Envelope, HandoffContract, TraceEvent  # noqa: F401
from .plan import Plan
from .prompt import (  # PY-7 (WP-P2d)
    BuiltPrompt,
    ChangeRequest,
    DeferredChangeGate,
    DuplicateTokenMeter,
    PromptBuilder,
)
from .session import Budget, Session

__all__ = [
    "Session",
    "Budget",
    "Plan",
    "HesmosError",
    "CompileError",
    "ReasonError",
    "FfiError",
    "BuiltPrompt",
    "ChangeRequest",
    "DeferredChangeGate",
    "DuplicateTokenMeter",
    "PromptBuilder",
    "NATIVE_AVAILABLE",
    "models",
    "prompt",
]

# `models` is exposed as a namespace (generated mirror); individual re-exports above
# exist only for type checkers — the contract names are Session/Budget/Plan.
from . import models  # noqa: E402  (after __all__ for readability of the surface)
