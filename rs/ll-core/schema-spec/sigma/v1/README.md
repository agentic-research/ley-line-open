# `sigma/v1` — known-answer vectors for every Σ construction

**Status:** Shipped (ADR-0047 step M0, bead `ley-line-open-10d71a`).

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
| `partition` | `PartitionSpec` fold, `derive_key("leyline partition fold v1", …)` | `leyline-core` `partition.rs` | same |
| `headDigest` | head digest v2, `derive_key("leyline head digest v2", …)` | `leyline-core` `head_digest.rs` | same |
| `parserId` | `RowSet` fold `leyline/parser-id/v1` | `leyline-cli-lib` `tree_root.rs` | `leyline-cli-lib` `tests/sigma_axioms.rs` |
| `treeRoot` | `RowSet` fold `leyline/tree-root/v1`, `parserId` as params | `leyline-cli-lib` `tree_root.rs` | same |
| `astNode` | `node_hash` (`merkle-ast-v2`), plain σ over `llo/ast/v1 ‖ 0x00 ‖ …` | `leyline-cli-lib` `cmd_parse.rs` | `cmd_parse.rs` unit test |
| `restrictionKey` | `RowSet` fold of `derive_key("leyline/restriction/v1", …)` rows | `leyline-cache` `restriction.rs` | `leyline-cache` `tests/sigma_axioms.rs` |
| `runId` | `run-` ‖ σ(`cloister/execution/v1/run-id` ‖ 0x00 ‖ …) | `leyline-runtime` `authorization.rs` | `../../execution/v1/test-vectors/run-id.json` |

Where a construction hashes an encoding, the case carries `preimageHex`, and
the consuming test rebuilds that preimage from the documented encoding rather
than calling the fold, so the encoding and the hash are checked separately.

**A changed value is an identity migration.** ADR-0047 §4 moves three of these
constructions (`treeRoot` in M1, `node_hash` in M2, `run_id` in M3); each
migration replaces its cases in the PR that bumps the version, never by
editing a value in place. `astNode` and `runId` fail law (c) today (their
address is σ_bytes of their preimage, `ley-line-open-73add7`); the vectors pin
that behavior so the migration is visible.

`VECTORS.sha256` pins the file's bytes (`verify_vectors_sha256`).
