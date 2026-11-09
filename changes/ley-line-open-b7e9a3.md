Section: Security

- **The daemon's `query` op is read-only** (bead `ley-line-open-b7e9a3`).
  It ran its SQL on the living db's writer, so any caller of the control
  socket, or of MCP with the `sql` group, could `DROP` or rewrite tables,
  `ATTACH` a host file, or `VACUUM INTO` a host path (which writes even on a
  `query_only` connection). The statement now runs on a reader-pool
  connection (`query_only=ON`) under a SQLite authorizer that denies by
  default: it allows `SELECT`, reading a table, recursive CTEs, a listed set
  of pure functions (`load_extension`, `sqlite_log` and the connection-state
  functions are not among them) and the table-valued schema-introspection
  pragmas (`pragma_table_info` and kin). The statement must also be a single
  one that SQLite reports as a read-only `SELECT`. An `ATTACH` whose file
  name is an expression, which reaches the authorizer as an action rusqlite
  does not name and which SQLite reports read-only, is refused by the
  deny-by-default rule alone. A refused statement returns `ok: false`, an
  `error` message, and a typed `refusal` (`QueryResponse.refusal` in
  `daemon.capnp`, additive): `reason` is `denied_action`,
  `multiple_statements` or `not_a_query`, `action` names the refused action
  (`insert`, `delete`, `attach`, `pragma`, `function`, ..., or the
  statement's keyword such as `vacuum`), and `subject` what it names. A DROP
  or CREATE TABLE is refused as the schema-table write SQLite asks for first
  (`delete` or `insert` on `sqlite_master`). It is refused before any row is
  returned and changes nothing. Behavior change: `query` no longer writes;
  an empty statement and a bare `PRAGMA` are refused. A `SELECT` returns the
  same rows as before. Go: `wire.QueryResponse` gains `Error` and `Refusal`
  (`wire.QueryRefusal`, `wire.QueryRefusalReason`). The `query` tool stays
  in its own Cloister group, `sql`.
