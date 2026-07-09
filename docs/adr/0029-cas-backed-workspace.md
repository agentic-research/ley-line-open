# ADR-0029: CAS-backed workspace (bead-scoped manifest mounts)

- **Status**: proposed
- **Date**: 2026-07-09
- **Bead**: `ley-line-open-4b19f2`
- **Related**: [ADR-0026 (content-addressed pointer store)](./0026-content-addressed-pointer-store.md), [ADR-0028 (content-addressed source blobs)](./0028-content-addressed-source-blobs.md)

---

## 0. One-line claim

Replace `git worktree add` as the agent-dispatch primitive with a CAS-backed manifest mount. The agent's cwd is a mache mount over a manifest constructed from the bead's `files:` scope. Reads resolve through the substrate's content-addressed layers. Writes are copy-on-write into the CAS. Sub-file granularity (per-AST-node scoping) falls out.

---

## 1. Context

### 1.1 What worktree-fork costs today

`git worktree add` is the current agent-dispatch primitive. For a bead scoped to "rename function X in these 3 files":

| Cost | What actually happens |
|---|---|
| **Time** | Seconds to minutes: `git worktree add` copies files, allocates checkouts, sets up index. On a 500MB repo the round trip is human-perceptible. |
| **Disk** | Full checkout per parallel agent. 4 concurrent agents = 4× the repo size. |
| **Context / rediscovery** | Agent walks the tree to build its mental model. mache index rebuild, file-type detection, dependency resolution — all repeated per agent. |
| **Isolation** | Real (agents work on separate branches) but at the cost of the above. |
| **Scope** | Full repo access. Bead says "3 files" but agent could read anything. |

### 1.2 What we've built that enables the alternative

By the time this ADR lands, LLO has:

