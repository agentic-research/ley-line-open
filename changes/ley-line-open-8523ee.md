Section: Breaking

- **Six dead daemon ops are removed: `flush`, `list_roots`, `sheaf_stalks`,
  `get_schema`, `get_refs_map` and `get_defs_map`** (bead
  `ley-line-open-8523ee`). None has a caller in LLO, mache or ley-line at
  their `origin/main`. `flush` read `current_root`, which `status` returns,
  changed nothing, and still emitted a `daemon.flush` event; `list_roots` was
  `list_children` with `id: ""`; `sheaf_stalks` returned a strict subset of
  `sheaf_status`; `get_schema` returned a hardcoded crate list; mache builds
  its refs and defs maps from its own index. The wire refuses each name as an
  unknown op, the MCP registry and its Cloister groups drop the four that were
  tools, and `daemon.capnp` drops `FlushRequest`/`FlushResponse`,
  `TokenMapEntry`, `GetRefsMap*`, `GetDefsMap*`, `SchemaTier`, `GetSchema*`
  and `SheafStalksResponse`, with the matching Go bindings in
  `clients/go/leyline-schema` (`wire.FlushResponse`, `wire.TokenMapEntry`,
  `wire.GetRefsMapResponse`, `wire.GetDefsMapResponse`, `wire.SchemaTier`,
  `wire.GetSchemaResponse`, `wire.SheafStalksResponse`). Migrate `flush` to
  `status`, `list_roots` to `list_children`, and `sheaf_stalks` to
  `sheaf_status`. `get_db_path` stays: open bead `ley-line-open-cc386f` plans
  mache's `udsGraph` use of it.
