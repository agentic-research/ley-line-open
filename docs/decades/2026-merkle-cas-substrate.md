# Decade: Merkle-CAS Substrate as Unifying Primitive

**Status:** Proposed
**Author:** James Gardner + Claude
**Date:** 2026-05-01
**Channel (per BDR/Harmony lattice):** `analysis` — decade-level architectural reasoning
**Relates to:**
- `bdr/tests/ADR-A-sheaf-cache.md` (Čech cohomology over Merkle stalks)
- `bdr/tests/ADR-B-merkle-sheaf-sync.md` (per-community Merkle sync)
- `bdr/ADR-001-harmony-lattice-decomposition.md` (decade/thread/bead structure)
- This repo: `ll-core/core/src/control.rs` (current atomic-flip primitive)
- This repo: `ll-open/cli-lib/src/cmd_daemon.rs::snapshot_to_arena` (current snapshot path)

---

## 0. One-line claim

> A content-addressed, Merkle-structured, CAS-advanced state substrate
> Σ = (𝓥, 𝓒, ρ, σ, R, S) is *necessary* — not sufficient, *necessary* —
> to satisfy the requirements R1–R7 enumerated below. The existing arena
> protocol is a degenerate case of Σ (sequence-named instead of
> hash-named); ADR-A and ADR-B are upper layers built on Σ. This decade
> formalizes Σ, makes its claims falsifiable, and proves necessity from
> requirements.

---

## 1. Formal definition

The substrate is a six-tuple:

```
Σ = (𝓥, 𝓒, ρ, σ, R, S)
```

### 1.1 Carriers

| Symbol | Type                              | Meaning                                                  |
|--------|-----------------------------------|----------------------------------------------------------|
| 𝓥     | `{0,1}*` (bounded)               | Content vocabulary — raw bytes of any storable blob      |
| 𝓒     | `{0,1}^256`                       | Content addresses — 256-bit hashes (BLAKE3 in practice)  |
| ρ      | `𝓒 → 𝓥 ∪ {⊥}`                  | Content-addressed retrieval                              |
| σ      | `𝓥 → 𝓒`                         | Content addressing function (collision-resistant hash)   |
| R      | `Var<𝓒>`                          | Root pointer — atomically advanced via CAS               |
| S      | `(𝓒, SK) → Sig`, `(𝓒, Sig, PK) → 𝟚` | Signature scheme over content addresses             |

### 1.2 Substrate axioms

| Axiom | Statement                                                                 | Establishes        |
|-------|---------------------------------------------------------------------------|--------------------|
| (CA)  | `∀v ∈ 𝓥: ρ(σ(v)) = v`                                                    | Round-trip         |
| (CR)  | `∀v ≠ v' ∈ 𝓥: Pr[σ(v) = σ(v')] ≤ 2^{-128}`                              | Collision resistance |
| (TR)  | Second-preimage resistance of σ                                            | Tamper resistance  |
| (DET) | σ is deterministic across hosts and runs                                   | Distributability   |
| (IM)  | Once `ρ(h)` is defined, it is permanently fixed (immutability)             | Crash consistency  |
| (CAS) | `R := cas(R_old, R_new)` is atomic across concurrent observers             | Concurrent advancement |
| (SIG) | `V(h, S(h, sk), pk) = 1 ⟺ pk` corresponds to `sk`; otherwise `0`           | Identity binding   |

### 1.3 Merkle structure

For composite content `c = (c_1, …, c_k)`:

```
σ(c) = σ(σ(c_1) ‖ σ(c_2) ‖ … ‖ σ(c_k))
```

Subtree update: changing `c_i` re-hashes only the path from `c_i` to the
root, of length `O(log_k |c|)`.

### 1.4 State evolution

A history is a sequence `(R_0, R_1, …, R_n)` where each `R_{i+1}` is
produced by:

```
R_{i+1} ← CAS(R_i, σ(update(ρ(R_i), Δ)))
```

Failed CAS ⟹ writer rebases by reading the new `R`, re-applying `Δ`, and
retrying. This is **optimistic concurrency control with content-addressed
linearization**.

