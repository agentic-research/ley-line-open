# ADR-0032 — Declared decompositions: three identity structures, one fold operator

**Status:** Proposed (2026-07-27)
**Bead:** `ley-line-open-ef5c1d` (P0)
**Related:**

- ADR-0014 (capnp as protocol — the additive-field rule this ADR's `Head` changes obey)
- ADR-0026 / ADR-0028 (content-addressed pointer store / source blobs — sibling F-gate discipline)
- ADR-0031 (restriction-addressed caching — the exact-not-approximate move this ADR generalizes)
- ADR-0033 (retroactive CDC record — consumes this ADR's D3, bead `b6653a`)
- Decade `ley-line-open-9d30ac` (Σ — Merkle-CAS substrate as unifying primitive)
- Private ley-line ADR-014 / ADR-015 (transport composition; ADR-015's `HierarchySpec` is the
  prior instance of this ADR's descriptor)

______________________________________________________________________

## Thesis

> **Σ defines naming but not partitioning.** `σ : 𝓥 → 𝓒` is total on whole values. There is
> no partition operator in the tuple and no law composing part-addresses into whole-addresses.
> Every layer that needed parts invented its own cut, so the repo holds five hash identities
> over the same bytes that are not merely unlinked — they are **incommensurable**.
>
> The fix is not a link between roots. It is a **declared decomposition** carried with every
> address, and a **co-attestation** binding the structures that provably cannot be merged.

______________________________________________________________________

## Context — what ef5c1d observed

Five identities, each correct, each answering a different question, none declaring which:

| # | Identity | Cut of the bytes | Question answered | Hash |
|---|---|---|---|---|
| 1 | `Controller.current_root` | none — whole serialized SQLite buffer | is this byte image intact? | BLAKE3 |
| 2 | `Head.rootHash` | per-capnp-segment, concatenated `source‖ast‖bindings` | did this parse run produce these segments? | BLAKE3 |
| 3 | CDC chunk + `content_manifest` | content-defined, 8–128 KiB | which regions changed? | BLAKE3 |
| 4 | ADR-0031 restriction key | logical row-set spanning objects | is this derived view still valid? | **SHA-256** |
| 5 | ADR-014 sheaf-block / `hierarchy_id` | structural, restriction lattice | what recovers independently on the wire? | BLAKE3 |

`docs/ARCHITECTURE.md:97-108` already tabulates four of these as "deliberately separate." That
table *describes* the split. It does not say what relates them, which is what a consumer needs
in order to verify anything spanning two of them.

The gap is not local. It has surfaced independently three times:

- **arena identity** — the five roots above;
- **derived views** — ADR-0031 specifying SHA-256 against a substrate whose `Hash` doc
  (`substrate.rs:28-42`) declares BLAKE3 locked and warns that mixing functions "breaks (DET)
  and (CR) **at the composition boundary**";
- **ledger serialization** — `rosary-afdc19`: an unordered export makes `BLAKE3(beads.jsonl)`
  unstable, blocking signing of the bead ledger.

Three unrelated layers reaching the same failure is the evidence that this is a missing
substrate operator rather than local sloppiness.

______________________________________________________________________

## The formal result

**The operator.** `A(spec, children) = BLAKE3(canon(spec) ‖ (addr₁,frame₁) ‖ … ‖ (addrₙ,frameₙ))`
— a tagged Merkle fold over a *declared* decomposition. ADR-015's `canonical_serialize` +
`hierarchy_id` is exactly this, already built for one spec.

**Theorem 1 — no root homomorphism.** For two folds `A`, `B` over the same value there is no
function `root_A → root_B` valid for all values. A root is a 256-bit digest of an n·256-bit
leaf vector; a universal root-to-root map would require the root to determine that vector, and
pigeonhole forbids it for n ≥ 2. **Refinement order induces no maps between roots.**

**Theorem 2 — the alignment law.** `root_B` *is* computable from `A`'s **witness** (leaf
addresses + framing) in `O(#leaves)` 64-byte hashes, touching zero bytes, **iff** `B`'s parts
are unions of `A`'s parts. Where alignment fails, the extra cost is exactly the bytes of the
`A`-parts split by `B`'s foreign cuts — bounded for CDC leaves by `MAX_CHUNK` (128 KiB,
`cdc/src/lib.rs:35`) per foreign cut.

So `σ` extends to a functor on leaf-anchored decomposition trees, but *partition ↦ root* is not
functorial along refinement: the assignment factors through the leaf vector. **Roots do not
compose. Witnesses do.**

This is `rechunk`'s "exact, not heuristic" argument (`cdc/src/lib.rs:93-117`) — a boundary as a
pure function of a declared input closure — promoted from a local optimization to the general
law. That pattern is already mutation-tested in this repo, which is why it is the load-bearing
claim and not an analogy.

### Impossibility results (the failure boundaries)

**(A) Flat `σ` is not compositional.** BLAKE3 is internally a Merkle tree over **fixed 1024-byte**
chunks whose chaining values incorporate a chunk counter — the counter exists to kill
splice/reorder attacks. CDC cuts are content-defined and unaligned, so **no CDC chunk hash is
ever a node of BLAKE3's internal tree.** Identities 1 and 2 are not folds over CDC leaves and
cannot be made so.

**(B) Verify and dedup are irreducibly two structures.** BLAKE3's position-**binding** counter is
what makes it secure as a single hash. Position-**independence** of part addresses is what makes
dedup work at all (boundary stability, `cdc/src/lib.rs:11-21`). One Merkle structure cannot have
both: position-bound interior nodes cannot be shared across offsets; position-free interior nodes
cannot reproduce the flat root. Forcing one structure means abandoning either `σ(v) = BLAKE3(v)`
or dedup.

**(C) Row-sets are not stably byte-representable.** A row's byte extent depends on physical
layout; inserting an *unrelated* row splits B-tree pages and relocates rows the closure never
touched — destroying exactly the locality restriction-addressing exists for.

**(D) Identity 4 is not even a partition.** Different review families' closures **overlap**. They
form a *cover* of the row universe, not a partition. Type error, not alignment failure.

______________________________________________________________________

## Decision

### D1 — Three declared structures, not four parallel roots

| Structure | Question | Leaf universe | Root |
|---|---|---|---|
| **Integrity** | is this byte image intact, per page, lazily? | BLAKE3's intrinsic fixed 1024-B grid (position-bound) | `current_root` = `σ(v)`, **unchanged** |
| **Dedup / transport** | what changed, dedups, streams? | CDC chunks (position-free) | `manifestRoot`, `hierarchyId` |
| **Logical view-validity** | is this derived view still valid? | canonically-serialized rows | per-family keys, `logicalRoot` |

These are not a taxonomy of convenience. (A) and (B) prove the first two cannot be merged; (C)
and (D) prove the third is a different kind of object. **Nobody should re-attempt the merge**;
that is the main thing this ADR exists to record.

### D2 — Every address is a tagged fold over a declared decomposition

No bare hash of a concatenation is a valid address. `A(spec, children)` with the scheme tag
folded **into** the digest, not carried alongside it.

The precedent is already correct in-repo at exactly one site: `head_digest`
(`rs/ll-core/core/src/head_digest.rs:38-44`) uses `blake3::Hasher::new_derive_key(HEAD_DIGEST_CONTEXT)`
binding `(generation, rootHash, parentHash)`. Its test at lines 50-65 states the rationale:
`RootSigner::sign` takes a bare 32-byte `Hash` whatever its provenance, so without a context tag
a head signature would be bit-identical to a bare-root signature — *"the same key, two meanings."*

A label that rides *adjacent* to a digest is not bound to it and can disagree with the bytes
while everything still "verifies." Generalize the `new_derive_key` pattern to every identity
domain.

### D3 — Cross-structure binding is co-attestation, not derivation

Theorem 1 forecloses derivation. The `Head` therefore names the roots in one signed claim:

```
Head' = { rootHash, manifestRoot, logicalRoot, hierarchyId?,
          generation, parentHash, signature }
```

Additive fields per ADR-0014 §1 — an unset field must not change canonical bytes for existing
instances. `signature @5` (`head.capnp:55-66`) already set that precedent.

**Consistency is re-derivable, not assumed:** one linear pass streams the manifest's chunks
through BLAKE3, asserts it reproduces `rootHash`, and fills the bao outboard tree in the same
pass. Third-party re-executable — the `answer = f(root, query)` property applied to the roots
themselves.

`logicalRoot` is **required in v1**, not reserved. Binding a structure with no consumer produces
a sixth root nobody can derive; shipping a restriction family alongside it (bead `d25f6e`) makes
the field load-bearing on day one.

### D4 — Authority and dependency arrows

| Domain | Authoritative for | May depend on | Must NOT claim |
|---|---|---|---|
| Integrity (`current_root`) | the exact byte image of a snapshot | nothing | anything about logical content |
| Dedup (`manifestRoot`) | which regions changed; transport units | integrity (containment) | that it names the same thing as `rootHash` |
| Logical (`logicalRoot`) | derived-view validity | canonical row serialization | byte-level locality |
| SQL projection ABI | queryable tables/columns/indexes | none of the above | to be "the substrate" |

`nodes.record` remains the cross-runtime ABI (mache reads it directly, no cgo —
`docs/ARCHITECTURE.md:157`). The SQL projection is a **contract**, not an identity domain; it has
no root and should never be given one.

### D5 — One hash function

BLAKE3, per `substrate.rs:28-42`. No `sha2` in any substrate crate. ADR-0031 is **amended**, not
grandfathered — pre-1.0 clean break with a CHANGELOG entry (bead `b61cd6`).

Two stated exceptions, both INTEROP identifiers owned by external specs rather than substrate
addresses (`tools/check_doc_claims.sh` enforces this list exactly): `leyline-sign` (canonical
kid — sha256 appears in signet ADR-012's derivation lineage) and `leyline-envelope` (in-toto
Statement v1 subject digests — the in-toto spec names sha256 as the interop digest algorithm,
and byte-compatibility with rosary's emitted envelopes requires computing it; bead `be5f86`).
Neither hash ever names substrate content — σ remains BLAKE3 everywhere an address lives.

Deliberately **absent** from the descriptor: a per-address hash-algorithm field. Multihash-style
agility is an anti-feature under a locked `σ`; it reintroduces at the descriptor layer exactly
the ambiguity D2 exists to remove.

______________________________________________________________________

## Caveats — proven vs. mechanism-backed vs. judgment

**Proven.** Theorems 1 and 2. Impossibility (A) — flat non-compositionality follows from BLAKE3's
fixed-chunk tree and position-binding counter. (B) — verify/dedup irreducibility. (D) — identity 4
is a cover, not a partition. That identity 5 is already a fold (by ADR-015's construction).

**Mechanism-backed, one experiment from proven.** (C) row→byte instability under B-tree page
splits. **Recommended F-gate:** insert one unrelated row, count how many CDC chunks change.

**Engineering judgment, not proof.** Keeping `σ` flat and adding a bao outboard rather than
migrating `current_root` onto a manifest root. The exact `Head'` field set. Prolly trees as the
row index.

**Analogy only, and load-bearing on nothing.** Fibration/span framing. The Datomic lineage.
This ADR deliberately does **not** use the word "sheaf": ADR-0030 returned NO-GO on approximate
sheaf methods and ADR-0031 replaced it with an exact mechanism. The framing adds nothing
operational here and would invite the confusion those two ADRs settled.

______________________________________________________________________

## Cost

Measured on aarch64 (`rs/ll-open/cdc/examples/throughput.rs`): gearhash scan 1734 MiB/s; full CDC
including BLAKE3 of every chunk 1547 MiB/s — hashing every chunk costs ~12% over the bare scan.
Live arena 64 MB; legacy arena 1.0 GB.

Today the snapshot loop pays flat `O(arena)` BLAKE3 **every flip**. Under D1/D3: one-time entry
~41 ms at 64 MB (~0.68 s at 1 GB), with the outboard fill riding the same linear pass; steady
state is a ≤512 KiB rescan per edit plus a µs-scale fold. **Net: per-snapshot `O(arena)` becomes
amortized `O(edit)`** — strictly less work than today. Outboard storage ~6%.

`current_root` stays bit-identical to `blake3::hash(buffer)`, so every σ conformance pin at
`substrate.rs:324-343` still passes.

**Gate:** any proposal putting a full re-partition on the snapshot hot path is rejected unless it
*replaces* work already being done.

______________________________________________________________________

## Next moves (beads)

| Bead | Work |
|---|---|
| `b67a73` | `PartitionSpec` — the descriptor family (blocked on this ADR) |
| `b64505` | `Head.rootHash` framing defect — fold over segment addresses, not a concatenation |
| `b68fa6` | `Head'` co-attestation + the one-pass consistency verifier |
| `d25f6e` | Restriction family v1 — canonical row addresses populating `logicalRoot` |
| `b6ba16` | Prolly-tree index for row membership against `logicalRoot` |
| `b6a4dd` | bao outboard — incremental root + per-page verify-on-fault |
| `b61cd6` | Remove SHA-256 from `sheaf/src`; amend ADR-0031 |
| `d274a4` | Generalize signing to the co-attested `Head` (cloister) |
| `ef5c5a` | Reconcile README / ARCHITECTURE / TABLE_CONTRACT terminology to D4 |
| `df6402` | Cross-repo composition — its provenance chain is this operator |

______________________________________________________________________

## Non-goals

- **Extraction fidelity.** A fold notarizes wrong facts flawlessly. The witness's fidelity, not
  the record's integrity, is the ceiling — and this ADR does not raise it.
- **The signed-head authority problem.** Who is entitled to advance `R` is upstream of this and
  stays there.
- **Cross-head merge / confluence.**
- **Per-family restriction closure design.** The algebra gives the shape, not the family.
- **A universal value-traversal format.** The *descriptor* is zero-copy; traversal stays native
  to each value's format — SQLite pages are not capnp. IPLD-style universal traversal would
  smuggle back the translation layer the substrate exists to abolish.
