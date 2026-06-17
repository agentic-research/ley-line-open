# BLAKE3 Inline-Site Retrofit Audit — Phase 0 + 1

**Date:** 2026-05-19
**Bead:** `ley-line-open-e7b983` (P1 task, dev-agent)
**Parent direction:** Retrofit existing inline `blake3::hash(...)` sites onto the Σ substrate's `BlobStore` trait so the substrate's documented invariants (substrate.rs:28-50) become load-bearing rather than docstring-only. Retrofit-first per user directive: tests + audit BEFORE impls + migration.

**Scope of this doc:** the enumeration, classification, and surrounding-code findings. Does **not** propose new impls, migrate any call sites, or modify the trait surface. Those are Phase 2–4.

---

## 1. Enumeration

`rg -n 'blake3::hash' rs/ll-open/ --type rust` returns **23 matches**. The seam-discovery subagent's initial count of "6 sites" was wrong on three axes: undercounted production sites (missed `cmd_load` + `cmd_daemon`), miscounted one test as production (`graph.rs:1449`), and didn't distinguish semantically different uses. Classified properly the real shape is **8 production sites in 3 semantic groups**.

| # | Site | Group | Function | Status |
|---|---|---|---|---|
| 1 | `rs/ll-open/cli-lib/src/cmd_load.rs:63` | **A** | `pub fn load_into_arena(control, db_bytes)` | retrofit candidate |
| 2 | `rs/ll-open/cli-lib/src/cmd_daemon.rs:734` | **A** | `pub fn snapshot_to_arena(conn, ctrl_path)` | retrofit candidate |
| 3 | `rs/ll-open/fs/src/lib.rs:82` | **A** | `fn verify_arena_root(ctrl, header, buf)` | retrofit candidate (read-side) |
| 4 | `rs/ll-open/fs/src/graph.rs:954` | **A** | `pub fn flush_to_arena(&self)` | retrofit candidate (canonical) |
| 5 | `rs/ll-open/hdc/src/sheaf.rs:484` | **B** | `pub fn merkle_root_for_layer(...)` | stay inline |
| 6 | `rs/ll-open/hdc/src/sheaf.rs:552` | **B** | `fn merkle_root(leaves)` empty-leaves arm | stay inline |
| 7 | `rs/ll-open/hdc/src/sheaf.rs:566` | **B** | `fn merkle_root(leaves)` internal-node arm | stay inline |
| 8 | `rs/ll-open/hdc/src/util.rs:158` | **C** | `pub fn blake3_seed(bytes) -> u64` | stay inline |

The other 15 matches are tests (`*/tests/*.rs`, `#[cfg(test)] mod tests`), benches (`*/benches/*.rs`), or doc-example code (`control.rs:300`). Test code legitimately computes BLAKE3 as a test oracle — these are not "bypass sites" and should remain inline after the retrofit.

## 2. Classification rationale

### Group A — `BlobStore`-shape (4 sites)

Same semantic across all four: "the BLAKE3 of these bytes IS the content identity for this blob being stored in (or verified against) the arena." This is exactly what `BlobStore::put(&[u8]) -> Result<Hash>` declares.

Three sites are write-side (`load_into_arena`, `snapshot_to_arena`, `flush_to_arena`) and follow a near-identical 3-step pattern:

1. Get bytes (param or `serialize`).
2. `write_to_arena(&mut mmap, &bytes)`.
3. Compute `blake3::hash(&bytes)`; `ctrl.set_arena_with_root(path, size, root)`.

One site is read-side verification (`verify_arena_root`): "given bytes from the arena buffer and a `current_root` from the controller, compute `blake3::hash(buf)` and bail if they disagree."

All four are retrofit-able to the `BlobStore` trait. Phase 2 ships the impl; Phase 3 migrates these sites; Phase 4 lint-gates against regressions.

### Group B — Sheaf merkle (3 sites, stay inline)

