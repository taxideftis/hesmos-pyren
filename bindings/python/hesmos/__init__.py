"""hesmos — Python layer over the deterministic Rust core (PY-1..8 public surface).

P2b exposes the completed PY-1..5 surface (Session/Budget/Plan with run + provider/
tool decorators), the PY-6 exception hierarchy, and the generated pydantic mirrors
(hesmos.models). The native module hesmos._ffi is built by maturin from
crates/hesmos-ffi; all boundary data crosses the FFI-1 immutable surface only
(SS-16 rule 1 — no FFI bypass path exists).
"""

from __future__ import annotations

try:  # native extension built by maturin; absent before `maturin develop`
    from . import _ffi

    NATIVE_AVAILABLE = True
except ImportError:  # pragma: no cover - exercised only pre-build
    _ffi = None  # type: ignore[assignment]
    NATIVE_AVAILABLE = False

from .exceptions import CompileError, FfiError, HesmosError, ReasonError
from .models import (  # noqa: F401
    BudgetEnvelope,
    BudgetState,
    Envelope,
    HandoffContract,
    LlmReply,
    LlmRequest,
    RunReceipt,
    ToolCall,
    ToolResult,
    TraceEvent,
)
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
    # PY-3/PY-4/PY-5 wire + result types (contract users annotate with these).
    "LlmRequest",
    "LlmReply",
    "ToolCall",
    "ToolResult",
    "RunReceipt",
    "BudgetState",
    "BuiltPrompt",
    "ChangeRequest",
    "DeferredChangeGate",
    "DuplicateTokenMeter",
    "PromptBuilder",
    "NATIVE_AVAILABLE",
    "models",
    "prompt",
    "eval_judge",
]

# `models` is exposed as a namespace (generated mirror); individual re-exports above
# exist only for type checkers — the contract names are Session/Budget/Plan.
from . import models  # noqa: E402  (after __all__ for readability of the surface)
from . import memory, skills, tools  # noqa: E402  (WP-P2e surface namespaces)
from . import eval_judge  # noqa: E402  (WP-P3b judge surface — Session.judge's config)
