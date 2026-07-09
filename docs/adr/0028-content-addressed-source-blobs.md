# ADR-0028: Content-addressed source blobs

- **Status**: proposed
- **Date**: 2026-07-08
- **Bead**: `ley-line-open-925cab`
- **Related**: [ADR-0026 (content-addressed pointer store)](./0026-content-addressed-pointer-store.md), [ley-line ADR-014 (CDC + fountain-code composition)](https://github.com/agentic-research/ley-line/blob/main/docs/design/014-cdc-fountain-composition.md)

---

## 0. One-line claim

`_source` becomes content-addressed under BLAKE3 alongside its existing path index, closing the last non-CAS layer in LLO's substrate and setting up unified-CAS composition with rosary + cloister.

---

## 1. Context

### 1.1 The gap

Every other content-bearing layer in LLO's substrate is content-addressed under BLAKE3:

| Layer | Table / structure | Address |
|---|---|---|
| Arena buffers | Σ Merkle tree | BLAKE3 root |
| AST subtree dedup | `node_content` | BLAKE3 of subtree bytes |
| AST blob pointer store (ADR-0026 Phase 1) | `capnp_blobs` | BLAKE3 of canonical `List(AstNode)` |
| **Source bytes** | **`_source`** | **File path (text)** |

Source is the outlier. Path-keyed means:

- Two files with byte-identical content: **two rows** (no dedup)
- Rename: row moves to new path (dedup opportunity lost)
- Cross-repo: each repo has its own path namespace; no possibility of cross-repo dedup
- Cross-generation: reparse rewrites `_source` rows even when bytes are unchanged

### 1.2 Why now

Two forcing functions landed 2026-07-08:

1. **ADR-0026 Phase 1 shipped** (PR #145). The dual-write pattern is proven; the same discipline applies mechanically to source bytes.
2. **Unified-CAS direction crystallized**. The filesystem format (LLO/mache) + rosary + cloister compose into an application-layer CAS where code + state + work + identity share one address space. Source bytes must be Σ-addressed for that composition to work — an agent asking "give me function X's implementation" can't resolve through a path key.

### 1.3 What this enables

- **Cross-file source dedup**: identical source in two files → one blob
- **Cross-generation dedup**: reparse an unchanged corpus → zero new source blobs
- **Rename dedup**: move a file → path row updates, blob unchanged
- **Cross-repo dedup**: shared source content across repos deduplicates naturally under a shared CAS
- **git alignment**: BLAKE3-source-blobs are conceptually identical to git blobs under BLAKE3 hashing. Sets up future git-object ingest under the same substrate.
- **Granular clone**: agents materialize only source bytes they actually read, resolved by hash from any substrate node.

---

## 2. Decision — dual-store source blobs

### 2.1 Schema shape

```sql
-- Content-addressed source storage. One row per UNIQUE byte content.
CREATE TABLE source_blobs (
    blob_hash BLOB PRIMARY KEY,   -- BLAKE3(blob_bytes)
    blob_bytes BLOB NOT NULL,
    byte_len INTEGER GENERATED ALWAYS AS (length(blob_bytes)) STORED
);

-- _source stays as the path index. Gains content_hash FK.
ALTER TABLE _source ADD COLUMN content_hash BLOB REFERENCES source_blobs(blob_hash);
```

**Invariants:**

- `source_blobs.blob_hash == BLAKE3(source_blobs.blob_bytes)` — F1s enforces
- Every `_source` row has a non-null `content_hash` after Phase 1 completes
- `source_blobs.blob_bytes == _source.source` when joined on `content_hash` — F1s enforces byte-identity
- INSERT OR IGNORE on `source_blobs` handles dedup at insert time

### 2.2 Blob unit — per-file

Phase 1 uses per-file source blobs. Same rationale as ADR-0026 Phase 1's per-file AST blobs: safer default, F5s (cross-file dedup) proves the win at reasonable granularity. Sub-file (per-hunk / per-chunk) dedup is a possible Phase 2 refinement pending measurement.

Content-defined chunking (CDC, ley-line ADR-014) is a possible Phase 3 layer that would give sub-file dedup for free — but that's downstream and out of scope here.

### 2.3 What NOT to do

- **Do NOT drop `_source.source` in Phase 1.** Dual-store preserves rollback; matches ADR-0026 Phase 1 discipline.
- **Do NOT keep path-only queries as the primary read path.** Consumers should migrate to `content_hash` reads incrementally (Phase 2).
- **Do NOT store bytes twice permanently.** Phase 3 drops `_source.source` once F1s is green over a soak period.

---

## 3. Composition with git-CAS and the unified-CAS direction

### 3.1 git alignment

Git blobs are content-addressed under SHA-1 (SHA-256 in modern git). BLAKE3-source-blobs are the same idea under our preferred hash. If a future ADR ingests `.git/objects` into LLO's substrate:

- **Source blobs** map directly: `git cat-file blob <sha>` bytes get inserted into `source_blobs` under their BLAKE3 hash
- **Address-map table** records `git_sha ↔ blake3_hash` for cross-references
- **Cross-repo dedup** works transparently — same bytes, same BLAKE3, one entry regardless of which git repo they came from

Consumers never see the git SHA; they query by BLAKE3. Git becomes a data source, not the addressing model.

### 3.2 Three-legged unified CAS

- **LLO substrate** (this ADR + ADR-0026 + Σ): code + AST + source under BLAKE3
- **Rosary Dolt** (`me`, `ley-line-open`, `mache`, ...): beads + threads + agent runs; can gain a BLAKE3 view over Dolt commit refs
- **Cloister**: capability tokens reference BLAKE3 hashes as authorized_refs

Composition claim: agent state = manifest of BLAKE3 hashes. Ship the manifest, resolve via any substrate node, materialize what's actually read.

### 3.3 What ADR-0028 does NOT commit to

- Rosary's BLAKE3 view — separate ADR (rosary-side)
- Cloister's capability-hash tokens — separate ADR (cloister-side)
- Git-object ingest — separate ADR (probably ADR-0029 or later)
- CDC layer over source blobs — separate ADR (composes with ley-line ADR-014)

This ADR commits ONLY to: source_blobs table + dual-store + F-tests + migration path. The composition ADRs land later once their substrate is proven.

---

## 4. Falsifiability protocol

### F1s: Round-trip integrity

**Claim:** for every `_source` row, `source_blobs[content_hash].blob_bytes` is byte-identical to `_source.source`.

**Test:** dual-store during migration. Query pair per row; assert 100% agreement.

**Pass:** 100% agreement across ≥100k `_source` rows on the mache/internal benchmark corpus.

### F4s: Cross-generation dedup

**Claim:** re-parsing an unchanged corpus produces zero new `source_blobs` rows.

**Test:** parse corpus, snapshot `source_blobs` count. Re-parse, snapshot again.

**Pass:** exact-equal row counts. No new blobs.

### F5s: Cross-file dedup

**Claim:** N files with byte-identical content share one `source_blobs` entry.

**Test:** synthetic corpus with 100 files each containing byte-identical content. Parse. Query `source_blobs`.

**Pass:** exactly 1 blob for that content, 100 `_source` rows pointing to it.

### F-rename: Rename preserves dedup

**Claim:** moving a file preserves its `content_hash` — the blob is unchanged, only the path index moves.

**Test:** parse corpus. Rename file A → B. Reparse. Assert `_source[source_id=B].content_hash == _source[source_id=A].content_hash` (before rename). Assert `source_blobs` row count unchanged.

**Pass:** hash preserved, no new blob.

### F-git: git-compat proof

**Claim:** BLAKE3-source-blobs are conceptually identical to git blobs under BLAKE3. Ingesting a `.git/objects` blob and computing its BLAKE3 matches what LLO would produce for the same source bytes.

**Test:** pick a git blob from a real repo, extract via `git cat-file blob`, compute BLAKE3. Parse the same file through LLO. Assert `source_blobs.blob_hash == BLAKE3(git blob bytes)`.

**Pass:** hashes match. Proves the substrate is git-compatible for future ingest.

---

## 5. Kill criteria

Reasons to reject this ADR after implementation attempt:

1. **F1s fails**: dual-store returns different bytes than the row-projected source. Design broken; refactor or abandon.
2. **F5s fails or shows negligible dedup**: real-world corpora don't have enough content-identical files to make dedup pay for the extra table. Reject as premature optimization.
3. **F-git fails**: BLAKE3 of a source blob differs from BLAKE3 of the equivalent git blob. Suggests our substrate isn't hash-compatible with git ingest, breaking the unified-CAS composition claim.
4. **Storage overhead exceeds 2× during dual-store phase**: `source_blobs` is meant to eventually replace `_source.source`, but if the transition period is memory-catastrophic on real repos, revisit.

---

## 6. Non-goals

- Do NOT re-implement `_source`'s query path in Phase 1. Path lookups still work through `_source`; content_hash is metadata.
- Do NOT prescribe consumer migration order — that's Phase 2 (ADR-0026 §9.2 covers the analogous discipline; this ADR points at that pattern rather than duplicating it).
- Do NOT commit to CDC layer over source blobs. That's downstream (ley-line ADR-014 composes here later).
- Do NOT commit to git-object ingest. F-git proves compatibility; actual ingest is a separate ADR.

---

## 7. Migration path

**Phase 1** (this ADR's implementation scope): add `source_blobs` table. Add `_source.content_hash` column. Dual-store during parse: `_source.source` populated (existing) AND `source_blobs` populated (new) with `content_hash` linking them. F1s, F4s, F5s, F-rename, F-git tests run continuously.

**Phase 2**: consumers migrate from `_source.source` reads to `content_hash → source_blobs.blob_bytes` reads. Same dual-read discipline as ADR-0026 §9.2 (shadow → F2-analog gate → primary → soak).

**Phase 3**: drop `_source.source` column. `_source` becomes pure path index. `source_blobs` is the storage.

**Phase 4**: extend Σ substrate to publish `source_blobs` as first-class hash-addressed content (composes with the arena's existing BLAKE3 addressing).

---

## 8. Open questions

1. **Storage compression** — `source_blobs.blob_bytes` is uncompressed. Should we ship zstd compression at rest? Answered by measured storage overhead in Phase 1.
2. **content_hash on _file_index** — should `_file_index` (which tracks file existence + type) also gain content_hash? Answered when migrating that table's consumers.
3. **BLAKE3 chunking parameters** — plain BLAKE3 for now. When we add CDC (ley-line ADR-014 composition), chunk parameters need to be pinned. Deferred to that ADR.
4. **Git-compat hash tree** — do we ship an address-map table (`git_sha ↔ blake3_hash`) in this ADR, or defer to the git-ingest ADR? Recommend defer.

---

## 9. Provenance

- **2026-07-08**: user's insight during Phase 2 execution planning: "we should extract the CAS of git so that code and repo are content addressable at a byte level. So I can then do granular cloning and get to context savings?" Follow-up: "The filesystem format + rosary + cloister means we can have a unified CAS of all of it no?"
- The gap analysis (source is the only non-CAS layer) landed in the same conversation. This ADR closes that gap as the substrate-first move toward unified-CAS composition.
- ADR-0026 (2026-07-07) established the dual-write pattern for AST blobs; this ADR is the parallel application to source blobs.

---

## 10. Depends-on / Relates-to matrix

| Doc / bead | Role |
|---|---|
| ADR-0026 (content-addressed pointer store) | Parallel pattern for AST; this ADR mirrors its discipline for source |
| Σ substrate (BLAKE3 Merkle-CAS) | Hash function + address space this ADR extends to |
| ley-line ADR-014 (CDC + fountain-code) | Future CDC layer will chunk source blobs for sub-file dedup |
| Bead `ley-line-open-925cab` | This ADR's tracking bead |
| Bead `ley-line-open-8201de` | ADR-0026 Phase 2 execution plan; provides the dual-read discipline pattern this ADR references |
| Bead `ley-line-open-5b58ff` | Sheaf-driven granularity dispatcher; gains a THIRD storage layer (source blobs) to route to |
| Future rosary ADR | BLAKE3 view over Dolt beads; second leg of unified CAS |
| Future cloister ADR | Capability-hash tokens; third leg of unified CAS |
| Future git-ingest ADR | Uses F-git compat proof as prerequisite |