These compute internal nodes of a merkle tree over the sheaf cache (per-layer roots → structural root). Different semantic from blob storage:

- **No blob is being stored.** `merkle_root` is a pure function `&[[u8; 32]] -> [u8; 32]`. There's no "put bytes, get hash" — it's "fold a tree."
- **The hash is the merkle node value**, not a content-identity for some external bytes.
- **`BlobStore::put`'s axioms don't apply.** Idempotence holds, but `put(b1) == put(b2) iff b1 == b2` doesn't make sense for an internal merkle node.

Could a separate trait (`MerkleHasher`) abstract these? In principle yes. Should it? Probably not in the Σ substrate proper. The merkle algorithm is a fixed primitive of `leyline-hdc`'s sheaf cache; abstracting it would invite the same "swap BLAKE3 for something else" question that Σ §3.4 (substrate.rs:28-50) already decided in the negative for the substrate. No retrofit. Audit-only.

### Group C — HDC seed derivation (1 site, stay inline)

`blake3_seed(bytes: &[u8]) -> u64` truncates BLAKE3 to a u64 for use as a PRNG seed. The function is documented (util.rs:153-156) as "truncating to 64 bits is fine — the codebook only needs ~200 distinct seeds." Different return type (`u64` not `[u8; 32]`), different purpose (seed not identity).

Not a `BlobStore` candidate. Stays inline.

## 3. Test coverage status

For each Group A site, where do existing tests stand and what do we still need?

| Site | Existing coverage | Coverage of *exact* hash mapping | New test needed? |
|---|---|---|---|
| 1. `cmd_load.rs:63` `load_into_arena` | `cmd_load.rs::tests::load_errors_when_arena_path_unset` covers the no-arena-path error. No happy-path test pinning `current_root == blake3::hash(input)`. | **Missing** | YES |
| 2. `cmd_daemon.rs:734` `snapshot_to_arena` | `integration.rs::snapshot_populates_current_root_with_blake3_of_db_bytes` (line 1164-1206) pins exact hash. Plus `snapshot_idempotent_root_for_same_db_state` for idempotence. | **Present** | NO — cite existing |
| 3. `fs/src/lib.rs:82` `verify_arena_root` | `arena_flush_e2e.rs::flush_round_trip` exercises the verify path indirectly via `from_arena`. No explicit positive+negative pair pinning hash-mismatch bails. | **Partial** | YES — explicit positive + negative |
| 4. `graph.rs:954` `flush_to_arena` | `arena_flush_e2e.rs::flush_round_trip` + `double_flush_advances_root` verify the root advances and is non-zero. No exact-hash pin. | **Partial** | YES — exact hash mapping |

**Conclusion:** characterization test additions cover sites 1, 3, 4. Site 2 is already pinned exactly in `integration.rs`. New test file (this PR): `rs/ll-open/fs/tests/characterization_blake3_sites.rs` for sites 3 + 4. New cli-lib test extends `integration.rs` (or sibling file) for site 1.

## 4. Surrounding-code audit findings

Self-audit-skill checklist applied to each Group A site. Findings classified `inline-fix` | `separate-bead` | `noted-only`.

### F1 — Three near-duplicate "write + publish" implementations (separate-bead)

Sites 1, 2, 4 (`load_into_arena`, `snapshot_to_arena`, `flush_to_arena`) implement the same 3-step pattern with small variations:

- `cmd_load.rs:36-68` — 32 lines; takes `&[u8]` directly; no arena growth.
- `cmd_daemon.rs:678-744` — 67 lines; computes new arena size with `ARENA_GROWTH_FACTOR`; calls `set_arena` (size advertise) before `write_to_arena` if growing.
- `graph.rs:939-966` — 28 lines; reads bytes from a `SqliteGraphAdapter`; logs the new root.

Differences are real (growth, logging, source-of-bytes) but the **publish primitive** (`write_to_arena` + `blake3::hash` + `set_arena_with_root`) is identical across all three. After Phase 2, all three collapse to `blob_store.put(&bytes)?` followed by `ctrl.set_arena_with_root(path, size, hash.0)?`.