- **Σ substrate**: BLAKE3-Merkle-CAS at the arena level
- **`capnp_blobs`** (ADR-0026 Phase 1, PR #145): per-file AST blobs content-addressed
- **`source_blobs`** (ADR-0028 Phase 1, PR #153): source bytes content-addressed
- **F-git compat proof** (ADR-0028 F-git test): substrate is byte-identical to git's blob model
- **mache FUSE/NFS mount**: already presents CAS content at paths for AST queries
- **rosary bead schema**: `files:` field declares exactly which paths a bead touches
- **rosary dispatch**: today spawns `git worktree add`; can be swapped

Every leg of the composition exists. Nothing is speculative — this ADR is about ORCHESTRATION, not new substrate.

### 1.3 The insight

An agent's workspace doesn't need to be a filesystem checkout. It can be a **manifest** — a list of `(path, blob_hash, mode)` triples. Reads resolve through the CAS; writes hash new bytes and update the manifest. The agent never sees files outside its scope because they aren't in the manifest.

Sub-file granularity is the natural extension. Since AST subtrees are Σ-addressed (`capnp_blobs`), a bead can scope at the AST-node level. Agent's view: "these 12 subtrees are yours to mutate; the rest of the file is read-only projection."

---

## 2. Decision — manifest-mount replaces worktree-fork

### 2.1 Manifest data structure

```
Manifest {
    root: BLAKE3,                 // manifest itself is content-addressed
    bead_id: String,
    scope_kind: Enum { WholeFile, PerNode, Hybrid },
    entries: [
        Entry {
            path: PathBuf,
            blob_hash: BLAKE3,    // → source_blobs or capnp_blobs
            mode: FileMode,       // POSIX permissions
            granularity: Enum { WholeFile, NodeSubtree(node_hash), FileRange(start, end) },
        }
    ],
}
```

Manifests are themselves content-addressed. Given a manifest hash, resolve the manifest → resolve each entry → materialize a POSIX-visible tree.

### 2.2 Mount driver

Mache's existing FUSE/NFS mount gains a manifest-mode:

- Given a manifest hash, mount at some path (e.g. `.mache/mounts/<manifest-hash>/`)
- `open(path)` inside the mount resolves through the manifest → CAS → returns bytes
- `read` outside the manifest's paths → ENOENT
- `write` inside the manifest's paths → hash new bytes → insert into source_blobs → update manifest (in-memory; commit on close)
- `write` outside → EACCES

### 2.3 Copy-on-write semantics

Source blobs are immutable (content-addressed). A write:

1. Buffers new bytes in memory
2. On flush: `BLAKE3(new_bytes)` → `INSERT OR IGNORE INTO source_blobs`
3. Updates the manifest entry's `blob_hash` in-memory
4. On close: the delta manifest is what gets translated to git

The old blob remains in the CAS unchanged. Multiple agents editing the same file operate on independent manifest snapshots — no locking beyond the CAS's own INSERT OR IGNORE.

### 2.4 Bead-scope enforcement

Bead schema already has `files: [String]`. Manifest constructor:

- For each `path` in `bead.files`: look up current blob_hash from `_source.content_hash` → add entry
- If bead scopes at AST-node granularity (`node_hashes: [BLAKE3]`), lookup via `_ast_pointer` → capnp_blobs
- Reads outside the manifest's paths return ENOENT (mount enforces)

**Isolation is structural, not policy.** An agent literally can't see out-of-scope bytes because they aren't in its manifest.

### 2.5 Manifest → git commit translation

When the agent finishes, the manifest delta needs to become git commits on the branch:

1. Compare initial manifest (from bead spawn) with final manifest (after CoW writes)
2. For each changed entry: `git checkout` the branch, write blob_bytes to the working tree path, `git add`
3. `git commit` with the bead-id prefix (Golden Rule 11)
4. `git push` — standard PR flow from there

The git-side is standard. The novelty is that the source-of-truth during the agent's session is the manifest, not the working tree.

---

## 3. Sub-file granularity — the killer feature

A bead scoped to "rename function X in these 3 files" doesn't need whole files. It needs the AST subtrees containing X's definition + call sites.

Since `capnp_blobs` (ADR-0026 Phase 1) already content-addresses per-file blobs and `_ast_pointer` maps node_id → blob+offset, sub-file scoping is:

- Bead names AST node hashes (or symbol names → resolved to node hashes at spawn)
- Manifest entries point at capnp_blobs offsets, not whole files
- Mount exposes a synthetic file (`function_X.rs.slice`) whose content is the node's source text
- Writes: agent edits the slice → hash new node subtree → CAS → manifest updated → git-side reconstructs the parent file by splicing the new subtree bytes at the original offset

Context savings: a rename that today loads 3 whole files (~30KB each) can be a manifest of 3 node subtrees (~1KB each). Order-of-magnitude reduction in agent input.

Bead `ley-line-open-5b58ff` (sheaf-driven granularity dispatcher) is the query-side analog: which granularity to READ. This ADR-0029 is the WRITE-side analog: which granularity to expose for mutation.

---

## 4. Composition with unified CAS (three-legged story)

- **LLO substrate** (this ADR + ADR-0026 + ADR-0028 + Σ): code + AST + source under BLAKE3
- **Rosary** (future ADR): BLAKE3 view over Dolt beads + manifest emit + dispatch mode
- **Cloister** (future ADR): capability-hash tokens authorize specific manifest hashes

Agent state = manifest hash. Ship the hash, resolve on any substrate node, materialize what's actually read.

The `me` repo direction (portable identity-rooted personal-state bundle) becomes: identity + manifest root. Load `me`, resolve refs, materialize.

---

## 5. Falsifiability protocol

### F1w: Startup time

**Claim:** manifest mount startup < 1s vs `git worktree add` typical 5-30s on repo > 100MB.

**Test:** on a 500MB test repo, measure wall-time for:
- (a) `git worktree add` + `cd` into the worktree
- (b) `rsry_dispatch --mount-manifest <bead>` + `cd` into the mount

**Pass:** (b) < 1s across a 5-file bead.

### F2w: Storage cost

**Claim:** N concurrent agents on the same repo consume O(1) disk (shared CAS), not O(N × repo size).

**Test:** spawn 4 concurrent agents on 4 different beads. Measure `du -sh` on the CAS directory before and after.

**Pass:** delta < 10MB per agent (only the CoW deltas).

### F3w: Isolation

**Claim:** agent cannot read files outside its bead scope.

**Test:** spawn agent on a bead scoped to `["src/foo.rs"]`. Attempt to `open("src/bar.rs")`. Must fail.

**Pass:** ENOENT (or EACCES). No fallback that leaks the file.

### F4w: Sub-file granularity

**Claim:** bead scoped to 3 AST subtrees produces manifest with total bytes ~KB, not MB.

**Test:** bead scoped to 3 function subtrees from files that are each > 10KB. Measure manifest byte-size.

**Pass:** manifest + resolved bytes < 5KB total.

### F5w: Commit fidelity

**Claim:** manifest → git commit produces identical git diff to what a worktree-based agent would have produced for the same edits.

**Test:** synthetic edit script (rename function, add call site, delete comment). Apply via manifest-mount workflow AND via worktree workflow. Compare `git diff` output.

**Pass:** byte-identical diffs.

---

## 6. Kill criteria

- **F1w fails** — mount startup ≥ worktree startup. Design bet on "manifests are faster" broken.
- **F3w fails** — mount leaks out-of-scope reads. Isolation guarantee broken; design must fix or abandon.
- **F5w fails** — manifest → git commit doesn't produce faithful diffs. Downstream tooling (code review, CI, deploy) breaks.
- **CoW correctness bug** — parallel writes to same file produce incoherent state. Rare but load-bearing; kill the parallelism claim.

---

## 7. Non-goals

- Do NOT prescribe rosary's dispatch API in this ADR (rosary's own ADR owns that)
- Do NOT prescribe cloister capability-hash tokens (their ADR owns that)
- Do NOT commit to git-object ingest here (ADR-0028's F-git is the compat proof; ingest is a future ADR)
- Do NOT commit to network-fetched CAS (this ADR assumes local CAS; distributed CAS is a future concern)
- Do NOT deprecate `git worktree add` — dual-mode during transition

---

## 8. Migration path

**Phase 1**: manifest data structure + local mount driver. Prove F1w-F5w on a single bead type (single-file scope). No rosary integration yet.

**Phase 2**: rosary dispatch mode. `rsry_dispatch --mount-manifest` alongside existing worktree mode. Falsifiable A/B: run same bead through both paths, compare wall-time + correctness.

**Phase 3**: sub-file granularity. Extend manifest schema to node-hash refs; extend mount driver to expose synthetic slice files; extend git-translator to splice edits back.

**Phase 4**: cloister capability tokens. Manifests become authorization-scoped (agent can only mount manifests it holds a capability for).

**Phase 5**: distributed CAS. Manifest hash resolves against local CAS first, remote CAS (via ley-line ADR-014 transport) as fallback. Cross-machine agent portability.

---

## 9. Open questions

1. **Deletes and creates** — manifest represents current state; how do we express "delete file X" or "create new file Y"? Answer probably: manifest entries can be null/tombstoned; new entries added by CoW.
2. **Rename detection** — path change is a manifest edit; can we preserve blob_hash across rename to make git-side rename detection trivial? Design point.
3. **Concurrent writes to same file** — CoW gives each agent an independent snapshot, but conflict resolution at git-commit time is standard. Do we need at-close-time reconciliation?
4. **Binary files** — non-source blobs (images, .lock files, generated code) also live in source_blobs conceptually. Schema might need a `binary` flag to skip AST parsing.
5. **Mount performance at scale** — FUSE/NFS have per-operation latency. For interactive `ls` in a 10k-file scope, mount performance matters. Tune or defer?

---

## 10. Provenance

- **2026-07-09**: user's question during backlog sweep: "Today a worktree is a fork, right? meaning parallel work is $$$ in both context, time, rediscovery, etc. Git is CAS native. Rosary is CAS native. Mache is CAS native (cause of LLO). What would it take to make it so we dont need to 'fork' and we can just take the piece the bead is scoped to?"
- Follow-up: "like in a sub-file / CAS of the bytes manner that aligns with the overall idea?"
- Analysis showed every leg (source_blobs from ADR-0028, mache mount, rosary bead scope, cloister perimeter) exists. This ADR is the orchestration layer that composes them.
- ADR-0028 F-git compat proof (PR #153, 2026-07-09) established that the substrate can talk git; ADR-0029 uses that substrate as the workspace.

---

## 11. Depends-on / Relates-to matrix

| Doc / bead | Role |
|---|---|
| ADR-0026 (pointer store) | Provides `capnp_blobs` for sub-file (AST-node) manifest granularity |
| ADR-0028 (source blobs) | Provides `source_blobs` for whole-file manifest granularity + F-git compat |
| Bead `ley-line-open-4b19f2` | This ADR's tracking bead |
| Bead `ley-line-open-5b58ff` | Sheaf-driven granularity dispatcher (READ-side); this ADR is WRITE-side analog |
| Bead `ley-line-open-925cab` | ADR-0028 tracker; substrate this ADR builds on |
| Future rosary ADR | Dispatch mode + BLAKE3 bead view; second leg of unified CAS |
| Future cloister ADR | Capability-hash tokens; third leg of unified CAS |
| Future ley-line ADR | Distributed CAS transport (Phase 5 network fetch) |