### 1.5 Mapping the existing arena onto Σ

| Σ component           | Today's implementation                                          |
|-----------------------|------------------------------------------------------------------|
| `R: Var<𝓒>`           | `Controller.generation: u64` (sequence-named, not content-named) |
| `cas(R_old, R_new)`   | `Controller::set_arena(path, size, gen)` mmap'd write             |
| `ρ`                   | `sqlite3_deserialize` from arena buffer                          |
| `σ`                   | **Missing** — bytes are addressed by sequence, not hash          |
| Merkle structure      | **Missing** — arena is one monolithic blob                        |
| `S`                   | **Missing** — no root signing                                    |

The current system is the degenerate `σ = identity-on-sequence-number`
case. Hash-naming the buffer + storing under hash + replacing
`generation` with `current_root: [u8; 32]` is a continuous migration, not
a re-architecture.

---

## 2. Requirements (R1–R7)

These are the load-bearing requirements for the agentic platform.
*Necessity of Σ is proved against this set.*

| ID | Requirement                                                                          |
|----|---------------------------------------------------------------------------------------|
| R1 | Concurrent writers without global lock                                                 |
| R2 | Cross-host distribution without central authority                                      |
| R3 | Cryptographic integrity: signed identity bound to content                              |
| R4 | Crash consistency: atomic visibility of completed writes; partial writes invisible     |
| R5 | Time-travel / versioning: any past state addressable                                  |
| R6 | Registry-scale partial updates: O(log n) propagation for k-leaf changes (n total)      |
| R7 | Composes with: workerd execution, signet identity, mache projection, rosary dispatch   |

---

## 3. Necessity proof (sketch)

> Theorem. Any system meeting R1–R7 must implement Σ up to isomorphism.

The proof is a chain of forced moves. Each step shows that dropping the
named substrate component invalidates at least one requirement.

### 3.1 R2 ∧ R3 ⟹ identity = content (forces σ : 𝓥 → 𝓒)

Suppose identity ≠ content. Then some external name N maps to content C
through a name service NS.

- If NS is centralized: violates R2 (no central authority).
- If NS is distributed: NS itself requires identity binding ⟹ recurse on
  the same problem. The recursion terminates only if the binding
  *terminates in content-addressing* — i.e., names *are* hashes of
  content somewhere in the chain.

∴ identity must equal content somewhere; the minimal such system uses
content addressing throughout. ∎ (R2 ∧ R3 forces σ.)

### 3.2 R4 ⟹ immutability (IM) and atomic root (CAS)

Atomic visibility requires that the post-write state be referenceable by
a stable identifier *the moment* the write completes, with no
intermediate visible state.

- If artifacts are mutable: a reader can observe state mid-mutation
  (violates R4 unless mutation is atomic, which for arbitrary-size
  artifacts is impossible without a swap primitive).
- The minimal swap primitive is CAS on a pointer to immutable content.

∴ writes produce immutable artifacts; visibility is governed by CAS on a
pointer (R). ∎

### 3.3 R1 ∧ R4 ⟹ optimistic CCC via single advancement primitive

Concurrent writers without a global lock require a serialization point.

- Pessimistic locks violate R1 (some writer holds, others wait).
- Lock-free protocols at the data-structure level (DLHT-shape) work
  in-process but require the data-structure to be the data plane, not a
  blob.
- Optimistic CCC against a single CAS-advanced root pointer is the
  minimal protocol that admits both R1 and R4: writers compute a new
  root from the same parent and race to advance R; only one wins per
  generation; losers rebase.

∴ R is a single-pointer CAS primitive; advancement is optimistic. ∎

### 3.4 R6 ⟹ Merkle structure

R6 requires that a k-leaf change propagate in O(log n) work, where n is
total leaves.

- Flat addressing: any change re-hashes the entire blob ⟹ O(n).
- A k-ary tree where each parent's hash = H(child hashes): change to one
  leaf re-hashes O(log_k n) parents.

The Merkle tree is the unique structure (up to k-ary fan-out) that
achieves O(log n) propagation while preserving (CR) at the root. ∎

### 3.5 R5 ⟹ retention of past R_i

