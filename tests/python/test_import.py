"""T6 AC1 acceptance: `import hesmos` succeeds with the native extension (WP-P0c gate).

Hard requirement (no skip): this is the P0 completion gate from build-plan §3 —
"hesmos-py 임포트". A missing native build must fail loudly here.
"""

import hesmos
from hesmos import Plan, Session  # noqa: F401  (PY-1/PY-2 public surface)


def test_import_hesmos_with_native_extension():
    assert hesmos.NATIVE_AVAILABLE, "native hesmos._ffi missing — run: maturin develop"


def test_public_surface_exports_py1_py2():
    assert callable(hesmos.Session)
    assert callable(hesmos.Budget)
    assert callable(Plan.from_yaml)
    # PY-6 exceptions are public API surface.
    for name in ("HesmosError", "CompileError", "ReasonError", "FfiError"):
        assert hasattr(hesmos, name)
