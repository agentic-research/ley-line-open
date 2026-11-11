Section: Security

- **Read-only `query` refuses a statement that opens the reader's temp
  database** (bead `ley-line-open-3befbc`). SQLite reports no read of a
  table joined by `NATURAL` or `USING`, so `… NATURAL JOIN
  temp.sqlite_master` returned rows, and `pragma_table_info('t', 'temp')`
  opened the temp schema before its pragma was denied; either left the
  pooled reader with a second database. Such a statement is now refused as
  `denied_action` / `read` on `sqlite_temp_master` (or its pragma refusal),
  over the control socket and MCP alike, and the reader pool discards a
  reader whose temp database is open when it returns, so every reader again
  holds `main` alone.