If past states must be addressable, then for every prior R_i the blob
ρ(R_i) must remain retrievable.

Combined with (IM), this means writes never overwrite. Storage grows
monotonically (mod GC of unreachable history). ∎

### 3.6 R7 ⟹ no shared state across composition seams

Compositionality requires that signet, workerd, mache, rosary all
interact with Σ without coordinating internal state.

- σ provides the universal address.
- S provides the universal binding (sign the root).
- ρ provides the universal retrieval.

Each composing system holds *only* a hash and a signature. State sharing
is through `cas(R_old, R_new)`, not through mutable handles. ∎

### 3.7 Conclusion

R1–R7 force every component of Σ. None can be dropped without losing a
requirement.

> ∴ Σ is **necessary**. ∎

(The proof is sketch-level. Formalizing R1–R7 in a temporal logic
[TLA+ or Apalache] and mechanizing the necessity argument is **Bead
1.2** below.)

---

## 4. Falsifiability protocol

The substrate is *only useful* if its claims are falsifiable. Six
protocols, each an executable test:

| ID | Protocol                                | Falsifies                                                  |
|----|------------------------------------------|------------------------------------------------------------|
| F1 | Crash mid-write, verify reader behavior  | (IM) — atomic visibility / no torn state                   |
| F2 | N concurrent writers, measure throughput | R1 — non-blocking concurrent advance                       |
| F3 | MITM gossip with crafted blob            | (CR) — content addressing prevents tampering               |
| F4 | k-leaf edit in n-leaf tree, count hashes | R6 — O(log n) Merkle propagation                           |
| F5 | Compose Σ with signet/workerd/rosary     | R7 — boundaries are hash-only, no per-blob coordination    |
| F6 | Adversarial: build a system meeting R1–R7 *without* Σ | Necessity claim itself                              |

**F1 — Crash consistency.** Spawn a writer that performs a snapshot;
SIGKILL at random points during write; verify that for every observed
`R`, `ρ(R)` is complete and `σ(ρ(R)) = R`. If any reader sees a
half-written state that satisfies the hash check, (CR) is broken; if any
reader sees a torn `R` that points at a partial blob, (IM) is broken.

