Section: Changed

- **The daemon's LSP row mappers no longer carry a bare `nid` in JSON**
  (bead `ley-line-open-765bd5`). `lsp_symbols` and `lsp_diagnostics` rows
  were built as JSON holding the arena-local `nid` as an integer, stripped
  before the reply left. The row's `nid` is now read as a typed `Nid`
  beside the reply fields and rendered as `node_id`; the JSON never holds
  it, and `Nid` has no serde encoding to put it there. `list_children` no
  longer selects a `nid` it discarded. The wire is unchanged.
