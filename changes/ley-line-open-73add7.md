Section: Breaking

- **`node_hash` is keyed: merkle-ast-v3 replaces merkle-ast-v2 (ADR-0047 M2,
  D-i, bead `ley-line-open-73add7`).** merkle-ast-v2 hashed a node as plain
  BLAKE3 over `"llo/ast/v1" ‖ 0x00 ‖ tag ‖ …`, so a file whose bytes were that
  preimage had a `blob_hash` equal to the node's `node_hash` (the 18-byte file
  `llo/ast/v1\0\0\x02fn\x02fn` named the Rust leaf `fn`). merkle-ast-v3 keys
  the same fields, without the prefix, under
  `derive_key("leyline/ast-node/v3", …)`, a different BLAKE3 mode from plain
  hashing, so no `blob_hash` reaches a `node_hash`. The tree each hash names
  is unchanged; every value moves.
  - **What moves:** `node_content.node_hash`, the `v_ast_hash`,
    `v_node_defs_hash` and `v_node_refs_hash` views, `parserId` (it folds
    `IR_SCHEMA_VERSION`, now `merkle-ast-v3`) and the `_meta.derived_from`
    stamp. `storeRoot` and `Head.rootHash` do not move.
  - **Version handshake and `compatibility.json`:** `ir_schema_version` is
    `merkle-ast-v3`. A consumer that memoizes `node_hash` values must treat
    the bump as a lineage boundary and re-key. `wire_format_major` stays 2: no
    wire field is renamed, removed or retyped; `validate`'s `emit_ast` rows
    and sheaf region hashes carry v3 values in the same fields.
  - **Known-answer vectors:** the `astNode` cases of
    `schema-spec/sigma/v1/test-vectors/constructions.json` are v3 (each pins
    its context), and the Go module `clients/go/leyline-schema` package
    `sigma` adds `AstLeafHash`/`AstInternalHash` and reproduces them
    (`task schema:go-sigma-vectors`).
  - **Old arenas:** the bump ships with `projection-v12`, so a v11 arena is
    refused at parse open; delete the `.db` and reparse. Nothing converts old
    hashes.