**F2 — Optimistic concurrency.** N writer threads, each performing
`Δ_i`. Measure committed transactions per second vs serial baseline.
Predicted: at low contention, throughput ≈ N×serial. As contention rises
(many writers same subtree), throughput plateaus at the rebase-cost
limit. If throughput is bounded by *serial* throughput, R1 is falsified
(global lock somewhere we didn't account for).

**F3 — Distribution integrity.** Sender hosts blob B, root `σ(B)`.
Receiver pulls. MITM swaps in B' with `σ(B') ≠ σ(B)`. Receiver MUST
reject. If accepted, (CR) or signature verification is broken.

**F4 — Diff complexity.** Construct Merkle tree over n = 50,000
documents. Edit k = 1 leaf. Count hashes recomputed. Predicted:
`O(log_2 n) ≈ 17`. Falsified if recomputation is `≥ √n` or `n`.

**F5 — Compositionality.** Build a workload where:
- signet signs `R_n` only (one signature per advance, not per blob)
- workerd executes against `R_n` by fetching ρ(R_n)
- rosary dispatches a bead whose content is `R_n` reference
- mache projects ρ(R_n) into its FUSE view

If any of those requires per-blob coordination beyond `(R_n, sig_n)`,
R7 (compositionality) is falsified.

**F6 — Necessity counterexample.** Adversarial: search for a system that
satisfies R1–R7 without using content-addressed identity, immutability,
CAS-advanced root, or Merkle structure. If found, the necessity proof
has a gap and we revisit. (This is the formal-methods bead — likely
TLA+/Apalache to bound the search space.)

---

## 5. Decade → Thread → Bead structure

Per the BDR/Harmony lattice (ADR-001):

- **Decade:** this document — `analysis` channel reasoning
- **Threads:** ordered groups of beads, each addressing a sub-property
- **Beads:** atomic deliverables (a PR, a commit, a closed issue)

### Thread T1 — Mathematical foundations (4 beads)

Establishes the formal substrate as a Rust module + machine-checked proof.

| Bead   | Title                                                              | Channel    |
|--------|--------------------------------------------------------------------|------------|
| 1.1    | Substrate types in Rust: `H`, `ρ`, `σ`, `R`, signing trait         | final      |
| 1.2    | TLA+ / Apalache spec of R1–R7 + necessity argument                 | analysis   |
| 1.3    | Falsification suite F1–F6 as integration tests                     | final      |
| 1.4    | ADR amendments: link ADR-A and ADR-B as upper layers on Σ          | commentary |

### Thread T2 — Controller migration (4 beads)

Replaces sequence-named arena with content-named arena. Backwards-compatible additive path.

| Bead   | Title                                                              | Channel    |
|--------|--------------------------------------------------------------------|------------|
| 2.1    | Add `Controller.current_root: [u8; 32]` alongside `generation`     | final      |
| 2.2    | Compute σ(arena buffer) on snapshot; populate `current_root`       | final      |
| 2.3    | Reader-side: verify `σ(ρ(R)) = R` before mmap-deserialize          | final      |
| 2.4    | Deprecate `generation`; release with breaking version bump         | final      |

### Thread T3 — Content-addressed blob store (3 beads)

git-shaped object directory; multiple roots coexist; GC by reachability.

| Bead   | Title                                                              | Channel    |
|--------|--------------------------------------------------------------------|------------|
| 3.1    | `objects/<hash[0..2]>/<hash[2..]>` layout in arena dir              | final      |
| 3.2    | Atomic blob write (tmpfile + rename) with fsync                    | final      |
| 3.3    | GC: retain N most recent roots + reachable subtree closure         | final      |

### Thread T4 — Subtree Merkle (4 beads)

Per-file / per-table / per-community subtree hashes. Enables F4.

| Bead   | Title                                                              | Channel    |
|--------|--------------------------------------------------------------------|------------|
| 4.1    | Per-file Merkle hash in `_file_index` (hash-of-content)             | final      |
| 4.2    | Per-table Merkle hash in `_meta`                                    | final      |
| 4.3    | Top-level Merkle reduce: `R = σ(∥ subtree hashes)`                  | final      |
| 4.4    | Diff API: `(R_a, R_b) → changed-subtree set` in O(log n + k)       | final      |

### Thread T5 — Composition seams (4 beads)

Validates R7 — signet, workerd, rosary, mache compose via hash-only.

| Bead   | Title                                                              | Channel    |
|--------|--------------------------------------------------------------------|------------|
| 5.1    | signet signs `R_n` advance; cert binds key to advancement authority | commentary |
| 5.2    | workerd manifest = Cap'n Proto + content-addressed root reference  | commentary |
| 5.3    | rosary bead body references `R_n`; dispatch fetches ρ(R_n)         | commentary |
| 5.4    | mache projects ρ(R_n) into FUSE view; root-aware hot-swap           | commentary |

### Thread T6 — Concurrent-write protocol (3 beads)

Validates R1 — optimistic CCC with rebase-on-loss.

| Bead   | Title                                                              | Channel    |
|--------|--------------------------------------------------------------------|------------|
| 6.1    | Writer protocol: `read R; build R'; CAS(R, R')`                    | final      |
| 6.2    | Loser rebase: 3-way merge over Merkle subtrees                     | final      |
| 6.3    | Conflict-resolution policy (CRDT-shaped per-table or app-level)    | analysis   |

### Thread T7 — Falsification gauntlet (6 beads)

Each protocol F1–F6 as a continuous test that can fail in CI.

| Bead   | Title                                                              | Channel    |
|--------|--------------------------------------------------------------------|------------|
| 7.1    | F1: crash-mid-write integration test                                | final      |
| 7.2    | F2: N-writer throughput benchmark                                   | final      |
| 7.3    | F3: MITM gossip integrity test                                      | final      |
| 7.4    | F4: O(log n) diff-complexity benchmark                              | final      |
| 7.5    | F5: end-to-end composition test                                     | final      |
| 7.6    | F6: TLA+ necessity counterexample search                            | analysis   |

### Thread ordering

```
T1 (foundations) ─→ T2 (controller migration) ─→ T7.1, T7.2 (early gauntlet)
                                              │
                                              ↓
                                          T3 (CAS store) ─→ T7.3
                                              │
                                              ↓
                                         T4 (subtree Merkle) ─→ T7.4
                                              │
                                              ↓
                          T5 (composition) + T6 (concurrent writes) ─→ T7.5, T7.6
