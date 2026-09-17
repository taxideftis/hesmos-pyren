"""Reference tool implementations + the plan-allowlist registration helper.

`Toolbox` is the boring glue between a compiled plan and the PY-4 registry: it
reads the plan's per-node tool allowlists and refuses to register a tool no
node's profile carries. That is a REGISTRATION-side convenience sharing the same
allowlist data the permission gate enforces — the gate itself (core-side,
pre-execute) remains the only permission authority (SS-19 rule 3); this check
cannot weaken it, only surface a wiring mistake earlier.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable


class Toolbox:
    """Registers tool callbacks scoped to a plan's tool allowlists."""

    def __init__(self, core: Any, plan: Any) -> None:
        self._core = core
        self._allowlist = _plan_toolsets(plan)

    @property
    def allowlist(self) -> set[str]:
        return set(self._allowlist)

    def register(self, name: str, fn: Callable[[Any], Any], *, external: bool = False) -> None:
        """Register `fn` under `name` — refuses tools outside the plan's allowlist.

        The refusal is a wiring error (the plan never grants this tool), raised
        at registration time where the mistake is cheap to fix; dispatch-time
        permission remains the gate's job.
        """
        if name not in self._allowlist:
            raise ValueError(
                f"tool `{name}` is not in the plan's tool allowlist — "
                "add it to the granting node's profile first (SS-19)"
            )
        self._core.tool(name, external=external)(fn)


def _plan_toolsets(plan: Any) -> set[str]:
    """Collects every tool named in the plan's node profiles (`tools:` lists).

    Accepts a hesmos.Plan (its `.raw` DSL dict) or the raw dict itself — the
    pre-compile shape session_open receives, so the allowlist here is by
    construction the same data the gate judges.
    """
    raw = getattr(plan, "raw", None)
    raw = raw if isinstance(raw, dict) else plan
    tools: set[str] = set()
    for stage in raw.get("stages") or []:
        profile = (stage or {}).get("profile") or {}
        tools.update(str(t) for t in profile.get("tools") or [])
    return tools


def register_reference_tools(core: Any, notes: dict[str, str] | None = None) -> dict[str, str]:
    """Registers the canonical marking-convention tools used by tests/examples.

    - `local.note`: a LOCAL tool — its reads never cross the trust boundary, so
      its ToolResult is Clean.
    - `web.fetch`: an EXTERNAL tool (registered external=True) — everything it
      returns crossed the boundary, so its ToolResult is Tainted with the read
      origin. Returning Clean from here would be an unmarked import and the
      executor rejects the result (SS-20 rule 1).

    Returns the tool names for assertions. `notes` optionally receives the
    local-note store (a plain dict — no persistence; persistence with taint
    filtering lives in hesmos.memory).
    """
    store: dict[str, str] = notes if notes is not None else {}

    @core.tool("local.note")
    def local_note(call: Any) -> dict[str, Any]:
        store[call.arguments["key"]] = call.arguments["value"]
        return {
            "call_id": call.id,
            "content": f"noted {call.arguments['key']}",
            "taint": {"kind": "Clean"},
        }

    @core.tool("web.fetch", external=True)
    def web_fetch(call: Any) -> dict[str, Any]:
        return {
            "call_id": call.id,
            "content": f"external document for {call.arguments['url']}",
            "taint": {"kind": "Tainted", "source": {"origin": "web.fetch"}},
        }

    return {"local": "local.note", "external": "web.fetch", "store": store}


def skill_guard_tool(core: Any, name: str, skill_path: Path) -> Callable[[Any], Any]:
    """The author pattern for a tool that loads skills (story AC ⑤).

    A rejection record must reach the trace as tool.call(ok=false) — the W3-
    confirmed path. The executor records ok=false exactly when the callback
    raises, so the guard RAISES on rejection instead of returning a record.
    """

    from .. import skills  # local import: keeps hesmos.skills import cost at call time

    @core.tool(name)
    def load_skill(call: Any) -> dict[str, Any]:
        record = skills.load(skill_path)
        if not record["ok"]:
            raise RuntimeError(f"skill rejected: {record['reason']}: {record['detail']}")
        import json as _json

        return {
            "call_id": call.id,
            "content": _json.dumps(record["manifest"]),
            "taint": {"kind": "Clean"},
        }

    return load_skill
