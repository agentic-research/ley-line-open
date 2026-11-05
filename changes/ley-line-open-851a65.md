Section: Security

- **MCP dispatches only the tools it advertises, inside the Cloister groups
  the listener grants** (bead `ley-line-open-851a65`). `tools/call` used to
  dispatch any canonical daemon op, so `load` (which replaces the arena),
  `sheaf_update_topology` and `sheaf_reap` were callable over MCP although
  `tools/list` never offered them, and Cloister's group policy, written
  against the advertised tools, did not limit them. Now a name `tools/list`
  does not advertise is refused before dispatch with JSON-RPC error `-32601`
  and `data: {"reason": "not_advertised", "tool"}`. The new
  `--mcp-groups <a,b>` (`DaemonConfig::mcp_groups`, `mcp::McpGrant`) grants a
  daemon's MCP listeners exactly those groups of `server.json`'s
  `_meta.art.cloister/v1.groups[]`: a call outside the grant is refused with
  `-32001` and `data: {"reason": "not_granted", "tool", "group"}`,
  `tools/list` lists only the granted tools, and a name no group has refuses
  startup. Without the flag every group is granted, so advertised tools work
  as before. The `query` tool, which runs any SQL on the writer (`DROP`,
  `ATTACH`, `VACUUM INTO` a host path), moves out of the read-side `query`
  group into its own `sql` group in `server.json`, so a `query` grant is a
  read grant; Cloister configurations that route raw SQL through the `query`
  backend must add `sql` (making `query` itself read-only is
  `ley-line-open-b7e9a3`). Breaking for Rust callers: `mcp::spawn` and `mcp::spawn_uds` take
  the grant as a new last argument.
