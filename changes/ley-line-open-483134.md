Section: Breaking

- **A workspace grant names where the run may write** (bead `ley-line-open-483134`). `execution/v1` `WorkspaceGrant` gains `writeScope @4 :List(Text)`: exact file paths and `/`-terminated directory trees, relative to the workspace root, or the single entry `/` for the whole workspace. Before, a grant with `mutate` or `commit` could write anywhere in its workspace.
  - **A writing grant must name a scope.** Authorization refuses, as `invalidSpec` naming the workspace, a grant that permits `mutate` or `commit` with an empty `writeScope`, and a grant that names a scope while permitting neither. A grant issued before this field existed is refused; reissue it with `writeScope: ["/"]` to keep its old meaning. The field is additive on the wire, so an older reader ignores it.
  - **Entries are literal.** No leading `/`, no empty, `.` or `..` component, no NUL, and no entry twice.
  - **Enforced by the share.** The store-backed share maps the scope to `source_write::Grant::Scoped`. A write, create, rename or delete outside it is the typed `outside_grant` refusal, `EACCES` in the guest, and lands nothing. A rename needs both names inside the scope. Reads are unchanged: `read` or `list` shows the whole tree at `graphRoot`.
  - `leyline_storefs::scope_for` takes the `WorkspaceGrant`, not its operations. `leyline_runtime::WorkspaceGrant` gains `write_scope`.
