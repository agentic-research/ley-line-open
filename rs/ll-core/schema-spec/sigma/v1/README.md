# `sigma/v1` — known-answer vectors for every Σ construction

**Status:** Shipped (ADR-0047 step M0, bead `ley-line-open-10d71a`; `storeRoot` and head
digest v3 cases from step M1, bead `ley-line-open-73c6ee`; `astNode` v3 cases from step M2,
bead `ley-line-open-73add7`).

Σ (`docs/decades/2026-merkle-cas-substrate.md`) is indexed by construction
(ADR-0047 §1): one value has one address per construction, and each
construction is either σ_bytes (plain BLAKE3, the one unkeyed construction) or
a keyed fold. `test-vectors/constructions.json` pins one or more cases per
shipped construction so a second implementation, in Go or TypeScript, checks
its encoding and its hash without an LLO checkout. This is Σ axiom (DET) made
executable: the same file is read by Rust tests on every CI host.

| Key | Construction | Code | Consumed by |
|---|---|---|---|
| `sigmaBytes` | σ_bytes = BLAKE3(bytes): blobs, CDC chunks, arena images | `leyline-core` `substrate.rs` | `leyline-core` `tests/sigma_axioms.rs` |
| `deriveKey` | the keyed primitive, BLAKE3 `derive_key(context, input)`; cases from BLAKE3's own `test_vectors.json` (input byte `i` is `i mod 251`, first 32 output bytes). A primitive, not a construction: no address moves | `leyline-core` `substrate::derive_key` | `leyline-core` `tests/sigma_axioms.rs`; `leyline-cas-ffi` `leyline_derive_key` (C ABI, and the wasm build cloister loads; bead `ley-line-open-12e252`) |
| `partition` | `PartitionSpec` fold, `derive_key("leyline partition fold v1", …)` | `leyline-core` `partition.rs` | same; the Go module `clients/go/leyline-schema` package `sigma` (`sigma_vectors_test.go`) |
| `headDigest` | head digest v3, `derive_key("leyline head digest v3", …)` over `generation ‖ rootHash ‖ parentHash ‖ storeRoot ‖ parserId ‖ P` | `leyline-core` `head_digest.rs` | same; the Go module `clients/go/leyline-schema` package `sigma` (`sigma_vectors_test.go`) |
| `parserId` | `RowSet` fold `leyline/parser-id/v1` | `leyline-cli-lib` `store_root.rs` | `leyline-cli-lib` `tests/sigma_axioms.rs` |
| `storeRoot` | directory Merkle tree `leyline/store-tree/v1`: one `RowSet` fold per directory, params empty, children in name-byte order as `(σ(name), 2i, \|name\|)` and `(child, 2i+1, git mode)`; no parser input. Each case pins its leaves (a directory leaf has no `content`), every directory node by path with its entry count, and the root | `leyline-cli-lib` `store_root.rs` | `leyline-cli-lib` `tests/sigma_axioms.rs`; the Go module `clients/go/leyline-schema` package `sigma` (`sigma_vectors_test.go`) |
| `astNode` | `node_hash` v3 (`merkle-ast-v3`), `derive_key("leyline/ast-node/v3", …)` over `0x00 ‖ uvarint\|kind\| ‖ kind ‖ uvarint\|token\| ‖ token` (leaf) or `0x01 ‖ uvarint\|kind\| ‖ kind ‖ uvarint n ‖ child × n` (internal), `kind` the canonical κ kind of a named node and the grammar kind of an anonymous token, uvarints unsigned LEB128. Each case pins its context | `leyline-cli-lib` `cmd_parse.rs` | `cmd_parse.rs` unit test; the Go module `clients/go/leyline-schema` package `sigma` (`sigma_vectors_test.go`) |
| `restrictionKey` | `RowSet` fold of `derive_key("leyline/restriction/v1", …)` rows | `leyline-cache` `restriction.rs` | `leyline-cache` `tests/sigma_axioms.rs` |
| `runId` | `run-` ‖ σ(`cloister/execution/v1/run-id` ‖ 0x00 ‖ …) | `leyline-runtime` `authorization.rs` | `../../execution/v1/test-vectors/run-id.json` |

Where a construction hashes an encoding, the case carries `preimageHex`, and
the consuming test rebuilds that preimage from the documented encoding rather
than calling the fold, so the encoding and the hash are checked separately.

**A changed value is an identity migration.** ADR-0047 §4 moves three of these
constructions (the store identity in M1, `node_hash` in M2, `run_id` in M3); each
migration replaces its cases in the PR that bumps the version, never by
editing a value in place. M1 (`ley-line-open-73c6ee`) removed the `treeRoot`
cases (`leyline/tree-root/v1`, `parserId` as params) and added the `storeRoot`
cases, and moved `headDigest` from v2 to v3: the preimages are byte-identical
and the digests differ by the derive-key context alone. M2 (`ley-line-open-73add7`)
replaced the `astNode` cases: merkle-ast-v2 addressed a node as σ_bytes of
`llo/ast/v1 ‖ 0x00 ‖ enc`, so a file holding those bytes had the node's address (law (c));
v3 keys `enc` (the same fields, prefix removed) under `leyline/ast-node/v3`, and the
consuming tests check that neither plain digest names the node. `runId` still fails
law (c) (its address is σ_bytes of a prefixed preimage) until M3; its vectors pin that
behavior so the migration is visible.

`VECTORS.sha256` pins the file's bytes (`verify_vectors_sha256`).
