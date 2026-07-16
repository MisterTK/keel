"""Real-SDK MCP stdio server fixture for ``test_farm_adk_composition.py``
(farm-gated, KEEL_ADAPTER_FARM=1 only). Built on the real ``mcp`` package's
``mcp.server.fastmcp.FastMCP`` — never a fake — fixed server name
``farm-fixture``, two tools:

* ``echo(text)`` — returns ``text`` unchanged; the happy-path leg.
* ``always_fails()`` — hard-crashes the process (``os._exit``) rather than
  raising a plain Python exception. This is deliberate, not an accident: the
  real MCP protocol swallows a tool's own business-logic exception into a
  *successful* ``CallToolResult(isError=True)`` (verified empirically —
  see ``test_farm_mcp.py``'s module docstring and
  ``test_raising_tool_yields_error_result_not_transport_exception``), which
  never reaches ADK's ``McpTool.run_async`` outer ``try/except`` at all — so
  a plain ``raise`` here would NOT exercise the
  ``_MCP_GRACEFUL_ERROR_HANDLING`` error-dict path
  (``keel.packs.adk_pack._is_mcp_error_dict``), only ADK's own (irrelevant to
  this fixture) ``isError`` telemetry hook. A genuine transport-level crash
  (this process dying mid-call) is what actually drives ADK's
  ``McpTool.run_async`` into its ``except McpError`` / ``except Exception``
  branches, which is the real scenario the module docs point at ("the 5-minute
  hang seen when Model Armor... returns a 403 mid-tool-call") and the one
  ``adk_pack._is_mcp_error_dict`` is built to classify. Confirmed in a scratch
  venv (google-adk==2.4.0 + mcp==1.28.1): a raised exception yields
  ``{"content": [...], "isError": True}`` (NOT the ``{"error": ...}`` shape);
  a process crash yields ``{"error": "MCP tool execution failed: Connection
  closed"}`` (the ``McpError`` branch) — exactly the shape
  ``_is_mcp_error_dict`` checks.

Runnable directly: ``python mcp_stdio_server.py`` (stdio transport).
"""

from __future__ import annotations

import os

from mcp.server.fastmcp import FastMCP

server = FastMCP("farm-fixture")


@server.tool()
def echo(text: str) -> str:
    return text


@server.tool()
def always_fails() -> str:
    # Hard process exit — no cleanup, no Python-level exception on this side
    # of the pipe — so the client observes a genuine transport crash rather
    # than a swallowed tool-level error. See module docstring.
    os._exit(1)


if __name__ == "__main__":
    server.run(transport="stdio")
