"""hesmos.skills — declarative skill loading + signature gating (PY-8, SS-21).

Two load paths, one rule: NO code executes at load time.

- [`load`] accepts DECLARATIVE manifests only (name/description/instructions —
  the agentskills.io shape the story accepts). An allowlist decides: any other
  key is a rejection, because a denylist cannot know tomorrow's exec vector.
- [`load_signed`] adds the signature gate: the package runs (its instructions
  are released for use) ONLY after HMAC verification passes. In the declarative
  world "execution" = applying the instructions; a signature authorizes trust,
  it never reintroduces arbitrary code.

Rejections are returned as records ({"ok": False, ...}) — evidence, not
exceptions. The trace-side recording (tool.call(ok=false, tool_name)) happens
when a rejection crosses a registered tool callback: the callback raises, and
the FFI executor records the failure (W3-confirmed path — see
hesmos.tools.executor.skill_guard_tool for the pattern).
"""

from .manifest import load, load_signed, sign_manifest

__all__ = ["load", "load_signed", "sign_manifest"]
