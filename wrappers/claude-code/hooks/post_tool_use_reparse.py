#!/usr/bin/env python3
"""PostToolUse hook for the leyline-stale-sync Claude Code plugin.

Triggered after Edit/Write/MultiEdit/NotebookEdit. Extracts the file path
from `tool_input`, then fires `reparse(files=[<path>])` against the ley-
line-open daemon's MCP endpoint so the daemon re-runs its tree-sitter
parse pass and LSP enrichment on the edited file. Without this hook,
follow-up `lsp_hover` / `find_callers` / etc. queries on the same file
return stale data until the daemon's mtime poll catches up.

Wire contract (ADR-0022):
  POST http://localhost:8384/mcp
  Headers: x-leyline-token: <hex>
  Body: {"jsonrpc":"2.0", "id":1, "method":"tools/call",
         "params":{"name":"reparse", "arguments":{"files":[<path>]}}}

Token resolution: read from the OS-appropriate platform data dir:
  Linux:   $XDG_DATA_HOME/leyline/daemon.token  (default ~/.local/share/...)
  macOS:   ~/Library/Application Support/leyline/daemon.token
  Windows: %APPDATA%/leyline/daemon.token

Failure mode: this hook is best-effort. If the daemon is down, the port
isn't 8384, the token file isn't present, or anything else goes wrong,
the hook logs to stderr and exits 0 — Claude Code's edit flow must NOT
be blocked by a stale-sync hiccup. The next on-demand `enrich` request
will catch up via the daemon's own lazy enrichment path.

Configuration knobs (env vars):
  LEYLINE_MCP_URL    override the daemon URL (default: http://localhost:8384/mcp)
  LEYLINE_TOKEN_FILE override the token path (default: platform-appropriate)
  LEYLINE_HOOK_TIMEOUT  HTTP timeout seconds (default: 2)
  LEYLINE_HOOK_DEBUG=1  print debug info to stderr
"""

from __future__ import annotations

import json
import os
import platform
import sys
import urllib.error
import urllib.request
from pathlib import Path


def _debug(msg: str) -> None:
    if os.environ.get("LEYLINE_HOOK_DEBUG") == "1":
        print(f"[leyline-stale-sync] {msg}", file=sys.stderr)


def _platform_token_path() -> Path:
    """Resolve the platform-appropriate token path per ADR-0022."""
    system = platform.system()
    if system == "Darwin":
        return Path.home() / "Library" / "Application Support" / "leyline" / "daemon.token"
    if system == "Windows":
        appdata = os.environ.get("APPDATA")
        if appdata:
            return Path(appdata) / "leyline" / "daemon.token"
        return Path.home() / "AppData" / "Roaming" / "leyline" / "daemon.token"
    # Linux + other Unixes: XDG_DATA_HOME or ~/.local/share/
    xdg = os.environ.get("XDG_DATA_HOME")
    base = Path(xdg) if xdg else Path.home() / ".local" / "share"
    return base / "leyline" / "daemon.token"


def _resolve_token() -> str | None:
    override = os.environ.get("LEYLINE_TOKEN_FILE")
    path = Path(override) if override else _platform_token_path()
    try:
        return path.read_text().strip()
    except FileNotFoundError:
        _debug(f"token file not found at {path}; skipping reparse")
        return None
    except OSError as e:
        _debug(f"token file unreadable at {path}: {e}; skipping reparse")
        return None


def _extract_file_paths(tool_name: str, tool_input: dict) -> list[str]:
    """Extract file paths from Edit/Write/MultiEdit/NotebookEdit tool inputs.

    All four tools carry the absolute path in `file_path`. MultiEdit
    applies multiple edits to the SAME file, so it's still one path.
    NotebookEdit edits a single .ipynb. Returns a list (always length 1
    today) so the wire-shape is forward-compatible if a future tool
    edits multiple files in one call.
    """
    path = tool_input.get("file_path") or tool_input.get("notebook_path")
    if not path or not isinstance(path, str):
        _debug(f"no file_path in {tool_name} tool_input; skipping")
        return []
    return [path]


def _fire_reparse(files: list[str], token: str) -> None:
    url = os.environ.get("LEYLINE_MCP_URL", "http://localhost:8384/mcp")
    timeout = float(os.environ.get("LEYLINE_HOOK_TIMEOUT", "2"))
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "reparse", "arguments": {"files": files}},
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={
            "content-type": "application/json",
            "x-leyline-token": token,
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:  # noqa: S310 — localhost only
            _debug(f"reparse({files}) -> {resp.status}")
    except urllib.error.HTTPError as e:
        _debug(f"reparse HTTP {e.code}: {e.read()[:200]!r}")
    except urllib.error.URLError as e:
        _debug(f"reparse URL error (daemon down? port wrong?): {e}")
    except (TimeoutError, OSError) as e:
        _debug(f"reparse transport error: {e}")


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError as e:
        _debug(f"stdin JSON parse error: {e}; exiting 0 (don't block CC)")
        return 0

    tool_name = payload.get("tool_name", "")
    tool_input = payload.get("tool_input") or {}

    files = _extract_file_paths(tool_name, tool_input)
    if not files:
        return 0

    token = _resolve_token()
    if token is None:
        # Token file missing — either daemon not auth-enabled (`--mcp-no-auth`)
        # or never started. Either way, can't fire reparse; exit clean.
        return 0

    _fire_reparse(files, token)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