```

T1 is sequencing-critical (math first). T2 unlocks T3. T4 unlocks
F4. T5 and T6 are parallelizable after T4. T7 beads run continuously
once their dependencies land.

---

## 6. Feature-completion criterion

Σ is *feature-complete* when:

1. **All R1–R7** have at least one falsification protocol (F1–F6) passing in CI
2. **Necessity proof (Bead 1.2)** is mechanized in TLA+/Apalache with bounded model checking finding no counterexample at depth ≥ 7 (one per requirement)
3. **All composition seams (T5)** are exercised by a single integration test that touches signet, workerd, rosary, mache simultaneously
4. **The arena migration (T2)** is shipped: `current_root` is the canonical advancement primitive; `generation` is removed
5. **F2 throughput** at N=10 concurrent writers exceeds serial by ≥ 4× (validates R1 isn't lock-hidden)

This is the operational definition of "the substrate works."

---

## 7. Risks and open questions

| ID    | Risk / question                                                                          | Mitigation                                                                 |
|-------|-------------------------------------------------------------------------------------------|----------------------------------------------------------------------------|
| K1    | Hash collision (CR) — practical risk vanishingly small at 256 bits but not zero          | Use BLAKE3; audit for known weaknesses on cadence                          |
| K2    | Storage growth from immutability                                                          | T3.3: retention policy; reachability GC                                    |
| K3    | Cross-host CAS coordination beyond a single host (R is local mmap today)                  | Future thread: "distributed root advancement" — quorum CAS, possibly via raft on R alone (NOT on data) |
| K4    | Subtree Merkle requires *stable canonicalization* of table content                        | Bead T4.2: define canonical serialization (sorted by primary key, no NULLs vs missing) |
| K5    | TLA+ proof effort                                                                         | Bound at depth ≥ 7; defer full formalization to T1.2 only                  |
| K6    | Sheaf cache (ADR-A) and sync (ADR-B) terminology must align with this substrate           | Bead 1.4: cross-references to ADR-A/B                                       |
| K7    | Workerd / signet integrations require those projects to expose hash-friendly seams         | Bead 5.1, 5.2: scoping spike before commitment                              |

---

## 8. Why this can't be merged into existing ADRs

ADR-A and ADR-B are *upper layers*: ADR-A is "structurally-aware cache invalidation," ADR-B is "per-community sync." Both *assume* a Merkle structure exists. Σ is the substrate they assume. Naming and formalizing the substrate is what makes ADR-A/B's preconditions checkable.

The current arena protocol (`Controller`, `ArenaHeader`, `HotSwapGraph`)
*almost* implements Σ — it has CAS-advance, double-buffered atomic flip,
and reader hot-swap. It's missing σ (content addressing) and the Merkle
structure. This decade closes that gap by treating the existing
implementation as a starting point, not a replacement target.

---

## 9. Provenance

This decade emerged from a session-pair:

- Earlier 2026-04-30: scale-problem hardening sweep (28 commits across LLO, ts, lsp, schema, hdc) shipped concrete fixes for the *symptoms* of an unnamed substrate problem (Mutex<SqliteGraph> contention, idx_source_file bloat, _lsp orphans, MAX_PARSE_FILE_SIZE).
- 2026-05-01: thread on Pollen / DLHT / WASM-as-transport revealed that those projects *all* point at the same missing primitive, which the user named as "CAS-based + Merkle hash".
- This document formalizes the primitive they named and structures the
  work to ship it.

The Merkle-CAS substrate is the architectural proper name for what got
called "the concurrent write problem," "the rest of K8s," and
"WASM-as-transportable" in earlier discussion. None of those framings
were right; the substrate framing is.
