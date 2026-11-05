Section: Breaking

- **Arena-local nids no longer reach the daemon wire, and `TreeLeaf.mode`
  is a `GitMode`** (bead `ley-line-open-80f037`, findings F3 and F8).
  - `lsp_symbols` and `lsp_diagnostics` rows no longer carry `nid`. Each
    row carries the display `node_id` it already had; `nid` was the row's
    key inside one arena and named nothing outside it.
  - `find_callers`, `find_defs`, `find_callees`, `get_refs_map`,
    `get_defs_map`, `lsp_hover`, `lsp_symbols` and `lsp_diagnostics` render a node with no
    tree path as `{source_id}#inj@{ordinal}` as before, and now refuse a
    node whose file has no `_source` row instead of emitting its decimal
    nid as `node_id`. `lsp_hover` also propagates a path-render error it
    used to swallow. mache reads `node_id` as a path and never parsed the
    decimal form.
  - `leyline_cli_lib::tree_root::TreeLeaf.mode` is
    `leyline_cli_lib::tree::GitMode` (`Regular`, `Executable`, `Symlink`,
    `Directory`) instead of `u32`; `GitMode::from_git` is the one parse
    from the integer, and the `_tree` and checkout-index reads go through
    it. A mode git does not store can no longer reach the fold, where mode
    `0` folded exactly as a regular file. The SQL column `_tree.mode` is
    unchanged.
