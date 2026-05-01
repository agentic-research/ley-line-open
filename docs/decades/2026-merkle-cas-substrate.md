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

> A content-addressed, vector-commitment-rooted, CAS-advanced state
> substrate Σ = (𝓥, 𝓒, ρ, σ, R, S) is the *canonical* realization of
> the requirements R1–R7 enumerated below — necessary in spirit
> (the requirements force a Σ-shaped commit-point) but not in
> implementation detail (Verkle, accumulator, and skip-list variants
> are admissible substitutes). The existing arena protocol is a
> degenerate case of Σ (sequence-named instead of hash-named);
> ADR-A and ADR-B are upper layers built on Σ. This decade formalizes
> Σ, makes its claims falsifiable, and argues canonicality from
> requirements (per the §3 red-team in
> `docs/decades/red-team/2026-05-01-section-3-necessity.md`, which
> downgraded the original "necessity" claim).

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
*Canonicality of Σ is argued against this set in §3.* Each requirement
is operationalized below — no informal hand-wave; each is a property
testable in TLA+ (Bead 1.2) and falsifiable by F1–F6 (§4).

| ID | Requirement                                                                          | Operationalization                                                                            |
|----|---------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------|
| R1 | Concurrent writers without global lock                                                 | At any time t, at most one writer holds an exclusive lock; the longest such hold ≤ ε (e.g. 1 ms)  |
| R2 | Cross-host distribution without central authority                                      | No single online entity is required for liveness; integrity is verifiable from cryptographic primitives alone |
| R3 | Cryptographic integrity: **content** identity self-naming + actor identity exogenous   | (R3a) Content c is named by σ(c); no external NS resolves identity-of-content. (R3b) Actor identity (S's pk) is bootstrapped exogenously and is *not* required to be content-addressed. |
| R4 | Crash consistency: atomic visibility of completed writes; partial writes invisible     | For every observed root R, ρ(R) is complete; no observer ever sees a half-committed state    |
| R5 | Time-travel / versioning: addressability of past states **within a retention horizon** | For every R_i committed within retention horizon T (default: 100 most recent advances), ρ(R_i) remains retrievable. GC of unreachable history outside T is permitted. |
| R6 | Registry-scale partial updates: O(log n) propagation for k = O(1) leaf changes         | For k = O(1), single-leaf updates touch at most ⌈log_b n⌉ + O(1) commitment nodes (b = fan-out). For k = O(n), bound degrades gracefully to O(k log_b(n/k)). |
| R7 | Hash-only interface: external systems interact with Σ via (σ, ρ, R, S) only            | No external system holds Σ-mutable runtime state. Static crypto config (σ params, S params, trust roots) is shared *immutable* configuration and is permitted. |

---

## 3. Canonicality argument

> Claim. Any system meeting R1–R7 implements Σ up to isomorphism in
> the *vector-commitment family*. Σ as defined in §1 is the canonical
> representative of that family; substitutes (Verkle trees, RSA
> accumulators, authenticated skip lists) are admissible.

**This is a downgrade from the original "necessity" framing**, applied
2026-05-01 after the §3 red-team
(`docs/decades/red-team/2026-05-01-section-3-necessity.md`) found five
counterexamples — cited inline below. The operational program (T1–T7,
F1–F6) is unaffected by the weakening; only Bead 1.2 adjusts scope
("canonicality search" rather than "necessity proof").

The argument is a chain of forced moves. Each step shows that dropping
the named substrate component invalidates at least one requirement, OR
admits the named family of substitutes.

### 3.1 R2 ∧ R3a ⟹ content-of-data is self-named (forces σ : 𝓥 → 𝓒)

Suppose identity-of-content ≠ content. Then some external name N maps
to content C through a name service NS.

- If NS is centralized: violates R2 (no central authority).
- If NS is distributed and **does not** terminate in content-addressing
  (federated PKI, DNSSEC, Web PKI, Matrix): the *actor* identities at
  the recursion's terminus are exogenously bootstrapped pubkeys. R3b
  permits this. But R3a still requires that **content** be self-named
  by σ(content), independent of who signed it. So even systems with
  exogenous actor identity (signet, IANA, Matrix) end up content-
  addressing the data they sign.

∴ R3a forces σ : 𝓥 → 𝓒 over content. R3b explicitly carves out actor
identity as exogenous — signet's CA pubkey is not required to be a
content hash. ∎

(*Red-team Gap 2 fix:* the original §3.1 conflated content-identity
with actor-identity. The fix splits R3 into R3a (content) and R3b
(actor) and applies σ-forcing only to R3a.)

### 3.2 R4 ∧ R5 ⟹ immutability (IM); R4 alone forces only atomic publish

Atomic visibility (R4) requires that the post-write state be
referenceable by a stable identifier the moment the write completes,
with no intermediate visible state.

R4 alone admits *mutable-but-atomically-published* designs: LMDB
(superblock swap over a CoW B+tree, intermediate pages mutable
during txn), PostgreSQL WAL+MVCC (in-place mutation under redo log),
ZFS/Btrfs (transaction-group rotation). All satisfy R4; none satisfy
IM in the strict sense — past page-states are reused or coalesced.

What forces IM is **R5 — past states must remain addressable within
the retention horizon**. If past R_i must be re-fetchable, then ρ(R_i)
cannot be overwritten; combined with σ's collision-resistance, this
means content-addressed blobs are write-once-keyed. The CAS publish
mechanism is the minimal *implementation* of R4 atomicity; IM is
contributed by R5.

∴ R5 forces IM; R4 forces atomic publish (whose lowest-overhead
realization is single-pointer CAS, but cf. §3.3). ∎

(*Red-team Gap 3 fix:* the original §3.2 attributed IM to R4 alone.
LMDB and WAL+MVCC are direct counterexamples. Re-attribution to
R4 ∧ R5 makes the claim defensible.)

### 3.3 R5 ∧ R6 ⟹ single-root commit point; CAS is the lowest-overhead realization

R1 (concurrent writers without global lock) admits multiple
linearization protocols:

- **Single-pointer CAS** on a root: writers race; loser rebases. The
  approach Σ specifies.
- **CRDTs** (Shapiro et al. 2011): commutative merges, no
  serialization point at all.
- **MVCC without root pointer** (PostgreSQL, Spanner): per-row
  visibility ranges.
- **Replicated logs** (Raft, Multi-Paxos): leader-elected log index.

What forces a *single-root commit point* is not R1 ∧ R4 (each of the
above satisfies both) but **R5 ∧ R6 jointly**: verifiable past states
(R5) with O(log n) authenticated diff (R6) require a single agreed
root hash per epoch. CRDTs natively give a state but not an
authenticated diff-root; MVCC has no global root; logs have a log
index that is not a content hash.

∴ R5 ∧ R6 force a single-root commit point. Single-pointer CAS is the
lowest-overhead realization (no leader election, no quorum) and is what
Σ canonicalizes. ∎

(*Red-team Gap 4 fix:* the original §3.3 claimed CAS was "minimal"
without justification. The fix attributes the single-root requirement
to R5 ∧ R6 and downgrades CAS from "minimal" to "lowest-overhead
realization" of an abstract single-root commit point.)

### 3.4 R6 ⟹ vector-commitment-rooted structure (Merkle is the canonical case)

R6 requires that k = O(1) leaf changes propagate in O(log n) work.

- Flat addressing: any change re-hashes the entire blob ⟹ O(n).
  Insufficient.
- **The vector-commitment family** is the set of data structures
  C : 𝓥* → 𝓒 such that single-leaf updates are sublinear and the root
  preserves collision-resistance. Members include:
  - **Merkle tree**: each internal node = σ(child‖child‖…). O(log_k n) updates.
  - **Verkle tree** (Kuszmaul 2018, Buterin 2021): vector commitment per
    internal node (KZG or IPA). O(log_k n) updates, **O(1) proofs**.
  - **RSA / class-group accumulators** (Camenisch-Lysyanskaya 2002,
    Boneh-Bünz-Fisch 2019): single group element. O(1) amortized.
  - **Authenticated skip lists** (Goodrich-Tamassia 2001): randomized
    DAG, expected O(log n).
  - **Sparse Merkle trees + non-membership proofs** (RFC 6962): same
    big-O, different commitment domain.

∴ Σ requires a vector-commitment family member at the root. The Merkle
tree is the canonical representative — chosen for Σ because BLAKE3 is
production-ready, the implementation cost is bounded, and ADR-A/B
already assume Merkle stalks. **Substitutes are admissible**: a future
Σ' built on Verkle would inherit Σ's correctness arguments with a
proof-size optimization. ∎

(*Red-team Gap 1 fix:* the original §3.4 claimed Merkle uniqueness,
which Verkle/accumulator/skip-list literature directly contradicts.
The fix downgrades from "unique" to "canonical representative of the
family."*)

### 3.5 R5 ⟹ retention of past R_i within horizon T

If past states must be addressable within retention horizon T, then for
every prior R_i committed within T the blob ρ(R_i) must remain
retrievable.

Combined with (IM) (§3.2), this means writes within T never overwrite.
Storage grows linearly with retained history; GC of states outside T is
permitted (T3.3). ∎

### 3.6 R7 ⟹ hash-only mutable runtime interface; static crypto config admitted

Compositionality requires that signet, workerd, mache, rosary all
interact with Σ without coordinating internal mutable runtime state.

- σ provides the universal content address.
- S provides the universal binding (sign the root).
- ρ provides the universal retrieval.

Each composing system holds *only* (R_n, sig_n) at runtime. State
sharing is through `cas(R_old, R_new)`, not through mutable handles.

**Static crypto config is shared and permitted by R7's
operationalization**: σ parameters (BLAKE3 variant, key context), S
parameters (curve, domain separators), trust roots (signet's CA
pubkey), and Merkle schema (fan-out, child encoding, leaf
canonicalization) are *immutable* configuration agreed at deployment
time. F5 (§4) tests that no *mutable runtime* state crosses
composition seams — which is the genuine R7 invariant.

∴ R7 forces hash-only mutable runtime interfaces; static crypto config
is shared immutable bootstrap. ∎

(*Red-team Gap 5 fix:* the original §3.6 said "no shared state
across composition seams" too strongly; trust roots and σ parameters
are necessarily shared. The fix admits static crypto config as
immutable bootstrap distinct from mutable runtime state.)

### 3.7 Conclusion

R1–R7 (operationalized per §2) force every component of Σ up to
isomorphism in the vector-commitment family. The Merkle tree is the
canonical realization; Verkle / accumulator / skip-list members are
admissible substitutes that inherit Σ's correctness arguments.

> ∴ Σ is **canonical**. ∎

The argument is sketch-level. Mechanizing it in TLA+/Apalache is
**Bead 1.2** below — scoped to *canonicality search* (does any system
satisfy R1–R7 outside the vector-commitment family?) rather than the
stronger original "necessity" framing.

(*The downgrade from "necessity" to "canonicality" was applied
2026-05-01 after a math-friend red-team. The full red-team analysis is
preserved at `docs/decades/red-team/2026-05-01-section-3-necessity.md`
for audit. The operational program — T1–T7, F1–F6, the feature-
completion criterion — is unchanged by the weakening.*)

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
| F6 | Adversarial: search for a vector-commitment-family member outside Σ that meets R1–R7 | Canonicality claim |

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

**F6 — Canonicality counterexample.** Adversarial: search for a system
that satisfies R1–R7 *outside* the vector-commitment family — i.e.,
without content-addressed identity, without immutable past states
within retention horizon, without single-root commit point, without
sublinear authenticated update at the root. If found, the canonicality
argument has a gap and we revisit (or accept that R1–R7 admit a
broader family than Σ canonicalizes). Note: a counterexample using
Verkle, accumulator, or auth-skip-list is *not* a falsification — the
argument explicitly admits these as substitutes (§3.4). (Formal-methods
bead — TLA+/Apalache bounded search.)

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
| 1.2    | TLA+ / Apalache spec of R1–R7 + canonicality argument              | analysis   |
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
| 7.6    | F6: TLA+ canonicality counterexample search                         | analysis   |

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
2. **Canonicality argument (Bead 1.2)** is mechanized in TLA+/Apalache with bounded model checking finding no counterexample at depth ≥ 7 (one per requirement) outside the vector-commitment family (Verkle / accumulator / auth-skip-list members are admitted as Σ-isomorphic, not counterexamples)
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
