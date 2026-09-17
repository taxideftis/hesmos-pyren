"""hesmos.tools — the tool executor layer (WP-P2e, SS-19/SS-21).

The tool layer's jobs, in the order the runner's design fixes them:

- Permission is decided ONCE, core-side, before any node executes (SS-19 rule 3:
  the permission gate checks the node's toolset against the grantor profile).
  Nothing here re-judges permissions — a re-check would be a second, divergent
  policy and is forbidden by the story's design notes.
- Dispatch is mechanical: the FFI executor (crates/hesmos-ffi) routes gate-passed
  tool_calls to registered callbacks, records every outcome as tool.call events,
  and enforces the taint boundary (PT-12: every ToolResult is marked; SS-20 rule
  1: an external tool returning Clean is an unmarked import and is rejected).
- The reference registrations here ([`register_reference_tools`]) exist so tests
  and adopters have canonical examples of the two marking conventions: a local
  tool returns Clean, an external tool returns Tainted with its read origin.
"""

from .executor import Toolbox, register_reference_tools
from .mcp_client import McpError, McpStdioClient

__all__ = ["Toolbox", "register_reference_tools", "McpStdioClient", "McpError"]
