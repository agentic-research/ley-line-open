Section: Changed

- **The documentation matches the v0.26.0 code** (bead `ley-line-open-a678c2`).
  `GETTING-STARTED.md` names the control socket the daemon binds,
  `~/.mache/default.sock` (it said `default.ctrl.sock`), its `_ast` example
  query joins `kinds` (it read an `_ast.kind` column that does not exist), and
  it lists the default `sheaf` feature and all 28 built-in languages.
  `AGENTS.md` lists every identity LLO ships with its construction: the head
  digest v3, the keyed `node_hash`, `run_id` (v1; v2 is proposed), the rootfs
  digest, and the arena-local locators, which are not identities.
  `docs/TABLE_CONTRACT.md` gives the actual columns of `nodes`, `_source`,
  `_cfg` and the `_lsp*` tables and lists `query_blobs`/`_queries`;
  `docs/ARCHITECTURE.md`'s ADR table gives each ADR's own status; and
  `docs/ROADMAP.md` records what shipped through v0.26.0 and in v0.27.0.
