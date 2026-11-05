# ADR-0047 — Σ amendment: σ is indexed by construction; witnesses, four edge kinds, and the identity migrations

**Status:** Accepted (owner, 2026-11-04). Decisions D-i to D-iv below take the recommended
option; the alternatives and their costs are kept as the record.
**Bead:** `ley-line-open-619a99`
**Amends:** the Σ decade (`docs/decades/2026-merkle-cas-substrate.md` §1.1–§1.3, typed in
`rs/ll-core/core/src/substrate.rs`); ADR-0032 D2 (generalised from "every address is a tagged
fold" to "every construction is keyed"); ADR-0040 D1 and D6 (`treeRoot` splits into a
parser-free `storeRoot` plus the `derived_from` triple); ADR-0041 S6 (transport is named by
`storeRoot` alone).
**Related:** ADR-0031 (amended 2026-11-04: `leyline-cache`'s restriction key, the first shipped
lawful `Ref<Restriction>`), ADR-0034 D6 (content / address / locator / provenance), ADR-0045
(the git-shaped store), `ley-line-open-ae82a7` (the eight verbs), `ley-line-open-80f037`
(identity hardening from the derivability matrix).

Tree cited throughout: `origin/integration` at `e4690505` (v0.24.0 plus #521, #522). Code wins
where docs disagree; where an ADR's prose and the code differ, the code is quoted.

______________________________________________________________________

## Thesis

This is an amendment to Σ, not a second algebra. Σ = (𝓥, 𝓒, ρ, σ, R, S) already supplies
verification (σ with ρ and axiom (CA)), containment (§1.3 Merkle structure), versioned
advancement (§1.4) and attestation (S). What the shipped code shows Σ lacks is that **σ is not
one function**. Each identity LLO ships is a different *construction* over a value, and one
value has one address per construction: a file's bytes have a `blob_hash`, a CDC manifest
address, and (once parsed) an AST root `node_hash`. Σ is therefore indexed by construction,
σ_D, and each construction D is a declared descriptor folded into the key.

## 1. Σ indexed by construction

### 1.1 Definition

A **construction** D is a declared descriptor `D = (domain, scheme, params, canonVersion)`,
the `PartitionSpec` form (`rs/ll-core/core/src/partition.rs:80-90`), plus the encoding
`enc_D` of a value's children. The amended substrate is

```
Σ' = (𝓥, 𝓒, ρ, {σ_D}_{D ∈ 𝔇}, R, S)

σ_bytes(v) = BLAKE3(v)                                   the one unkeyed construction
σ_D(x)     = BLAKE3_derive_key(ctx_D, enc_D(x))          every other D
```

where `ctx_D` is either a context string owned by D alone (`HEAD_DIGEST_CONTEXT`,
`rs/ll-core/core/src/head_digest.rs:61`), or the shared `PARTITION_CONTEXT`
(`partition.rs:127`) with D's descriptor injectively in front of the entries
(`partition.rs:164-169`; pinned by `every_domain_is_separated_from_the_others`,
`partition.rs:700`, and `scheme_and_params_boundary_is_unambiguous`, `partition.rs:295`).
BLAKE3's derive-key mode hashes under different domain flags from plain mode, so a keyed
digest and a plain digest live in separate spaces.

`σ_bytes` is the base case. A value whose canonical encoding *is* its identity (a blob, a CDC
chunk, an arena image, a canonical Cap'n Proto message, canonical JSON) is addressed by
`σ_bytes ∘ canon`, and its type is carried by the typed slot that holds it, not by the digest.
That is lawful because equal addresses then mean equal bytes, which is the claim being made.
What is not lawful is a construction that intends separation and puts its tag *inside a plain
σ preimage* (§3, node_hash and run_id): it claims a separate space and does not get one.

### 1.2 The three laws

**(a) Within a construction, address equality ⇔ value equality.** `σ_D(x) = σ_D(y) ⇔ x ≡_D y`,
where `≡_D` is D's declared equivalence (set constructions erase enumeration order and nothing
else, `partition.rs:27-35`). Σ's (CR) gives ⇒ up to 2⁻¹²⁸; ⇐ is (DET) plus canonical encoding.

**(b) Across constructions, addresses are never compared.** `Ref<D₁> = Ref<D₂>` for D₁ ≠ D₂ is
a type error, not `false`. Two refs to one value are related by an edge (§2): Derives (present
the value, recompute both) or Attests (sign them together). "This file is blob X and manifest
M" is a checkable claim, never an equation (ADR-0032 Theorem 1: no root homomorphism).

**(c) Construction separation.** `σ_D₁(x) = σ_D₂(y) ⇒ D₁ = D₂` except with negligible
probability. Enforced by keying every construction except `σ_bytes`.

### 1.3 Falsifying test per law

| Law | Test (to add) | Fails today? |
|---|---|---|
| (a) | `construction_equality_is_value_equality`: for every registered construction, a proptest that distinct values under `≡_D` give distinct addresses and that every presentation of one value (enumeration order for sets; location, run and arena for content) gives one address; plus a known-answer vector per construction (§5, (DET)). The restriction key's `key_tests::injectivity` (`rs/ll-open/cache/src/restriction.rs`) is the model. | `node_content` rows fail (a): `lang` and `raw_kind` are not functions of `node_hash` (D-iii). The hashes themselves pass. |
| (b) | `ref_of_one_construction_does_not_compare_with_another`: a `trybuild` compile-fail test that `Ref<Blob> == Ref<AstNode>`, `Ref<Tree> → Ref<ArenaImage>` and any `From<Ref<A>> for Ref<B>` do not compile; plus a test that a `Loc` does not serialize without its version. | Not representable yet: one `Hash` carries every construction (`substrate.rs:58`). |
| (c) | `crafted_preimage_does_not_alias_across_constructions`: for every keyed construction D, `σ_bytes(enc_D(x)) ≠ σ_D(x)` on sampled x; includes the `ley-line-open-73add7` reproduction (an 18-byte file whose `blob_hash` equals the `node_hash` of the Rust leaf `fn`) as a fixed case. | Yes, for `node_hash` and `run_id` (D-i). |

## 2. Witnesses, `verify`, and the four edge kinds

### 2.1 One generic operation

Every portable identity is a total function of a witness (derivability matrix, 619a99 comment
1218: 14 W cells and 3 D cells cover all ten portable identities). Hence one operation:

```
verify : Ref<D> × Witness<D> → bool        verify(r, w) ⇔ σ_D(w) = r.addr
```

| D | `Witness<D>` |
|---|---|
| bytes (blob, chunk, arena image, canonical message) | the bytes |
| any `PartitionSpec` construction | `(spec, entries)`; Theorem 2 of ADR-0032 makes the entries enough |
| `node_hash` | `(tag, kind, token)` or `(tag, kind, child node_hashes)` |
| `storeRoot` (D-ii) | the directory nodes on the path being checked |
| head digest | the `HeadClaim` (`head_digest.rs:70-77`) |
| `run_id` | `(canonical spec digest, grant_id, replay_key)` |

ρ fails closed: a read returns bytes only after `verify` (Σ (CA) as verify-on-read). A locator
has no witness of its own; it resolves inside a version (§2.2, Locates).

### 2.2 The four edge kinds

| Edge | Form | Evidence | Shipped instances |
|---|---|---|---|
| **Derives** | `A → B` | B recomputed from A's witness plus public inputs | bytes → `blob_hash`, chunk hashes; `(manifest, parserId)` → `treeRoot`; `(bytes, parserId)` → `node_hash`; spec → `run_id` |
| **Contains** | `A ∋ B` | a path of witnesses from A down to an entry naming B (Σ §1.3) | `treeRoot ∋ (path, mode, blob)`; `node_hash ∋` child `node_hash`; blob manifest ∋ chunk |
| **Attests** | `A ⟷ B` | S over a keyed digest naming both (ADR-0032 D3) | `RunReceipt`/`EvidenceBinding` over `run_id` and spec digest; head digest v2 over `rootHash`, `treeRoot`, `parserId` (#517) |
| **Locates** | `(Ref<V>, k) → B` | resolution of local key k inside version V; the result is checked by B's own witness | `nid`, `content_id`, `file_id`, `name_id`, `kind_id` |

**Locators are valid only as `(version Ref, local key)`.** A bare locator replayed against
another generation resolves to a different row without error. No wire field, signed claim or
cache key carries a bare locator; one that does is a defect (`rs/ll-open/cli-lib/src/daemon/ops.rs:1480`
emits a raw `nid` as `node_id`, 80f037 F3).

### 2.3 Type shape (normative for new code)

`Ref<D: Construction>` is a `repr(transparent)` newtype over the 32-byte address with D as a
phantom; it is minted only beside its fold (no public `from_bytes`, no `ZERO`; absence is
`Option`), and no `From` exists between constructions. Arena-local ids are newtypes (`Nid`,
`FileId`, `DirId`, `NameId`, `KindId`, `ContentId`) with SQL impls and no serde or Cap'n Proto
impls. They live in `leyline-schema` (`rs/ll-core/schema/src/locator.rs`), which owns the nid
encoding and is Apache-2.0 and so cannot depend on AGPL `leyline-core`. A `Loc<V, K>` will pair
one with a `Ref<V>`; it is not built yet. `kid` stays outside Σ (SHA-256 of canonical SPKI, a
key's name). Cap'n Proto does not gain a construction-tag field beside a digest (a label
adjacent to a digest is the unbound label ADR-0032 D2 forbids); new identity fields get
distinct struct types, existing `Common.Hash` ordinals stay put (ADR-0014) behind typed
accessors.

## 3. Conformance: every shipped identity against the laws

| Identity | Construction (code) | (a) | (c) | Other | Verdict |
|---|---|---|---|---|---|
| `blob_hash` | σ_bytes over file bytes (`substrate.rs:145-147`) | yes | base case | — | **Complies** as the one unkeyed construction, conditional on every other construction being keyed |
| CDC chunk hash | σ_bytes over chunk bytes (`rs/ll-open/cdc/src/lib.rs:92`) | yes | base case | a sub-floor blob's single chunk has the blob's address: same construction, same bytes, lawful | **Complies** |
| `Controller.current_root` | σ_bytes over the arena image (`rs/ll-core/core/src/control.rs:68`) | yes | base case | — | **Complies** |
| spec digest (`blake3-256:`) | σ_bytes ∘ Cap'n Proto canonical form (`rs/ll-open/runtime/src/authorization.rs:343`) | yes | base case | typed by `DigestRef` | **Complies** |
| `confinementDigest` | σ_bytes ∘ canonical JSON (`rs/ll-core/schema-spec/confinement/v1/README.md:80`) | yes | base case | — | **Complies** |
| `parserId` | keyed `RowSet` fold `leyline/parser-id/v1` (`rs/ll-open/cli-lib/src/tree_root.rs:89-121`) | yes | yes | — | **Complies** |
| grammar digest | keyed fold `leyline/grammar-digest/v1` (`rs/ll-open/ts/src/languages.rs:166,288`) | yes | yes | — | **Complies** |
| `Head.rootHash` | keyed fold `leyline/segment-root/v1` (`rs/ll-open/cli-lib/src/cmd_parse.rs:2644,2725`) | yes, over segments | yes | a run receipt, not a store identity (carries absolute paths and mtimes) | **Complies** |
| head digest v2 | own context `leyline head digest v2` (`head_digest.rs:61`) | yes | yes | binds `treeRoot`, `parserId`, `parentDigest` (#517) | **Complies**; moves to v3 under D-ii |
| restriction key | keyed `RowSet` fold, rows under `derive_key("leyline/restriction/v1")`, `parserId` in params (`rs/ll-open/cache/src/restriction.rs:87-103,200-214`) | yes (proptest) | yes | whole-object arm passes `Hash::ZERO` as params (`restriction.rs:740`): lawful (a distinct construction), but a sentinel where `Option` belongs | **Complies**: first shipped lawful `Ref<Restriction>` |
| `node_hash` | plain σ over `"llo/ast/v1" ‖ 0x00 ‖ tag ‖ …` (`cmd_parse.rs:3270,3297-3325`) | hash yes; `node_content` row no (`lang`, `raw_kind`, `rs/ll-open/cli-lib/src/cmd_rebuild.rs:368`) | **no**: prefix-in-preimage, reproduced collision (`73add7`) | — | **Violates (c)** → D-i; row violates (a) → D-iii |
| `run_id` | plain σ over `"cloister/execution/v1/run-id" ‖ 0x00 ‖ …` (`authorization.rs:864,912-925`) | yes | **no**: prefix-in-preimage | pinned cross-repo by `execution/v1/test-vectors/run-id.json` | **Violates (c)** → D-i |
| `Head.treeRoot` | keyed `RowSet` fold `leyline/tree-root/v1`, **`params = parserId`**, flat over 2n entries (`tree_root.rs:157-182`) | yes, for the (store, parser) pair | yes | **violates parameter independence** (a grammar, epoch or projection bump renames an unchanged store, `73c6ee`) and **Σ §1.3** (one flat fold: an edit or a membership proof costs O(n), not O(log n)) | **Non-conforming** → D-ii |
| blob manifest | rootless rows keyed by `blob_hash` (`rs/ll-open/fs/src/blob_chunked.rs:53-64`) | — | — | no address; sub-floor blobs never enter the pool (619a99 F3: 572 of 954 blobs on `rs/`) | **Not an identity yet** → D-iv |
| locators (`nid`, `content_id`, interned ids) | arena-local integers (`rs/ll-core/schema/src/lib.rs:86-98,138-143`) | n/a | n/a | one bare-`nid` wire leak (`daemon/ops.rs:1480`) | **Complies except the leak** (80f037) |

**Headline.** Of 15 shipped identities, 10 comply; `node_hash` and `run_id` violate
construction separation (both prefix-in-preimage); `treeRoot` violates parameter independence
and §1.3 (parser in params, flat fold); `node_content`'s `lang`/`raw_kind` violate (a); the
blob manifest has no address; one locator leaks bare onto the wire. Plain `blob_hash` is the
single unkeyed base case and stays so.

## 4. Decisions

Each decision names what moves and its version bump so that an old artifact and a new one are
never confused, and how an old artifact is refused or rebuilt. Pre-1.0 policy applies: the old
construction is removed, not deprecated, with a CHANGELOG `Breaking` fragment. Downstream
changes are filed as issues **before** the LLO implementation PR merges (§4.5).

### D-i — `node_hash` and `run_id` move to keyed constructions (`73add7`)

**Options.** (a) Move both to `derive_key` contexts: an identity migration. (b) Keep the
hashes; make separation a type (`Ref<D>`, stores keyed by `(construction, addr)`). (c) Move
`node_hash` only; keep `run_id` prefixed and typed by its `run-` rendering.

**Decision: (a), in two separate bumps.** (b) is adopted in any case (§2.3), but types do not
reach a peer that receives bare digests, a CAS keyed by 32 bytes, or a chunk pool shared across
constructions; law (c) must hold of the bytes. (c) leaves the same defect in the one identity
that crosses a trust boundary.

**Consequences.** Every `node_hash` changes; cross-arena comparisons with old arenas stop
matching (intended: they are a different lineage). `run_id` changes for every run; evidence
bound to an old `run_id` stops verifying (fail closed, since verifiers recompute the id).

**Versioning and migration.**
- `node_hash` — moves: `node_content.node_hash`, the v10 compatibility views (`v_ast_hash`,
  `v_node_defs_hash`, `v_node_refs_hash`), `parserId` (it folds `IR_SCHEMA_VERSION`), the
  `derived_from` stamp. Does not move: `Head.rootHash` (no AST segment field carries a node
  hash, `rs/ll-core/schema-capnp/schemas/ast.capnp:29-43`), and after D-ii lands, `storeRoot`.
  Construction: `derive_key("leyline/ast-node/v3", tag ‖ uvarint|kind| ‖ kind ‖ …)`, the
  current preimage without the in-band prefix. Bump: `IR_SCHEMA_VERSION`
  `merkle-ast-v2 → merkle-ast-v3` (`rs/ll-open/cli-lib/src/daemon/version.rs:72`), which mache
  already records (`mache/cmd/build_meta.go:60-75`). Old artifact: an arena whose
  `_meta.ir_schema_version` is `merkle-ast-v2` fails `leyline rebuild`'s `parserId` check and
  the incremental guard, and is re-parsed cold; nothing converts old hashes.
- `run_id` — moves: every derived run identity and the pinned vector. Construction:
  `derive_key("cloister/execution/v1/run-id/v2", u64le|spec| ‖ spec ‖ u64le|grant| ‖ grant ‖
  u64le|replay| ‖ replay)`, rendered `run-v2-<64 hex>`. Bump: the vector file gains the v2
  derivation and loses v1 (`rs/ll-core/schema-spec/execution/v1/test-vectors/run-id.json`);
  the rendered prefix changes so a v1 id fails `is_run_id`'s shape gate as malformed rather
  than as a mismatch (`authorization.rs:885-892`). Prerequisite: `leyline-cas-ffi` exports a
  derive-key entry point beside `leyline_hash_bytes` (`rs/ll-open/cas-ffi/src/ffi.rs:34`), since
  cloister derives `run_id` in TypeScript through that FFI.
- Order: cas-ffi export (additive) → LLO run-id v2 with vector → cloister re-vendors and pins.
  `node_hash` v3 ships after D-ii and together with D-iii (§4.6, M2).

### D-ii — a parser-free `storeRoot` with a Merkle construction, as one store-identity migration (`73c6ee`)

**Options.** (A) `storeRoot` = a directory Merkle tree over the git-shaped store, parser-free;
the parser-qualified identity is the `derived_from` triple, not a hash. (B) The same flat fold
with empty params: fixes parameter independence, not §1.3, and forces a second store-identity
migration later. (C) A balanced or content-defined (prolly) tree over sorted entries: O(log n)
regardless of directory width, but positions shift on insert (balanced), or chunking
parameters enter the identity (prolly), which reintroduces a tunable into a content address.
(D) Status quo.

**Decision: (A).** Each directory node is a keyed `RowSet` fold under
`leyline/store-tree/v1`, params empty, over its entries sorted by name bytes: per entry
`(σ_bytes(name), a = 2i, b = |name|)` and `(child, a = 2i + 1, b = mode_tag(mode))`, where
`child` is the `blob_hash` for a file or symlink and the child directory's node address for a
directory; an empty directory is the fold of no entries. `storeRoot` is the root directory's
address. A subtree's address is the `storeRoot` of a store holding exactly that subtree, so a
sub-manifest (ADR-0040 D2) is named by its own root. Contains proofs are the directory nodes on
one path: O(depth × fanout). That meets Σ §1.3 for bounded fanout; a single directory with
very wide fanout costs O(fanout), which the §1.3 falsifier (§5) measures; a fanout split for
wide directories would be `store-tree/v2`.

**Consequences.** A grammar, epoch or projection bump no longer renames the store, so open
proposals stay valid across a parser upgrade (they carry `storeRoot` bases). `treeRoot` is
removed; `derived_from = (storeRoot, parserId, projection_schema_version)` carries the
derivation. ADR-0041 S6 simplifies: the transport unit is named by `storeRoot` alone, and
blobs verify against it without the sender's parser.

**Versioning and migration.**
- Moves: `Head.treeRoot @7` is no longer written; `Head.storeRoot @10` is added
  (`rs/ll-core/schema-capnp/schemas/head.capnp:118,127,139`; ordinals are never reused, ADR-0014).
  Head digest `v2 → v3` (`leyline head digest v3`, binding `storeRoot` in `treeRoot`'s
  position); a v2 signature does not verify under v3 because the context differs. Daemon wire:
  `tree_root` (`rs/ll-core/public-schema/capnp/daemon.capnp:299,451`) is replaced by
  `store_root` under new ordinals; `compatibility.json`, the generated Go bindings and
  `server.json` are regenerated. `_meta.derived_from`'s first component becomes `storeRoot`.
  Bump: `PROJECTION_SCHEMA_VERSION` `projection-v10 → projection-v11` (`version.rs:175`), and the
  release `SCHEMA_VERSION` (`version.rs:42`).
- Old artifact: `leyline verify-head` on a head with no `storeRoot` reports it unstamped and
  refuses; `leyline rebuild` refuses a v10 stamp; the daemon re-parses a v10 arena cold.
  Proposals naming a `tree_root` base are refused as malformed at the wire.
- Order: mache issue filed → LLO M1 PR (scheme, head digest v3, wire, projection-v11, ADR-0040
  D1/D6 and ADR-0041 S6 text) → mache pins.

### D-iii — `lang` and `raw_kind` leave `node_content` and stay per occurrence (`b5d354`)

**Options.** (A) Put the language (and raw kind) into `node_hash`'s construction, a per-language
σ_D: rows split, every `node_hash` moves, cross-language sharing of κ-trees ends. (B) Drop
`lang` and `raw_kind` from `node_content`; they are occurrence facts, already carried per
occurrence by `_ast.kind_id → kinds(lang, raw_kind)` (`rs/ll-open/ts/src/schema.rs:44-54`,
`rs/ll-core/schema/src/lib.rs:138-143`). (C) Status quo with the rebuild-gate exclusion.

**Decision: (B).** The construction `node_hash` names is the κ-tree (canonical kind, token,
children; `cmd_parse.rs:3931-3936`). `lang` and `raw_kind` are not in that value, so by law (a)
they cannot be columns of its content row. (A) changes what the address means to fix a
storage defect; (C) keeps a row that is not a function of its key.

**Consequences.** `node_content` becomes a function of `node_hash`; `cmd_rebuild.rs:368`'s
exclusion is deleted and the gate compares every column (`b5d354`'s acceptance). The `R3`
storage cut of `ley-line-open-9c1af2` comes with it. Precondition the implementation proves: a
consumer reading a child's raw kind reads it from the child occurrence, and for content rows
no occurrence reaches, the raw kind is recoverable or not needed (test: a re-parse reproduces
every per-occurrence `(lang, raw_kind)`).

**Versioning and migration.** Moves: two `node_content` columns (`schema.rs:404-411`). Does not
move: any hash or root. Bump: one projection version, shared with D-i's `node_hash` v3 (§4.6).
Old artifact: a projection-v11 arena is re-parsed cold by a v12 binary; mache's projection map
learns v12.

### D-iv — chunks-primary keeps `blob_hash = σ_bytes(bytes)` (option 1) (`58cf9a`, decides the `d0781b` hold)

**Options.** (1) The blob's address stays σ_bytes; its CDC manifest gains its own keyed
address, related to the blob by Derives (checked at write: stream the chunks through σ,
compare with `blob_hash`, ADR-0032 D3's consistency pass). (2) The blob's address becomes the
manifest fold.

**Decision: (1).** Option 2 makes every blob address, every `Base::Blob`, every store-tree leaf
and every grant a function of CDC tunables (parameter independence), and ADR-0032 (A) proves a
manifest fold can never equal σ_bytes, so it is an identity migration of the whole store with
no gain in verification. Under (1) the manifest is a second construction over the same value,
which is exactly what §1 says one value may have.

**Consequences.** Construction: `Ref<BlobManifest>` = keyed `ByteStream` fold under
`leyline/blob-manifest/v1`, params = the canonical CDC parameters (`MIN_CHUNK`, `MAX_CHUNK`,
boundary mask, gear-table digest), entries = `(chunk hash, offset, len)`. A tunable change is a
new manifest address and never a new `blob_hash`. Sub-floor blobs become one-chunk manifests in
the same pool (619a99 F3). `nodes.record` stays a declared derived column until SQL consumers
read spans from chunks. **`d0781b`: keep the incremental rechunk API**; it is chunks-primary's
write path (an edit costs the rehashed window). `d0781b` stays held, blocked on the
chunks-primary build; it closes as not-removed when that build uses the API.

**Versioning and migration.** Moves: no blob, store or AST identity. Adds the manifest
address. Bump: one projection version (the `blob_manifest` DDL, `blob_chunked.rs:53-64`, gains
its root and the one-chunk rows). Old artifact: a pre-bump CDC index is rebuilt from
`source_blobs` (it is derived; ADR-0033 D1). Removing `source_blobs.blob_bytes` is a later,
separate decision with its own bump; it has verified downstream readers (§4.5).

### 4.5 Downstream issues (file BEFORE implementation)

Only consumers verified in code are listed. Checkouts read: mache `872216f` (pins v0.24.0,
projection-v10), cloister and notme at their current heads.

| Decision | Repo | Change | Acceptance |
|---|---|---|---|
| D-i `node_hash` | mache | Existing `mache-99568c`: the smell baseline is keyed `(rule_id, node_hash)` (`internal/smells/smell_ratchet.go:64-85`) and must record its `ir_schema_version` and refuse a cross-lineage diff; the first pin of `merkle-ast-v3` regenerates `docs/smell-baseline.json` once. | `mache-99568c`'s (1)–(4), with (4) taking option (a). |
| D-i `run_id` | cloister | Re-implement `src/wire/run-id.ts` for run-id v2 (derive-key, `run-v2-` prefix) through the new cas-ffi export in `src/wire/cas-hash.ts`; re-vendor `test/fixtures/llo-execution-v1/run-id.json` and `test-vectors/run-id.json`; update the domain assertion at `scripts/test/llo-execution-conformance.test.mjs:131`. `rs/crates/host-runtime/src/execution.rs:225` mints `run-<bundle>` and is already tracked as `cloister-af0761` (unchanged by this ADR). | `test/wire/run-id.test.ts` reproduces the v2 vector byte for byte; a v1-shaped id is refused as malformed. |
| D-ii | mache | `internal/leyline/propose.go:28,50,84,115` read `store_root` instead of `tree_root`; `propose_wire_test.go:58` asserts it; `graph/projection_version.go` learns `projection-v11`. | Propose and read_source round trips against the M1 release; a response carrying only `tree_root` is refused. |
| D-iii | mache | `internal/lloschema/child_edges.go:6-27` reads the child's `raw_kind` from `node_content` (`cc.raw_kind`); read it from the child occurrence's `kind_id → kinds` instead. `internal/fixturedb/emit.go:84` and the fixture DDL stop writing `node_content.lang`/`raw_kind`. Projection map learns the D-i/D-iii version. | FieldChildJoins results equal before and after on mache's fixtures; fixture DDL matches LLO's. |
| D-iv | mache | None for D-iv itself. When `source_blobs.blob_bytes` is removed (a later decision), `graph/parse_blobs.go:44`, `internal/lloschema/tree.go:73` and `internal/lloschema/source_bytes.go` lose their read and need a read path. | Filed with that later decision, not now. |
| all | mache | Existing `mache-701e21`: amend mache ADR-0011 (Pointer) to this ADR (SHA carries its construction, verified Read, versioned Record, no cross-kind equality). | `mache-701e21`'s criteria, reviewed against this ADR. |
| hygiene | mache | `internal/fixturedb/emit.go:298` writes `source_blobs.blob_hash` as SHA-256 of the content: a fixture whose blob addresses fail LLO's verify-on-read. Use BLAKE3. | Fixture blobs verify under σ_bytes. |
| — | notme | No consumer. Its `run_id` is the GitHub Actions OIDC claim (`worker/src/gha-oidc.ts:50`), unrelated to LLO's. | — |
| — | cloister | No consumer of `node_hash`, `treeRoot`, `blob_hash` or the head digest (grep). | — |

### 4.6 Implementation sequence

Each step is one versioned migration; downstream issues for a step are filed before its PR
merges.

| Step | Contents | Bump | Identities moved |
|---|---|---|---|
| M0 | `Ref<D>`, `Witness<D>`, `verify`, locator newtypes in `leyline-schema`; the law tests that hold today (§1.3) and the axiom suite (§5); cas-ffi derive-key export | none (additive) | none |
| M1 | D-ii: `storeRoot` (`store-tree/v1`), head digest v3, wire `store_root`, `derived_from` v2 | `projection-v11`, head digest v3, release | `treeRoot` removed, `storeRoot` added |
| M2 | D-i `node_hash` + D-iii, **one bump**: both rewrite `node_content`, both need one mache re-pin; `9c1af2`'s R3 rides along | `merkle-ast-v3`, `projection-v12` | `node_hash`, `parserId` (not `storeRoot`, since M1 made it parser-free) |
| M3 | D-i `run_id` v2 | run-id vector v2, release | `run_id` |
| M4 | D-iv: rooted blob manifests, one-chunk sub-floor manifests | `projection-v13` | none moved; manifest address added |

M1 precedes M2 so that the `IR_SCHEMA_VERSION` bump in M2 does not rename the store. M3 is
independent of M1/M2 and may run in parallel with them. Law (c)'s test lands green at the end
of M3; until then it carries the two known failures as named cases that M2 and M3 close.

## 5. The axiom-falsification suite

`substrate.rs:20-22` records that the "axiom → executable falsification" bead (decade T1.3) was
never filed. These are its tests; each runs in `task ci`.

| Axiom | Test | Scope |
|---|---|---|
| (CA) ρ(σ(v)) = v | `ca_round_trip_every_store`: proptest over arbitrary bytes (empty, sub-floor, ≥ `MAX_CHUNK`) through every ρ: `source_blobs` via storefs read, the chunk pool (`SqliteBlobStore`), blob-manifest reconstruction. `ca_tampered_payload_is_refused`: flip one stored byte; every ρ fails closed (decade F3). | `rs/ll-open/fs`, `rs/ll-open/storefs`, `rs/ll-open/cdc` |
| (DET) | `det_known_answer_vectors`: one checked-in vector per construction (σ_bytes, each `PartitionSpec` construction, `parserId` on fixed inputs, `storeRoot`, `node_hash`, segment root, head digest, restriction key, `run_id`), consumed by Rust tests and published under `schema-spec` for Go and TypeScript consumers; run on Linux and macOS CI. `det_parse_is_thread_count_invariant`: parse a fixture tree with one and with N rayon threads; equal `node_hash` sets, `storeRoot`, `rootHash` modulo absolute paths. | `rs/ll-core/core`, `rs/ll-core/schema-spec`, `rs/ll-open/cli-lib` |
| (IM) | `im_address_is_never_rebound`: a proptest over write sequences; for every address-keyed table (`source_blobs`, `node_content`, `content_chunks`, `capnp_blobs`) the bytes first stored under an address never change and still satisfy σ = address. `im_sigkill_mid_publish_never_exposes_torn_root` (decade F1): kill a writer at random points during publish; every observed root resolves to a complete image that hashes to it. | `rs/ll-open/cli-lib`, `rs/ll-open/fs` |
| §1.3 Merkle | `merkle_one_leaf_edit_rehashes_one_path` (decade F4): a 50,000-file store, edit one file, count fold invocations; bound c · depth · fanout. Fails on today's flat `treeRoot` (rehashes 2n entries); its passing is M1's closing test. `node_hash_edit_touches_depth_plus_one_rows`: one leaf edit adds at most depth + 1 `node_content` rows (passes today). | `rs/ll-open/cli-lib` |

## 6. Verb mapping (`ae82a7`)

| Verb | Edge |
|---|---|
| Ref | the vertex: `Ref<D>` |
| View / Set | Contains (a set or tree Ref and its members, with proof) |
| Resolve | Locates: `(storeRoot, path) → Ref<Blob>`, `(version, nid) → row` |
| Materialize | Derives with verify-on-read: a Ref to bytes or a checkout |
| Transform | Derives: `(inputs, Ref<Function>) → output` (parse under `parserId`; execution/v1 `RunSpec → RunReceipt`) |
| Propose | a new version Ref plus a Derives edge from its base (Σ §1.4 CAS step) and Contains for the changed entries |
| Observe | Attests: a signed claim relating Refs (the head) |
| Interrupt | no edge of its own: a message whose payload is a Ref, addressed by a Derives-constructed id over the target `run_id` |

## Non-goals

- A hash-algorithm field or multihash agility (Σ is BLAKE3-locked; `partition.rs:37-42`).
- Converting any old identity into a new one. Every migration re-derives from witnesses.
- `manifestRoot` / `logicalRoot` (ADR-0032 D3, D4): store-wide roots stay Proposed; D-iv's
  per-blob manifest address is a different, narrower construction.

______________________________________________________________________

## Appendix A — derivations

**A.1 A domain-free handle is unsound.** Let one untyped `Hash` name every identity. For a file
v, `blob_hash(v) = σ_bytes(v)` and its manifest address `M(v)` differ for every v (ADR-0032
(A)), and no map relates them (Theorem 1), so the handle space holds two names for one value
with no rule between them: completeness fails. Conversely the 73add7 file gives one name for
two values (a blob and an AST leaf): soundness fails. So the handle carries its construction,
and equality is typed. ∎

**A.2 Why prefix-in-preimage is not separation.** A preimage prefix p separates D from other
*prefixed* constructions with distinct prefixes, but σ_bytes accepts every byte string,
including `p ‖ enc_D(x)`. So σ_bytes(p ‖ enc_D(x)) = σ_D(x) by definition, for any author who
writes those bytes as a file. Keying moves σ_D off σ_bytes's domain flags; no byte string
reaches it through σ_bytes.

**A.3 Why `σ_bytes ∘ canon` is lawful.** If D's value is identified by its canonical bytes c,
an author file with bytes c is the same bytes; σ_bytes(c) naming both is law (a) holding, not
(c) failing. The type of the holder (a `DigestRef` slot, a `confinementDigest` field) says how
to interpret them.

**A.4 Directory Merkle cost.** For a path of depth d with fanouts k₁…k_d, an edit refolds d
nodes of total size Σkᵢ entries; for bounded k this is O(k log_k n), Σ §1.3's bound. The flat
`tree-root/v1` fold refolds 2n entries for any edit.

Full derivations, the derivability matrix and the storage measurements are in the 619a99
comments (1212, 1218, 1219, 1220, 1221).
