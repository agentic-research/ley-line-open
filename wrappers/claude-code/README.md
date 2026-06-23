# leyline-stale-sync — Claude Code plugin

A small Claude Code plugin that keeps `ley-line-open`'s parse + LSP cache
in sync with the files Claude Code edits in your session.

**Problem it solves:** when Claude Code edits a file, the LLO daemon's
`_lsp_*` tables stay stale until the daemon's mtime poll catches up.
Follow-up queries (`lsp_hover`, `find_callers`, `lsp_diagnostics`) on
the just-edited file return data from before the edit. In long sessions
this surfaces as confusing "the function I just edited isn't showing
its new signature" moments.

**What it does:** a `PostToolUse` hook fires after every `Edit` / `Write`
/ `MultiEdit` / `NotebookEdit`. The hook extracts the edited file path,
then POSTs `reparse(files=[path])` to the LLO daemon's MCP endpoint at
`http://localhost:8384/mcp` with the standard `x-leyline-token` header
(ADR-0022). The daemon re-runs its tree-sitter parse pass + LSP
enrichment on that file. Next query returns fresh data.

The hook is best-effort: if the daemon is down, the token file is
missing, or anything else goes wrong, the hook logs to stderr and exits
clean — Claude Code's edit flow is never blocked.

## What's in the plugin

```
wrappers/claude-code/
├── .claude-plugin/
│   ├── plugin.json              — manifest (Anthropic-format)
│   └── hooks/hooks.json         — PostToolUse hook registration
├── hooks/
│   └── post_tool_use_reparse.py — the hook implementation
├── .mcp.json                    — optional MCP server declaration for CC
│                                  (lets the agent call LLO's lsp_* /
│                                  find_callers / etc. directly)
└── README.md
```

## Install

The plugin is shipped as part of the `ley-line-open` repo. To enable it
in your Claude Code session, point Claude Code at this directory:

```bash
# From inside any project where you want LLO stale-sync:
claude mcp add-from-plugin --plugin-dir /path/to/ley-line-open/wrappers/claude-code
```

Or copy the directory into your project's `.claude/plugins/` folder, or
load it via your `~/.claude/settings.json` `plugins` configuration —
whichever matches your Claude Code version's plugin-loading convention.

### MCP server (optional but recommended)

The `.mcp.json` declares LLO as an MCP server CC can route `lsp_*`,
`find_callers`, `find_defs`, `inspect_symbol` (when shipped per ADR-0016
L1), etc. through. It uses an env var for the token:

```bash
export LEYLINE_TOKEN="$(cat ~/.local/share/leyline/daemon.token)"  # Linux
# macOS:
export LEYLINE_TOKEN="$(cat ~/Library/Application\ Support/leyline/daemon.token)"
```

(The token file is generated automatically the first time you run
`leyline daemon` per ADR-0022.)

If you'd rather not maintain the env var, you can omit `.mcp.json` from
your CC config — the hook still works without it, since the hook
resolves the token from disk directly.

## Configuration

Environment variables (all optional):

| Var | Default | Purpose |
|---|---|---|
| `LEYLINE_MCP_URL` | `http://localhost:8384/mcp` | Override daemon URL |
| `LEYLINE_TOKEN_FILE` | platform-appropriate | Override token path |
| `LEYLINE_HOOK_TIMEOUT` | `2` (seconds) | HTTP timeout |
| `LEYLINE_HOOK_DEBUG` | unset | Set to `1` to log hook activity to stderr |

Token paths (per ADR-0022, resolved by the hook automatically):
- Linux: `$XDG_DATA_HOME/leyline/daemon.token` (default `~/.local/share/leyline/daemon.token`)
- macOS: `~/Library/Application Support/leyline/daemon.token`
- Windows: `%APPDATA%/leyline/daemon.token`

## Debugging

To see what the hook is doing, run Claude Code with the debug flag set:

```bash
LEYLINE_HOOK_DEBUG=1 claude
```

Then edit any file. You should see lines like:

```
[leyline-stale-sync] reparse(['/path/to/edited.go']) -> 200
```

Common diagnostic lines:

- `token file not found at <path>; skipping reparse` — the daemon hasn't
  been started yet (or you're running with `--mcp-no-auth`). Run
  `leyline daemon` once to bootstrap the token.
- `reparse URL error (daemon down? port wrong?)` — the daemon isn't
  reachable. Check `leyline status` or that `LEYLINE_MCP_URL` matches
  the port you started the daemon on.
- `reparse HTTP 401` — the token in the file doesn't match the one the
  daemon is using. Restart the daemon, or copy the token file to the
  right location.

## What this plugin is NOT

- **It's not an LSP client.** The hook fires `reparse` against LLO's
  MCP `tools/call` surface, not LSP `textDocument/didChange` against
  LLO's internal language servers. LLO doesn't expose an LSP wire
  today (the `leyline lsp` shim from ADR-0016 §8 is deferred). The
  `reparse` op is the right primitive: it covers tree-sitter +
  LSP-enrichment in one call.
- **It doesn't route through cloister.** A future `LEYLINE_MCP_URL`
  pointing at a cloister gateway (`https://cluster.example.com/...`)
  could route the call remotely, but the cloister-routed mode is opt-
  in config only; the cloister side is already shipped (`cloister-
  ac8bcf`).

## Related

- ADR-0022 (MCP wire auth) — defines the token contract this hook uses
- ADR-0015 (lazy-on-access ingestion) — explains why mtime poll alone
  isn't enough for a long agent session
- Bead `cloister-acbf27` — the parent ask for this plugin
- Bead `cloister-ac8bcf` (closed) — the cloister-side `lsp_*` mcpProxy
  registration, which is already live

## License

AGPL-3.0-or-later (matches `ley-line-open`).
