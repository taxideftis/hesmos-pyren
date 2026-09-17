"""Declarative manifest loading + the HMAC signature gate (SS-21).

Algorithm choice (story §6: "알고리즘은 구현 시 확정 — boring 선택"):
HMAC-SHA256 over the manifest bytes via the STDLIB hmac module — zero new
dependencies, and `hmac.compare_digest` gives a constant-time comparison for
free. The known cost is key distribution: symmetric keys mean every verifier
holds the signing key. When asymmetric signing becomes a requirement (public
verify keys, per-author identity), the upgrade path is ed25519 via pynacl —
one dependency, swap confined to [`sign_manifest`]/[`_verify`].
"""

from __future__ import annotations

import hashlib
import hmac
import json
import os
from pathlib import Path
from typing import Any

# The allowlist IS the security boundary for loading: a manifest is trusted to
# carry only inert description fields. Anything else — today "script"/"code",
# tomorrow whatever the format grows — is rejected by not being listed.
ALLOWED_KEYS = frozenset({"name", "description", "instructions", "version"})
REQUIRED_KEYS = frozenset({"name", "description", "instructions"})

# Signing key is NEVER hardcoded (keystore rule): callers pass it or point
# HESMOS_SKILL_SIGNING_KEY at it.
SIGNING_KEY_ENV = "HESMOS_SKILL_SIGNING_KEY"


def sign_manifest(manifest_bytes: bytes, key: str) -> str:
    """Hex HMAC-SHA256 of the exact manifest bytes (sign what you ship)."""
    return hmac.new(key.encode("utf-8"), manifest_bytes, hashlib.sha256).hexdigest()


def load(path: Path | str) -> dict[str, Any]:
    """Loads a declarative skill manifest. Zero code execution, by construction.

    Returns {"ok": True, "manifest": {...}} or {"ok": False, "reason", "detail"}.
    Rejections are records so callers (and the tool path) can carry them as
    evidence instead of switching on exception types.
    """
    manifest_path = Path(path)
    if manifest_path.is_dir():
        manifest_path = manifest_path / "manifest.json"
    try:
        raw = manifest_path.read_bytes()
    except OSError as exc:
        return {"ok": False, "reason": "unreadable", "detail": str(exc)}
    try:
        manifest = json.loads(raw)
    except json.JSONDecodeError as exc:
        return {"ok": False, "reason": "invalid_json", "detail": str(exc)}
    if not isinstance(manifest, dict):
        return {"ok": False, "reason": "not_a_mapping", "detail": "manifest must be a JSON object"}
    unknown = sorted(set(manifest) - ALLOWED_KEYS)
    if unknown:
        return {
            "ok": False,
            "reason": "executable_content",
            "detail": f"manifest carries non-declarative keys: {', '.join(unknown)}",
        }
    missing = sorted(REQUIRED_KEYS - set(manifest))
    if missing:
        return {
            "ok": False,
            "reason": "incomplete",
            "detail": f"manifest missing required keys: {', '.join(missing)}",
        }
    instructions = manifest["instructions"]
    if not isinstance(instructions, list) or not all(
        isinstance(step, str) for step in instructions
    ):
        return {
            "ok": False,
            "reason": "invalid_instructions",
            "detail": "instructions must be a list of strings (declarative steps)",
        }
    return {"ok": True, "manifest": manifest}


def load_signed(pkg: Path | str, key: str | None = None) -> dict[str, Any]:
    """Loads a signed skill package: `pkg/manifest.json` + `pkg/manifest.sig`.

    The signature is verified BEFORE the manifest is even parsed as trusted
    content; a failed verification returns a rejection and nothing is released
    for execution. The key comes from the argument or HESMOS_SKILL_SIGNING_KEY.
    """
    pkg_dir = Path(pkg)
    manifest_path = pkg_dir / "manifest.json"
    sig_path = pkg_dir / "manifest.sig"
    signing_key = key if key is not None else os.environ.get(SIGNING_KEY_ENV, "")
    if not signing_key:
        return {
            "ok": False,
            "reason": "no_signing_key",
            "detail": f"pass a key or set {SIGNING_KEY_ENV} (keys are never hardcoded)",
        }
    if not sig_path.exists():
        return {"ok": False, "reason": "unsigned", "detail": f"{sig_path} missing"}
    try:
        raw = manifest_path.read_bytes()
        expected = sig_path.read_text(encoding="utf-8").strip()
    except OSError as exc:
        return {"ok": False, "reason": "unreadable", "detail": str(exc)}
    if not _verify(raw, expected, signing_key):
        return {
            "ok": False,
            "reason": "signature_mismatch",
            "detail": "manifest.sig does not verify against the signing key",
        }
    record = load(manifest_path)
    if record["ok"]:
        record["signed"] = True
    return record


def _verify(raw: bytes, expected_hex: str, key: str) -> bool:
    actual = sign_manifest(raw, key)
    return hmac.compare_digest(actual, expected_hex)
