# HCL/Terraform Raw Node References Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Leyline's raw CGO-free HCL/Terraform parse artifact emit the typed `env:` and `mod:` reference tokens already consumed by Mache.

**Architecture:** Add an HCL `tags.scm` that structurally selects variable block labels and literal module `source` values. Dispatch `TsLanguage::Hcl` through a small query-backed extractor that trims HCL string delimiters and applies the consumer-visible schemes, preserving the generic engine's block `node_id`, `source_id`, and enclosing-container attribution. Bump the scalar extraction epoch so existing arenas re-derive byte-identical HCL sources.

**Tech Stack:** Rust 2024, tree-sitter-hcl, tree-sitter queries, rusqlite serialized SQLite tests.

## Global Constraints

- Keep the producer path entirely Rust/tree-sitter and CGO-free.
- Emit only `variable NAME` -> `env:NAME` and literal `module ... { source = LOCATOR }` -> `mod:LOCATOR`.
- Do not emit resource labels, provider sources, or other non-module `source` values.
- Verify behavior by deserializing and querying the raw SQLite artifact.
- Preserve the existing SQL projection ABI and `node_refs` attribution semantics.

---

### Task 1: Serialized SQLite regression

**Files:**
- Test: `rs/ll-open/cli-lib/tests/hcl_address_refs_test.rs`

**Interfaces:**
- Consumes: `cmd_parse::parse_into_conn(&Connection, &Path, Option<&str>, Option<&Path>)` and `Connection::serialize`.
- Produces: A regression test proving exact tokens and valid block/source/container attribution in the serialized database.

- [ ] **Step 1: Write the failing test**

Add `raw_hcl_serialized_projection_emits_typed_address_refs`. Write a temporary fixture containing one variable, one module with a literal `source`, one resource with a `source` attribute, and another non-module source. Run the actual CLI producer into an in-memory connection, serialize and deserialize it, then query `node_refs` joined to `_ast`. Assert the exact ordered token set is `env:DATABASE_URL` and `mod:./modules/app`; every row has a non-empty `node_id`, `source_id = "main.tf"`, a matching `_ast` block row, and `container_node_id IS NULL`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p leyline-cli-lib --test hcl_address_refs_test -- --nocapture`

Expected: FAIL because the serialized artifact currently contains zero `node_refs` rows.

### Task 2: Query-backed HCL producer

**Files:**
- Create: `rs/ll-open/ts/queries/hcl/tags.scm`
- Modify: `rs/ll-open/ts/src/refs.rs`
- Test: `rs/ll-open/cli-lib/tests/hcl_address_refs_test.rs`
- Test: `rs/ll-open/cli-lib/tests/f6_extraction_epoch_invalidation.rs`
- Test: `rs/ll-open/ts/tests/coverage_contract.rs`

**Interfaces:**
- Consumes: `QueryEngine::extract(...) -> Vec<ExtractedRef>` and the existing per-node fold arguments.
- Produces: `extract_hcl(node, source, node_id, source_id, container_node_id) -> Vec<ExtractedRef>` and a `TsLanguage::Hcl` dispatcher arm.

- [ ] **Step 1: Add the structural HCL query**

Create two anchored `@ref` patterns: a `variable` block whose label is `@name`, and a `module` block whose direct-body `source` attribute has a literal string `@name`. Keep resource and non-literal source shapes structurally unmatched.

- [ ] **Step 2: Implement the minimal extractor**

Compile `queries/hcl/tags.scm` once with `OnceLock<QueryEngine>`, run it through the generic engine, determine the typed scheme from the anchored block's first identifier (`variable` or `module`), trim only the surrounding HCL quote delimiter from emitted ref tokens, prefix `env:` or `mod:`, and retain the generic engine's attribution fields unchanged.

- [ ] **Step 3: Wire the dispatcher and invalidation epoch**

Add the feature-gated `TsLanguage::Hcl` arm to `extract_refs`. Change `EXTRACTION_EPOCH` from 4 to 5 and document that epoch 5 adds HCL/Terraform typed address refs. Add an epoch-4 arena migration test that deletes the old zero-ref output, reparses unchanged HCL bytes under the current extraction identity, and observes the exact typed rows restored. Add HCL to the fact-emitting coverage contract as a ref-only algebra.

- [ ] **Step 4: Run the focused test to verify it passes**

Run: `cargo test -p leyline-cli-lib --test hcl_address_refs_test -- --nocapture`

Expected: PASS with exactly the two desired rows.

- [ ] **Step 5: Run the HCL and full crate slices**

Run: `cargo test -p leyline-ts --features hcl hcl_ -- --nocapture`

Run: `cargo test -p leyline-ts --all-features`

Expected: PASS; pre-existing compiler warnings may remain but no new HCL warnings or failures appear.

### Task 3: Release-path and repository verification

**Files:**
- Verify only: `rs/ll-open/ts/src/refs.rs`, `rs/ll-open/ts/queries/hcl/tags.scm`, `rs/ll-open/ts/src/lib.rs`

**Interfaces:**
- Consumes: repository `task` gates and the built `leyline` CLI.
- Produces: release-mode evidence that the standalone raw database contains the expected rows without Mache's schema projector.

- [ ] **Step 1: Run formatting and diff validation**

Run: `cargo fmt --check`

Run: `git diff --check`

- [ ] **Step 2: Build the release CLI and probe a controlled fixture**

Run:

```bash
task release
probe_dir=$(mktemp -d)
rs/target/release/leyline parse \
  /Users/jamesgardner/remotes/art/mache/cmd/testdata/preset_fixtures/terraform \
  -o "$probe_dir/terraform-raw.db"
sqlite3 "$probe_dir/terraform-raw.db" \
  "SELECT token FROM node_refs ORDER BY token;"
```

Expected output:

```text
env:bucket_name
env:region
mod:./modules/logging
```

Then query `node_id`, `source_id`, and `container_node_id`, joined to `_ast`, to confirm each ref points at an HCL `block` in `main.tf` with no enclosing function container.

- [ ] **Step 3: Run repository gates**

Run: `task check`

Run: `task ci`

Record any unavailable or environment-only gate explicitly; do not close the bead unless the acceptance criteria's executable evidence is satisfied.

- [ ] **Step 4: Record and checkpoint**

Add a Rosary bead comment listing changed files, red/green evidence, release probe output, gates run, and remaining risks. Commit with `[ley-line-open-55c1cc] fix(ts): emit Terraform address refs` only after verification succeeds, then close the bead.
