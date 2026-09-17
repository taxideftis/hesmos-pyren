"""The persistence filter — tainted content never reaches the store.

Verdict consumption, not re-judgment: entries carry the core-shaped taint field
({kind: Clean} | {kind: Tainted, source: {origin}}) that the core's marking and
the FFI boundary already enforce. Reading that field here is the PY-8 filter;
re-deriving taint from content inspection would be the forbidden string-scan
(PT-12) — the type mark is the only truth.

Store shape: append-only JSONL, one entry per line. Deliberately no deletion
API: persistence is evidence-adjacent, and a store that can forget undermines
the residual-0 guarantee the tests assert (rejections are simply never written).
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


class MemoryStore:
    """JSONL store with a persist-time taint filter."""

    def __init__(self, path: Path | str) -> None:
        self._path = Path(path)
        self._path.parent.mkdir(parents=True, exist_ok=True)

    def persist(self, entry: dict[str, Any]) -> dict[str, Any]:
        """Persists a mapping carrying a core-marked `taint` field.

        Returns a verdict record:
          {"ok": True}                            — written.
          {"ok": False, "reason": "unmarked"}     — no taint field (PT-12).
          {"ok": False, "reason": "tainted",
           "origin": <source origin>}             — Tainted: refused, 0 residual.

        An entry the core (or the FFI boundary) never marked is refused too: an
        absent mark is not a Clean bill — the filter refuses to guess.
        """
        taint = entry.get("taint")
        if not isinstance(taint, dict) or "kind" not in taint:
            return {"ok": False, "reason": "unmarked"}
        if taint["kind"] == "Tainted":
            origin = (taint.get("source") or {}).get("origin", "")
            return {"ok": False, "reason": "tainted", "origin": str(origin)}
        if taint["kind"] != "Clean":
            return {"ok": False, "reason": "unmarked"}
        with self._path.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(entry, sort_keys=True) + "\n")
        return {"ok": True}

    def entries(self) -> list[dict[str, Any]]:
        """Every persisted entry, in write order (the residual check reads this)."""
        if not self._path.exists():
            return []
        return [
            json.loads(line)
            for line in self._path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]

    def count(self) -> int:
        return len(self.entries())
