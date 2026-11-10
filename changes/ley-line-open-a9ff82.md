Section: Fixed

- **A function used as a value records a `node_refs` row** (bead
  `ley-line-open-a9ff82`). Before, only call targets (plus Go's bare
  argument and keyed-literal identifiers) emitted, so a handler passed by
  value — Go `var h = helper`, `cobra.Command{RunE: run}`,
  `http.HandleFunc("/", s.serve)`, a Python decorator or `target=worker`,
  a JS `onClick={handle}`, a Java `this::handle`, a C function pointer —
  read as dead in mache's `dead_code`. Go, Rust, Python, JavaScript,
  TypeScript, Java, C and C++ now emit a ref for a name in value position
  (assignment and declaration right-hand sides, arguments, returns,
  composite/initializer literals, method values and method references).
  Qualified forms dual-emit like qualified calls (`s.serve` + `serve`
  with qualifier `s`). The rows are additive: every call ref is unchanged.
  A value ref is anchored at its value-position container, so its
  `node_kind` / `kind_id` (`argument_list`, `var_spec`,
  `return_statement`, ...) tells it apart from a call ref
  (`call_expression`, `call`, `method_invocation`). Field reads in the
  same positions are syntactically identical to method values and emit
  too; their tokens stay unbound when no def carries them, so
  `_meta.unbound_facts` rises. `EXTRACTION_EPOCH` moves 5 → 6, which moves
  `parserId` and makes an existing arena re-derive its facts on the next
  parse; `projection-v12` is unchanged (no table or column moves).