**Finding action**: file a follow-up bead for "extract `publish_to_arena(bytes, ctrl, path, size, growth_policy) -> Result<Hash>` helper" — but only after Phase 2 + 3 land. The trait-based migration is the cleaner consolidation than introducing a helper first.

### F2 — Σ §3.4 algorithm-baked-into-contract is documented at the call sites (noted-only)

`cmd_daemon.rs:733` carries the comment `// current_root = BLAKE3(serialized db bytes); Σ §3.4 locks BLAKE3.` This is the *correct* expression of the substrate's invariant — the algorithm is locked at the spec level, not just the type level. The retrofit must preserve this assumption in the migrated code (the trait impl is BLAKE3-shaped; the documented contract stays BLAKE3-named).

The seam scan flagged this as a "leak" requiring abstraction. It is not. Σ §3.4 (substrate.rs:28-50) is a *deliberate* decision: the trait's axioms (collision resistance, second preimage) are conditional on BLAKE3 specifically. Algorithm-generic substrate is a separate `Σ'` decade per the docstring. No retrofit action needed for this comment.

### F3 — `cmd_daemon::snapshot_to_arena` two-phase publish (noted-only)

`cmd_daemon.rs:706-735` shows a deliberate two-phase pattern:

1. If growing the arena: `ctrl.set_arena(&arena_path, new_size)` — advertise the new size, **do not** advance `current_root`.
2. `write_to_arena(&mut mmap, &db_bytes)` — write into inactive buffer.
3. `ctrl.set_arena_with_root(&arena_path, new_size, current_root)` — atomic publish.

The retrofit must preserve this two-phase shape. If Phase 2's `BlobStore::put` is "do the whole thing atomically," it can't model `set_arena_then_set_arena_with_root`. Trait design implication: either the trait keeps the *write* and the *publish* separate (one method per phase), or `cmd_daemon` keeps direct `Controller` access for the size-advertise step and only routes the hash through the trait.

**Finding action**: pin this constraint in the audit. Phase 2's trait design must respect it. Probable resolution: `BlobStore::put` computes the hash + writes the buffer; the *publish* (controller-side `set_arena_with_root`) stays as a separate call. The trait abstracts hashing+writing, not publishing.

### F4 — `verify_arena_root` accepts `data_size == 0` and zero-root sentinel; retrofit must preserve (noted-only)

`fs/src/lib.rs:66-70`: an arena with `data_size == 0` returns `Ok(&[])` regardless of `current_root`. The fresh-arena case is the only one where the zero sentinel is accepted. Past this, a zero sentinel with non-empty buffer bails (lib.rs:72-80).

The retrofit must keep this branch. If `BlobStore::put(&[])` returns a non-zero hash (BLAKE3 of empty is a real value, not zero bytes), then `verify_arena_root`'s comparison logic still works for empty buffers because the data_size check happens *first*. But the retrofit shouldn't inadvertently make "empty arena requires non-sentinel root" — that's a regression.

Characterization test: `verify_arena_root_accepts_empty_with_zero_sentinel`.

### F5 — `hex_short_8` and `hex_short` are near-duplicates across files (separate-bead)

`fs/src/lib.rs:98-105` defines `hex_short_8` (8 hex chars from 4 bytes). `fs/src/graph.rs` and `cli-lib/src/cmd_daemon.rs` each define `hex_short` with similar signature. Three implementations of the same 4-byte-to-8-hex-chars helper.

Not blocking the retrofit. File as a separate `dev-agent` chore bead for consolidation into one `leyline_core` utility.

### F6 — `cmd_daemon.rs` comments reference Σ §3.4, `cmd_load.rs` doesn't (noted-only)

`cmd_load.rs:60-63` says "Publish via current_root (BLAKE3 of db bytes) — bead `ley-line-open-baee26`." Mentions BLAKE3 and the bead but not Σ §3.4. `cmd_daemon.rs:733` says "Σ §3.4 locks BLAKE3." Inconsistent documentation density. Trivial to fix during Phase 3 migration; not blocking.

### F7 — No dead struct fields, no rotting comments at the 4 sites (noted-only)

Walked the surrounding ~50 lines of each of the 4 Group A sites. No dead struct fields. No rotting comments (all references to T2.3, T2.4, Σ §3.4, and bead IDs check out against current code). No test-only `pub` exports detected. No stale TODOs.

This is unusual and worth noting: the substrate code is already well-maintained at the comment level. The retrofit work is moving code, not cleaning rot.

## 5. Phase 2-4 implications

Things this audit binds for the future phases (so they don't get re-derived):

1. **Trait design (Phase 2): `BlobStore::put` separates *write* from *publish*.** Per F3, the trait abstracts hashing+writing-the-buffer; the controller-side `set_arena_with_root` stays a direct call. Don't pack both into one trait method.
2. **Trait design (Phase 2): `BlobStore` exposes a `hash_of(&[u8]) -> Hash` method.** For the verify case (site 3), the caller has bytes from the arena buffer and wants to know "what would `put` have produced?" without actually putting. This is a pure-function method; the impl is `blake3::hash(bytes).into()`.
3. **Trait stays BLAKE3-baked (Phase 2-3).** Per F2, the algorithm lock is intentional. Don't introduce a `Hash<C>` generic in this work.
4. **Migration (Phase 3): site 2 may not change at all** because its existing test (`integration.rs:1188`) pins exact-hash behavior. If `BlobStore::put` produces the same bytes, the test passes; the migration is a 2-line code change with a green test.
5. **Lint gate (Phase 4): `rg 'blake3::hash' rs/ll-open/ --type rust` must allow Group B + Group C + tests, deny everything else.** Encode the allowlist explicitly — likely a path-based filter or a `#[allow(blake3_direct)]` attribute on the Group B/C functions.

   **Shipped 2026-06-17:** `tools/lint_blake3.sh` + explicit allowlist `tools/blake3-allowlist.txt`, wired into `task lint:blake3` and folded into `task ci`. Auto-allows `tests/`, `benches/`, in-file `#[cfg(test)]` blocks (cutoff at the first `#[cfg(test)]` line per file), and comment-only lines. Allowlist enumerates 6 production sites: 3 Group B (`hdc/src/sheaf.rs:484, 552, 566`), 1 Group C (`hdc/src/util.rs:158`), 2 cas-ffi cross-check sites (`cas-ffi/src/ffi.rs:104`, `cas-ffi/src/lib.rs:44` — added post-PR-#54 BLAKE3-FFI work, not in this audit's original scope). The lint also detects *stale* allowlist entries (line moved or file deleted) so the allowlist can't silently rot. Verified both directions manually (unsanctioned call → exit 1; stale entry → exit 1).

## 6. Crosswalk: bead body vs reality

The bead body says "6 sites" — wrong. Real count is 8 production sites in 3 groups, only 4 retrofit candidates. Filed correction as a comment on `ley-line-open-e7b983` (in this PR's accompanying bead-store delta). The bead's `files` field should also gain `rs/ll-open/cli-lib/tests/characterization_blake3_sites.rs` (the cli-lib site test file) once we decide whether to colocate with `integration.rs` or split.

## 7. References

- Σ trait surface: `rs/ll-core/core/src/substrate.rs:28-50` (axioms), `:58` (Hash newtype), `:145` (BlobStore), `:181` (RootPointer), `:207` (RootSigner).
- Existing exact-hash characterization (site 2): `rs/ll-open/cli-lib/tests/integration.rs:1186-1206`.
- Existing round-trip coverage (sites 3, 4 — partial): `rs/ll-open/fs/tests/arena_flush_e2e.rs`.
- Seam scan that surfaced this work: subagent output 2026-05-19, summarized in this session's transcript.
