"""PY-6 exception hierarchy tests (api-contracts PY-6 verify: FFI_* full parent chain).

Import strategy for all tests/python: the installed package (maturin develop) — no
sys.path injection, so source and installed copies can never shadow each other.
"""

from hesmos.exceptions import (
    FFI_CALLBACK,
    FFI_CONTRACT,
    FFI_SCHEMA,
    FFI_STATE,
    CompileError,
    FfiError,
    HesmosError,
    ReasonError,
)
from hesmos.models import REASON_CODES


def test_all_exceptions_share_hesmoserror_parent():
    # PY-6 verify: "FFI_* 전 부모 체인 확인" — every subclass inherits HesmosError.
    for exc in (CompileError, ReasonError, FfiError):
        assert issubclass(exc, HesmosError)


def test_positional_order_matches_py6_contract():
    # (class, code, session_id?, node_id?, message, hint?) — Rust raises with this order.
    err = FfiError("Ffi", FFI_SCHEMA, None, None, "bad payload", "check models")
    assert err.error_class == "Ffi"
    assert err.code == FFI_SCHEMA
    assert err.message == "bad payload"
    assert err.hint == "check models"


def test_reason_error_rejects_unknown_codes():
    # Reason codes are closed vocabulary (SS-08 rule 1) — a non-code must not construct.
    try:
        ReasonError("Reason", "NOT_A_CODE", None, None, "m")
    except ValueError:
        pass
    else:
        raise AssertionError("unknown reason code accepted")


def test_all_six_reason_codes_construct():
    for code in REASON_CODES:
        ReasonError("Reason", code, None, None, "m")


def test_ffi_code_constants_are_the_documented_four():
    assert {FFI_SCHEMA, FFI_CONTRACT, FFI_CALLBACK, FFI_STATE} == {
        "FFI-SCHEMA",
        "FFI-CONTRACT",
        "FFI-CALLBACK",
        "FFI-STATE",
    }
