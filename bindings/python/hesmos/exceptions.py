"""PY-6 exception hierarchy (api-contracts PY-6, exceptions.md §1/§7).

Single parent HesmosError with the three Python-visible subclasses. Vocabulary matches
exceptions.md exactly — reason codes are the 6 fixed codes, compile errors are CE-*,
FFI boundary rejections are FFI-* (FFI-SCHEMA/FFI-CONTRACT/FFI-CALLBACK/FFI-STATE).

Never swallow these: FFI-CALLBACK must be converted to PROVIDER_FAILURE upstream, not
hidden (exceptions.md §7 — 삽킴 금지).

Note: the contract spells the discriminator field `class`, which is a Python keyword;
the constructor keyword and attribute are `error_class` (documented deviation).
"""

from __future__ import annotations

from hesmos.models import REASON_CODES

# FFI boundary error codes (exceptions.md §7) — the only FFI codes in the system.
FFI_SCHEMA = "FFI-SCHEMA"  # serde<->pydantic dual-validation failure at the boundary
FFI_CONTRACT = "FFI-CONTRACT"  # unregistered callback signature / off-contract call
FFI_CALLBACK = "FFI-CALLBACK"  # Python callback itself raised (converted, not swallowed)
FFI_STATE = "FFI-STATE"  # state manipulation outside the immutable API


class HesmosError(Exception):
    """Parent of every Hesmos error. Carries the TYPE-7 HesmosError fields.

    Positional order follows the PY-6 contract sketch: (class, code, session_id?,
    node_id?, message, hint?). The Rust FFI raises these classes directly with that
    order, so keep it stable.
    """

    def __init__(
        self,
        error_class: str = "Reason",
        code: str = "",
        session_id: str | None = None,
        node_id: str | None = None,
        message: str = "",
        hint: str | None = None,
    ) -> None:
        super().__init__(message)
        self.error_class = error_class
        self.code = code
        self.message = message
        self.session_id = session_id
        self.node_id = node_id
        self.hint = hint


class CompileError(HesmosError):
    """class=Compile — plan/suite errors before any session exists (CE-01..09, exit 3)."""


class ReasonError(HesmosError):
    """class=Reason — runtime termination with one of the 6 reason codes."""

    def __init__(
        self,
        error_class: str = "Reason",
        code: str = "",
        session_id: str | None = None,
        node_id: str | None = None,
        message: str = "",
        hint: str | None = None,
    ) -> None:
        # Validate before construction: a non-code must never become a raisable error
        # (SS-08 rule 1 — closed vocabulary; new reasons need an exceptions.md revision).
        if code not in REASON_CODES:
            raise ValueError(
                f"{code!r} is not a reason code; allowed: {REASON_CODES}"
                " (exceptions.md — new reasons require an exceptions.md revision)"
            )
        super().__init__(error_class, code, session_id, node_id, message, hint)


class FfiError(HesmosError):
    """class=Ffi — boundary rejection; core state remains untouched (FFI-*)."""
