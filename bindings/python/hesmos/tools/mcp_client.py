"""Minimal MCP stdio client — the W5 tool-executor transport (story §3).

Dependency choice (Footprint Ladder, story §6): ZERO new dependencies. The MCP
spec's base transport is newline-delimited JSON-RPC 2.0 over stdio, which the
stdlib (subprocess + json) covers completely. The `mcp` SDK would pull pydantic
v1 compat shims and an async runtime for one transport we do not otherwise use;
revisit only when a second transport (SSE/WebSocket) or resource subscriptions
actually land.

Security stance (SS-20): an MCP server is ANOTHER PROCESS outside the session's
trust boundary — everything it returns is external data. The client therefore
marks every result Tainted with the server identity as origin; a caller that
forwarded results unmarked would be an unmarked import (the executor rejects
exactly that).
"""

from __future__ import annotations

import json
import subprocess
import threading
from typing import Any

# The date-based version string the MCP spec uses; the server may negotiate down.
_MCP_PROTOCOL_VERSION = "2025-06-18"


class McpError(RuntimeError):
    """A transport-level failure: server crash, bad response, or JSON-RPC error."""


class McpStdioClient:
    """Talks to one MCP server over stdio (spawn → initialize → tools/call).

    Not a general MCP surface: exactly the handshake + tool invocation the tool
    executor needs. Thread-safe via a request lock (requests are strictly
    sequential over one pipe).
    """

    def __init__(self, command: list[str], *, env: dict[str, str] | None = None) -> None:
        self._command = command
        self._env = env
        self._proc: subprocess.Popen[str] | None = None
        self._next_id = 0
        self._lock = threading.Lock()
        self.server_name = command[0] if command else "mcp-server"

    def start(self) -> str:
        """Spawns the server and performs the initialize handshake.

        Returns the server-reported name (used as the taint origin). The
        handshake is mandatory: calling tools on an uninitialized session is a
        protocol violation, so there is deliberately no skip path.
        """
        self._proc = subprocess.Popen(
            self._command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=self._env,
        )
        reply = self._request(
            "initialize",
            {
                "protocolVersion": _MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "hesmos-tools", "version": "0.1.0"},
            },
        )
        name = (reply.get("serverInfo") or {}).get("name", self.server_name)
        self.server_name = str(name)
        # The spec's initialized notification completes the handshake.
        self._notify("notifications/initialized", {})
        return self.server_name

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        """`tools/call` → a marked ToolResult dict (PY-4 shape).

        The result is ALWAYS Tainted (source origin = `mcp:<server>`): the
        server is another process, its output is external by construction.
        """
        if self._proc is None:
            raise McpError("server not started — call start() first")
        reply = self._request("tools/call", {"name": name, "arguments": arguments})
        content = reply.get("content") or []
        text = "\n".join(
            str(item.get("text", "")) for item in content if isinstance(item, dict)
        )
        return {
            "call_id": name,
            "content": text,
            "taint": {"kind": "Tainted", "source": {"origin": f"mcp:{self.server_name}"}},
        }

    def close(self) -> None:
        if self._proc is not None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=5)
            except subprocess.TimeoutExpired:  # pragma: no cover - stubborn server
                self._proc.kill()
            self._proc = None

    def __enter__(self) -> "McpStdioClient":
        self.start()
        return self

    def __exit__(self, *_exc: Any) -> None:
        self.close()

    # --- JSON-RPC plumbing -------------------------------------------------

    def _request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        with self._lock:
            self._next_id += 1
            request = {
                "jsonrpc": "2.0",
                "id": self._next_id,
                "method": method,
                "params": params,
            }
            self._send(request)
            while True:
                line = self._recv_line()
                message = json.loads(line)
                # Notifications/requests from the server are skipped: this client
                # solicits none and has no handlers to run (no code execution
                # from server input — skills/ owns the no-arbitrary-exec rule).
                if message.get("id") == self._next_id:
                    if "error" in message:
                        err = message["error"]
                        raise McpError(f"{method} failed: {err.get('message', err)}")
                    return message.get("result") or {}

    def _notify(self, method: str, params: dict[str, Any]) -> None:
        with self._lock:
            self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def _send(self, message: dict[str, Any]) -> None:
        assert self._proc is not None and self._proc.stdin is not None
        try:
            self._proc.stdin.write(json.dumps(message) + "\n")
            self._proc.stdin.flush()
        except (BrokenPipeError, OSError) as exc:
            raise McpError(f"server pipe closed during send: {exc}") from exc

    def _recv_line(self) -> str:
        assert self._proc is not None and self._proc.stdout is not None
        line = self._proc.stdout.readline()
        if not line:
            raise McpError("server closed stdout before replying")
        return line
