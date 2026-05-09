# ADR-0014 design analysis — capnp-as-protocol + schema-evolution rules

**Status:** Working analysis (NOT the ADR). Synthesis input for `docs/adr/0014-capnp-as-protocol.md`.
**Decade:** `ley-line-open-9d30ac` (Σ Merkle-CAS substrate)
**Thread:** `T8/capnp-as-protocol`
**Bead:** `ce8fd1` (T8.6)
**Date:** 2026-05-08
**Author:** theoretical-foundations-analyst (synthesis target: James Gardner)

---

## 0. Setup

The artifacts under analysis:

| Schema file | Top-level struct | Producer call site | Local SQL projection |
|---|---|---|---|
| `rs/ll-core/schema-capnp/schemas/common.capnp` | `Position`, `Range`, `Hash`, `NodeRef` | (used transitively) | (used transitively) |
| `rs/ll-core/schema-capnp/schemas/binding.capnp` | `BindingRecord` | `rs/ll-open/lsp/src/project.rs:565-604` (`write_binding_record`) | `_lsp_refs` table |
| `rs/ll-core/schema-capnp/schemas/ast.capnp` | `AstNode` | `rs/ll-open/cli-lib/src/cmd_parse.rs:733-760` (`write_ast_node_record`) | `_ast` table |
| `rs/ll-core/schema-capnp/schemas/source.capnp` | `SourceFile` | `rs/ll-open/cli-lib/src/cmd_parse.rs:705-731` (`write_source_file_record`) | `_source` table |
| `rs/ll-core/schema-capnp/schemas/head.capnp` | `Head` | `rs/ll-open/cli-lib/src/cmd_parse.rs:619-656` (`write_head_after_parse`) | (analogue of `Controller::current_root`, `rs/ll-core/core/src/control.rs:145`) |

The Σ-root chain is computed in `cmd_parse.rs:548-655`. Segment-suffix
canonical order is the constant `SEGMENT_FILE_SUFFIXES` at lines
557-561: `["source.capnp", "ast.capnp", "bindings.capnp"]`. The hash is
`BLAKE3(concat of file bytes in this order)` per `hash_segment_files`
(lines 568-582).

The cross-repo proof point: `~/remotes/art/mache/schemas/{common,binding}.capnp`
is byte-identical to the LLO copies modulo three Go-binding annotations
(`using Go = import "/go.capnp"`, `$Go.package(...)`, `$Go.import(...)`),
none of which alter struct ordinals or layout. Generated bindings live
at `~/remotes/art/mache/internal/lsp/bindings/binding.capnp.go`.

The be6136 origin (canonicalization at producer site) is fixed at
`rs/ll-open/cli-lib/src/cmd_parse.rs:359-361`:

```rust
let canon = abs_path.canonicalize().unwrap_or_else(|_| abs_path.clone());
let abs_str = canon.to_string_lossy().to_string();
```

This is the textbook example of a *latent projection invariant* — the
schema field `SourceFile.canonicalPath` documents the post-canonicalize
contract (`source.capnp:21-23`), and the producer is responsible for
maintaining it. Section 2 formalizes what the consumer can rely on as a
result.

---

## 1. Compatibility classes — formal definitions

### 1.1 Notation

Let `S` denote a schema (a tuple of struct definitions, each carrying
ordered field ordinals with types). Let `wire(S)` be the set of
byte-strings that are valid messages under `S`. Let `Π(S, b)` denote the
typed reading of bytes `b` under schema `S` — i.e. the partial function
that yields a typed Reader if `b ∈ wire(S)` and ⊥ otherwise.

For a typed reader `r : Π(S, b)` and a field `f` of `r`'s root struct,
let `r.f` be the value at field `f` (using the schema-default if `f` was
absent from the producer's bytes — capnp's lazy field-presence model).

### 1.2 Three compatibility relations

| Relation | Definition | What it preserves |
|---|---|---|
| **Forward** `S_p →ᶠ S_c` | For every `b ∈ wire(S_p)`, `Π(S_c, b) ≠ ⊥` and reading any field `f` defined in `S_c` gives the value the producer set (or `S_c`'s default for `f` if absent in `S_p`) | A producer running `S_p` (newer) writes bytes a consumer running `S_c` (older) can still parse. New fields are silently ignored by the older consumer. |
| **Backward** `S_p →ᵇ S_c` | For every `b ∈ wire(S_p)`, `Π(S_c, b) ≠ ⊥` and reading any field `f` defined in `S_p` either returns the producer's value or `S_c`'s default | A producer running `S_p` (older) writes bytes a consumer running `S_c` (newer) can still parse. Newer fields the producer didn't set show as defaults. |
| **Full** `S_p ↔ S_c` | Both `S_p →ᶠ S_c` and `S_p →ᵇ S_c` | Either side can be older. |

Capnp's published evolution rules give us **both** directions when, and
only when, the schema authors obey three primitive operations. Define
the **append-only-additive** subgroup `A` of schema edits:

- **Op-A** (field append at next ordinal): `S → S' = S ∪ { @N : T = default }` where `N = max(ordinals(S)) + 1`.
- **Op-G** (struct group / union variant append): same as Op-A but on a group/union.
- **Op-D** (deprecation comment): non-semantic edit (no wire change).

If `S → S'` is a finite sequence in `A*`, then **both** `S →ᶠ S'` and
`S →ᵇ S'` hold (capnp guarantees this; the substrate inherits it).

### 1.3 What "compatibility" alone does NOT give us

This is the crux that ADR-0014 must articulate. The above guarantees
**bytes parse** and **defaults compose**. They do not guarantee:

(a) **Semantic stability of a known field.** A consumer reads `r.refUri`
    on a `BindingRecord`. The schema didn't change `refUri`'s ordinal
    or type, but the producer code was edited to emit a non-canonical
    URI. The consumer's `Π` is happy; the *meaning* drifted. This is
    exactly the be6136 failure pattern, just relocated from SQL columns
    to schema fields.

(b) **Equivalence-class preservation.** The schema declares
    `BindingRecord.constructNodeId @2 :Text` (binding.capnp:38).
    Producer emits the empty string when no enclosing construct exists
    (`project.rs:469`); consumer treats `""` as "missing." If a future
    producer emits `"<none>"` instead — schema-compatible, type-stable —
    the consumer's missing-check breaks. The equivalence class
    `{absent, "", "<none>"} → "missing"` was not on the schema, only
    in producer/consumer convention.

(c) **Cross-field consistency.** `BindingRecord.refRange` (a
    `Common.Range`) and `BindingRecord.refUri` together must point at
    the same byte range of the same file. Schema compatibility doesn't
    police that.

(d) **Hash stability of the bytes.** Capnp's wire format is
    word-aligned. Adding a field at `@N` typically increases the data
    section or pointer section, which changes the byte layout of every
    instance — even if the producer never sets the new field. This
    matters for T8.5's segment-hash chain (Section 3.5).

### 1.4 The producer-future-consumer-past skew

The user's last bullet asks: *"do all fields the consumer knows still
mean the same thing?"* Let `S_c ⊏ S_p` denote that `S_c` is a prefix of
`S_p` in the append-only-additive ordering (`S_c` is older). The
guarantee is:

> **Capnp guarantee.** For every field `f ∈ fields(S_c)`, every
> producer-message under `S_p`, and every consumer reading under `S_c`,
> the typed value `r.f` is exactly the value the producer set (or the
> schema-default if absent), and its **type** is exactly the type
> declared in `S_c`.
>
> **What's NOT guaranteed.** That the value carries the same *meaning*
> as it did under `S_c`. Meaning is producer-controlled.

Concrete: if a future schema `S_p` adds a field `parseGenerationStrict @7`
and the LLO producer changes its semantics so `parseGen @6` now means
"approximate generation, may skip" while `parseGenerationStrict` is the
authoritative one, an `S_c`-running mache reads `parseGen` and gets a
field whose meaning has been silently weakened. The schema's compat
guarantees are intact; the contract is broken.

This is the failure mode the **"never repurpose a field's meaning"**
rule (Section 3) is designed to prevent — it's not a capnp property,
it's a producer-discipline property the ADR must police.

---

## 2. The protocol/projection invariant

### 2.1 Formalization

Let `M` be the type of a top-level capnp record (e.g. `BindingRecord`).
Let `T` be the type of a SQL row in a local projection table (e.g. a row
in `_lsp_refs`).

Define two functions:

```
proj_p : (producer state)  →  M       -- the producer's "what to emit"
proj_c : M  →  T_c                    -- the consumer's local-index derivation
proj_p_sql : (producer state)  →  T_p -- the producer's parallel SQL write
```

The L2/L3 reframe says: **the contract is `proj_p`, not `proj_p_sql` and
not `proj_c`**. Two consumers can derive different `T_c`s and that's
fine. The producer's parallel `proj_p_sql` is for *its own* local
queries — it is NOT part of the contract.

### 2.2 What "faithful" means

A projection `π : A → B` is **faithful** if it's *injective on the
relevant equivalence classes* — i.e. if two semantic states `a₁ ≢ a₂`
in the producer's space map to distinguishable `π(a₁) ≠ π(a₂)` in the
target. Loss of injectivity is OK if and only if the consumer never
needs to distinguish the conflated states.

For each schema field, the ADR should classify whether the producer's
projection into that field is required to be:

| Class | Property | Example field |
|---|---|---|
| **Identity** | A bijection from a canonicalized producer-side value | `BindingRecord.refUri` (file URI, post-canonicalize) |
| **Injective** | Producer-side distinct values map to distinct field values, but the field carries extra structure | `AstNode.nodeId` (path-shape, `pkg/auth.go/...`) |
| **Lossy-by-design** | Strips fields the consumer doesn't need; documented loss | (none currently — but a future "anonymize ranges" mode would be here) |
| **Cosmetic** | Human-readable, no consumer relies on it for joins | (none currently — every field is currently load-bearing) |

### 2.3 The be6136 reframe

be6136's failure was that `proj_p_sql` (LLO's `_source.path` write,
pre-fix) and `proj_c` (mache's JOIN against `_source.path`) used
different equivalence classes for paths. Specifically:

- `proj_p_sql_old(state)._source.path = abs_path.to_string_lossy()` — pre-canonicalize
- `proj_c(LSP file:// URI) = strip("file://", uri)` — post-canonicalize (LSP canonicalizes)

The path equivalence class `{/tmp/foo, /private/tmp/foo}` was crushed
to one representative on the LSP side and left as two on the producer
side. JOIN missed; bug.

In the L2/L3 capnp reframe, both producer and consumer talk about
`SourceFile.canonicalPath`, **whose schema docstring locks the
representative**: "Canonicalized absolute path. Equivalent to
`_source.path` post-be6136" (`source.capnp:22-24`). The producer's
obligation is to populate `canonicalPath` with a single canonical
representative (its job — `cmd_parse.rs:359-361` does exactly this);
consumers copy it verbatim. There is no JOIN against a column that
might have been written before canonicalization.

**Formal statement of the invariant ADR-0014 should commit to:**

> **(F1 — Contract = Schema)** For every field `f` of every top-level
> capnp record, the schema's docstring is the *contract* for `f`'s
> equivalence class and canonicalization rules. Producer code MUST
> respect the docstring's class. Consumers MAY rely on it.
>
> **(F2 — Projection commutativity)** If `proj_p` and `proj_c` are both
> faithful relative to their docstring contracts, then for any
> well-typed query `q` consumers issue against `T_c`, the result is
> derivable solely from the typed capnp records — no consumer-side
> canonicalization or producer-side guesswork is required.
>
> **(F3 — SQL is downstream)** Any local projection (LLO's `_lsp_refs`,
> mache's canonical-view) is computed from typed capnp records,
> downstream of `proj_c`. SQL columns are not the contract; their
> column names, types, and constraints can drift independently of the
> capnp schema.

### 2.4 Where `proj` may legitimately lose information

| Situation | OK? | Reasoning |
|---|---|---|
| Producer drops a field's *internal* representation when emitting (e.g. tree-sitter Tree gets compressed to `node_id` path-shape) | **OK** | The contract is the path-shape Text; the Tree internals are not consumer-visible. |
| `BindingRecord.refRange.start.byte` is `0` because LSP only gives line/col (`project.rs:591`) | **OK with caveat** | The schema says "byte is canonical"; setting it to 0 is a documented loss. Consumers using byte ranges for joins must guard against `byte == 0`. |
| Producer writes empty `contentHash` when `compute_hash` is disabled (`cmd_parse.rs:725-727`) | **OK with caveat** | Schema says "Empty `bytes` if not yet populated". Consumers MUST treat empty `Hash` as "absent," NOT as "the BLAKE3 of empty bytes." |
| Producer rounds an `mtime` from nanoseconds to seconds (`source.capnp:31`) | **OK** | Schema is `UInt64` seconds; producer-internal precision is irrelevant. |

| Situation | OK? | Reasoning |
|---|---|---|
| Producer writes `_source.path = abs_path` pre-canonicalize (be6136) | **NOT OK** | Schema docstring on `canonicalPath` says canonicalized; producer is unfaithful. |
| Two distinct producer states (different actual file contents) emit the same `BindingRecord` because `refToken` clobbers a meaningful difference | **NOT OK** | Loss of injectivity in a field consumers join on. |
| Producer emits `constructNodeId = "(none)"` instead of `""` when no construct exists | **NOT OK** | Equivalence class `"missing"` was previously `{""}`; producer expanded it without a schema change. |

### 2.5 What this means for ADR-0014

The ADR should require **per-field semantic docstrings** in every
`.capnp` file. Reading the current schemas, this is largely already
done (e.g. `binding.capnp:30-37` documents `constructNodeId` semantics
including the empty-string convention). The ADR should:

1. Mandate that every field carry a semantic docstring covering: type,
   canonicalization rule (if any), missing-value sentinel (if any), and
   producer obligations.
2. Require that producer changes that would alter a field's semantic
   class be treated as **breaking changes requiring a new field at a
   new ordinal** (Section 3 rule 3).
3. Require that consumers treat the schema docstring as authoritative
   over any locally-implemented expectation.

---

## 3. Soundness conditions for evolution rules

For each rule, I give a one-line "if-violated-then" failure mode tied
to a concrete schema field and producer/consumer code path.

### 3.1 Rule 1 — Append at next ordinal, never reuse

**If violated:** A field at ordinal `@N` in `S_old` carrying type `T_old`
is silently replaced by a different field at `@N` carrying `T_new` in
`S_new`. Consumers running `S_old` continue parsing bytes from a
producer running `S_new`, but `r.f` returns nonsense — bytes interpreted
through the wrong type. Capnp's compat machinery cannot detect this
because compat is defined per-ordinal-position.

**Concrete failure scenario:**
Suppose someone "cleans up" `BindingRecord` by removing `refToken @1`
and adding `refLemma @1 :Text` in its place (semantically the same!).
A pre-cleanup mache reads `binding.bindings.capnp` produced post-cleanup
and gets back values that *happen* to type-check (both are Text), but
the lemma extraction logic and the token extraction logic differ in
edge cases (e.g. operator overloads). Silently corrupted joins.

**Why capnp's structural compat doesn't save us:** Cap'n Proto compat
is keyed on ordinal-and-type, not field name. Reusing `@1` with a
type-stable substitute is *valid capnp* and *invalid contract.*

**Falsifiable:** see §5, claim F8.6.1.

### 3.2 Rule 2 — Never rename a field

**If violated:** Producer-side `set_target_node_id` becomes
`set_target_id`. Generated Rust code changes. Generated Go code (mache)
changes. The *wire bytes are unchanged* (capnp keys on ordinal, not
name) — but every consumer's source code stops compiling, including
the LLO crate itself.

**Why this is "softer" than Rule 1:** Rename is a build-break, not a
runtime corruption. The cost is purely human: every consumer repo
needs a coordinated edit. With many consumers (LLO, mache, future
control-room, future workerd cache), this scales badly.

**Concrete failure scenario:**
`BindingRecord.targetNodeId @0` → `BindingRecord.bindingTarget @0`.
mache PR 2 (`feat/mache-190508-binding-log-reader`) calls
`rec.TargetNodeId()`; it now fails to compile. Same for
`internal/lsp/bindings/binding.capnp.go` regeneration on the next
`capnp compile` in mache CI.

**Falsifiability gating:** Capnp toolchain itself reports a name change
as compat-OK on the wire — it's the consumer compilers that break. So
the test must be at the **build** layer: cross-repo CI that consumes a
new schema and rebuilds.

### 3.3 Rule 3 — Never repurpose a field's meaning

**If violated:** This is the subtlest rule. The schema is unchanged
(no rename, no ordinal reuse). The producer's emission semantics
change: e.g. `BindingRecord.refUri` was always a `file://` URI; the
producer is updated to sometimes emit a `lsp://` URI for synthetic
LSP-only refs.

The consumer's `Π` succeeds. The bytes are valid. But the consumer's
implicit assumption (URI starts with `file://`) is now wrong on a
subset of records. Worst case: the consumer treats `lsp://...` as a
file path, opens it, fails — or worse, accidentally reads a sibling
file with that name.

**Concrete failure scenario:**
`refUri @4 :Text` (binding.capnp:48-49) currently always carries
`file://canonicalized-abs-path`. mache PR 2's reader assumes this and
strips `file://` for joining against its own canonical-view. A
producer change to also emit `vscode-vfs://github/...` for refs
resolved against virtual filesystems silently breaks the join.

**Why this is the most dangerous rule:** No code change forces
attention. No build break. No wire format change. The drift can
persist undetected for months, exactly like be6136.

**Detection:** This is structurally identical to be6136 in the new
data plane. The mitigation is **schema-docstring as contract** plus a
falsifiability check that the producer's emitted values *belong to the
field's documented equivalence class* (§5, claim F8.6.2).

### 3.4 Rule 4 — Never reduce a field's range

**If violated:** Schema says `nodeId @0 :Text`; producer is unchanged
but a downstream validation layer is added that rejects `nodeId.len() >
1024`. A consumer that previously saw long path-shape ids
(`pkg/auth.go/function_declaration/block/.../identifier`) now sees a
truncated form or a missing record.

**When does this matter:** capnp's `Text` is unbounded in the schema
but the runtime libraries (Rust capnp, Go capnp/v3) impose a
per-message traversal limit (`ReaderOptions::traversal_limit_in_words`).
If the producer-side message exceeds this limit *for the consumer*,
parsing fails with `MessageSizeOverflow`. That's the practical
"reduced range" failure: a per-side configuration mismatch on size
budgets.

**Concrete failure scenario:**
`AstNode.nodeId` for a deeply-nested tree-sitter node could in
principle exceed the default traversal limit. LLO's producer uses
default `Builder` settings; a Go consumer using stricter
`capnp.NewDecoder` with a low `MaxSegmentSize` may reject. ADR-0014
must specify the **traversal/segment budget** as part of the
contract — or the wire format alone isn't enough to guarantee
interoperability.

**Recommendation for ADR-0014:** Document a **minimum required reader
budget** (e.g. `traversal_limit_in_words = 64M`). All consumers MUST
support this lower bound; producers MUST NOT emit messages exceeding
it without a schema-documented reason.

### 3.5 Rule 5 — Hash stability under additive change (the load-bearing question)

This is the rule the user is most worried about, and rightly so.

#### 3.5.1 The math

T8.5 computes (`cmd_parse.rs:568-582`):

```
rootHash = BLAKE3(bytes(source.capnp) || bytes(ast.capnp) || bytes(bindings.capnp))
```

where `bytes(f)` is the on-disk byte content of the segment file `f`.

The capnp wire encoding of a struct is:

```
[8-byte segment header] [data section, word-aligned] [pointer section, word-aligned] [referenced segments...]
```

When schema `S → S'` adds a field `@N : UInt64`, the struct's
`DataSize` increases by one word (8 bytes). **Every emitted instance
of that struct grows by 8 bytes**, even if the producer's call site
never invokes `set_<new_field>` (the field gets the schema default,
zero, written into the new word).

For `BindingRecord`: current struct size is `DataSize: 8, PointerCount: 6`
(`mache/internal/lsp/bindings/binding.capnp.go:18`). Adding a UInt32
field grows DataSize to 16; adding a Text field grows PointerCount to
7 (and adds the pointer/data chunk if set).

Therefore: **a pure additive schema change with NO producer-code
change** will, on the next parse run, emit byte-different segment
files, and `rootHash` will change.

#### 3.5.2 The three readings

**Reading A — bug.** The hash should be over the *semantic projection*:
some canonical encoding of the typed reading (e.g. JSON-canonical of
the typed Reader, or the wire bytes packed at *the producer's
schema-version-1*). An additive bump should leave Σ unmoved if the
semantic content is unchanged.

**Reading B — feature.** Σ records "the bytes that were emitted on
this run." If a producer running schema-version 2 emitted bytes that a
schema-version 1 consumer would have read identically except for the
new field's default value, that's still a *real* event in the
substrate's history. The hash chain documents it. A consumer
verifying with `verify-on-read` (T2.3) will succeed because the bytes
on disk match the recorded `rootHash`.

**Reading C — underspecified.** Both A and B are coherent stances;
ADR-0014 must declare which one Σ commits to.

#### 3.5.3 Recommendation

Commit to **Reading B**, but make schema versioning a first-class
substrate fact:

1. Add a **`schemaVersion`** commitment to `Head` (or a sidecar
   `manifest.capnp`). Concretely: a list of `(schemaFileName,
   schemaFileBlake3)` pairs at emission time. Schema bumps then
   advance Σ legibly: the new Head says "I was emitted under
   schemas {S₁:h₁, S₂:h₂, ...}"; consumers can verify they have a
   matching schema set, or warn.

2. Keep `rootHash = BLAKE3(segment bytes in canonical order)`. Do NOT
   try to canonicalize-by-typed-reading — the bytes ARE the
   substrate's canonical form. Adding a typed-reading canonicalization
   would require *every consumer* to agree on a canonical encoder,
   which is exactly the kind of out-of-band agreement T8 is trying to
   eliminate.

3. Document explicitly: "Σ root advance under additive schema change
   without semantic data change is a **legitimate** advance. The chain
   records 'producer ran schema-V_n on this generation.' Consumers
   detect schema mismatch via `schemaVersion` commitment, not via
   `rootHash` stability."

#### 3.5.4 Why Reading A is a trap

If we tried to make `rootHash` stable across additive schema changes,
we'd need a canonical-encoder function `canon : Π(S, b) → bytes` that:

- is identical across runtimes (Rust capnp, Go capnp/v3, future TS),
- is stable under schema additive changes (so adding a field that's
  always default leaves `canon(read) == canon_old(read_old)`),
- is collision-resistant (two distinct semantic states have distinct
  canonical bytes).

This is essentially re-implementing protobuf's "deterministic
serialization" debate, which is famously underspecified. Capnp
explicitly does NOT guarantee canonical encoding (capnproto.org's
"canonical form" exists but excludes default-valued fields, which
defeats Reading A's goal in a different way). Going down this path
recreates the "two consumers, two interpretations" failure mode T8 is
escaping. **Reading B is structurally simpler.**

### 3.6 Sixth rule (proposed): Stable `fileId` (`@0xNNN...`)

The `@0xb0c0debaadc0deb0` magic at the head of `common.capnp` is the
schema's content-id. **Never edit it.** Consumers generate type IDs
from this; `BindingRecord_TypeID = 0xb25434baab461394`
(`mache/binding.capnp.go:13`) is *derived* from the file ID and the
struct ordinal.

**If violated:** Every generated TypeID changes. Every consumer that
serialized a TypeID into its on-disk format (e.g. for cross-message
type discrimination) breaks. This is rule-1-but-at-the-file-level.

**Falsifiable:** A CI test that asserts `fileId(common.capnp) ==
0xb0c0debaadc0deb0` etc., gating any PR that edits the schemas.

---

## 4. The tooling-pin question

### 4.1 The three artifacts that need pinning

The user identified one pin (`capnpc-go @latest`); there are actually
three, listed in increasing wire-impact severity:

| Pin | What it controls | Wire impact |
|---|---|---|
| **A. `capnp` compiler** (`brew install capnp`, currently unversioned per `rs/ll-core/schema-capnp/build.rs:3-5` and README) | Schema parse + frontend; emits the codegen-AST consumed by language plugins | None directly (compiler is a frontend); but a buggy compiler could mis-parse schemas, propagating to every codegen |
| **B. Per-language code generators** (`capnpc-rust` from `capnpc = "0.20"` in Cargo.toml, `capnpc-go` from `go install capnp.org/go/capnp/v3/capnpc-go@latest`) | Translates schema-AST to per-language source | **High.** Different generator versions emit different source for the same schema. Object layouts, accessor names, defaults can drift. |
| **C. Per-language runtime libraries** (`capnp = "0.20"` Rust crate; `capnproto.org/go/capnp/v3` Go module) | Reads/writes wire bytes given the generated code | **Critical.** A runtime that handles segment framing differently from another (e.g. packed vs unpacked, single-segment vs multi-segment policies) produces wire bytes one runtime can't parse. |

### 4.2 What's currently pinned vs free

| Artifact | Current state |
|---|---|
| `capnp` compiler | **Free.** `build.rs:3-5` says "is `capnp` on PATH"; README says "Verified against Cap'n Proto 1.3.0+" but no enforcement. |
| `capnpc` (Rust generator) | **Pinned to `0.20`.** Cargo.toml line: `capnpc = "0.20"` (a `^0.20` semver-style range — patches OK, minor bumps allowed within `0.20.x`). |
| `capnp` (Rust runtime) | **Pinned to `0.20`.** Same line. |
| `capnpc-go` (Go generator) | **Free in mache.** Per Copilot review: `@latest` non-reproducible. Generated files are committed (`internal/lsp/bindings/binding.capnp.go`), so consumers see a stable artifact even with a free generator — but every regeneration risks drift. |
| `capnp v3` Go runtime | **Untracked here**, lives in mache's `go.mod`. |

### 4.3 Recommended pin policy

1. **Vendor the compiler version.** Document a minimum (`>= 1.0`) and
   a tested-against (`1.3.0`). Provide a `tools/install-capnp.sh`
   that pulls a specific release. CI uses that script.

2. **Pin generators by exact version.** `capnpc = "=0.20.0"` (Rust),
   `go install capnp.org/go/capnp/v3/capnpc-go@v3.0.X` (Go). Also pin
   any future `capnpc-ts`, `capnpc-swift`, etc. Document upgrade
   policy: a generator bump requires regen + diff review + ADR-0014
   amendment if any wire-byte change is observed.

3. **Pin runtime libraries by version range with major-version
   ceiling.** `capnp = "0.20"` is OK (patches allowed); `capnp = "*"`
   is not. Same for the Go module (`require capnproto.org/go/capnp/v3
   v3.0.X`). Rationale: minor versions of capnp libraries occasionally
   tighten validation (e.g. stricter segment-size checks); the
   producer should not emit messages a strict consumer rejects.

4. **Commit generated bindings.** mache already does this for Go.
   LLO uses `OUT_DIR` (regenerated each build). For Σ-grade
   reproducibility, consider committing the generated Rust files too,
   gated by a `regen` Cargo feature (so the build doesn't re-run
   `capnpc` on every developer machine, only when explicitly asked).
   This eliminates the "developer A and developer B have different
   capnp binaries on PATH" failure mode.

### 4.4 Falsifiability — wire-incompatibility detection

The test that catches a tool-version mismatch before it reaches
production:

> **Cross-runtime fixture round-trip (F8.6.4).** Maintain a
> hand-curated set of `BindingRecord`, `AstNode`, `SourceFile`, `Head`
> fixtures as committed `.capnp` byte-files. CI:
> 1. Reads each fixture with the Rust runtime → verifies typed-field
>    reading matches a JSON sidecar of expected values.
> 2. Reads each fixture with the Go runtime (in mache's CI) → same.
> 3. Re-emits the typed reading on each side; verifies the bytes
>    match the original fixture (where capnp guarantees byte-stability
>    of round-trip; for cases where it doesn't, normalize by
>    re-decoding-then-comparing-typed-reading).
>
> A capnp version drift that changes wire bytes shows up as a
> mismatch in step 3 on at least one side. A generator drift that
> changes accessor names shows up as a build break in step 1 or 2.

This test belongs in **both** repos — LLO emits the Rust side,
mache (and any future consumer) implements its own. The fixture
files are the shared substrate.

---

## 5. Falsifiable claims

Five conformance claims, each suitable for a CI test in T8.6
operationalization. Each follows the form **"If RULE is violated AND
condition X holds, observable failure Y will occur."**

### F8.6.1 — Additive-only ordinals

**Claim:** If a schema edit reuses or removes an existing ordinal AND
a producer is built against the new schema while any consumer is built
against the old schema, observable failure: **the consumer's typed
reading of a known field returns nonsense values that do not match the
producer's intent**, detectable by a fixture-based round-trip mismatch.

**Test shape:**

```
1. Load `schemas/binding.capnp` baseline (committed at HEAD~1 or a
   pinned baseline).
2. Diff against current HEAD `schemas/binding.capnp`.
3. Assert: ordinals(HEAD) ⊇ ordinals(baseline).
4. Assert: for every ordinal n in baseline, type(HEAD, n) == type(baseline, n).
5. Assert: for every ordinal n in baseline, name(HEAD, n) == name(baseline, n)
   (also catches Rule 2 — rename).
```

**Where it goes:** As a `cargo test` in `leyline-schema-capnp`, or a
GitHub Actions step that runs `capnp compile` and a small Rust
verifier on the generated reflection.

### F8.6.2 — Projection faithfulness on canonical paths (be6136 regression)

**Claim:** If `proj_p` writes `SourceFile.canonicalPath` without
canonicalizing AND a consumer joins on the path field, observable
failure: **`canonicalPath` does not equal `realpath(canonicalPath)` on
the producer host's filesystem**.

**Test shape (LLO side):**

```
1. Run `leyline parse <fixture-with-symlink>` where fixture is mounted
   under a symlink (e.g. /tmp/foo where /tmp is a symlink to /private/tmp
   on macOS, or a generated symlink on Linux).
2. Read every `SourceFile` record from `<db>.source.capnp`.
3. Assert: for every record r, `r.canonicalPath == realpath(r.canonicalPath)`
   on the running host. On hosts where realpath is a no-op (path is
   already canonical), the assertion is trivially true.
4. Additionally: assert no `r.canonicalPath` starts with the fixture's
   un-canonical form (the symlink prefix).
```

**Test shape (mache side):** mache PR 2's reader iterates 20 records;
add an assertion that every `r.canonicalPath` returns identical bytes
when re-canonicalized. This is the structural mirror of be6136.

### F8.6.3 — Hash chain monotonicity under additive schema change

**Claim:** If a schema edit appends a new ordinal AND the producer is
rebuilt with the new schema AND the parse is re-run on identical
source AND the previous Head was emitted under the old schema, then
observable behavior: **the new `rootHash` differs from the old
`rootHash`, the new Head's `parentHash` equals the old `rootHash`, and
the chain's `generation` advances by exactly 1**.

**Test shape:**

```
1. Parse a fixture with the current schema. Record (rootHash_1, gen_1).
2. Append a `dummyField @N` to one of {AstNode, SourceFile,
   BindingRecord}.
3. Rebuild leyline-schema-capnp; rebuild cli.
4. Parse the same fixture again. Record (rootHash_2, gen_2).
5. Read both Head records.
6. Assert: gen_2 == gen_1 + 1.
7. Assert: parentHash(Head_2) == rootHash_1.
8. Assert: rootHash_2 != rootHash_1.
9. Document this in test output as: "additive schema change advances
   Σ — this is by design."
```

**Why this is the load-bearing test:** it pins Reading B from §3.5 as
the substrate's commitment. If the team later wants Reading A
(stability across additive changes), this test fails and forces the
ADR amendment.

### F8.6.4 — Cross-runtime fixture round-trip

**Claim:** If the Rust producer's emitted bytes deviate from the Go
consumer's parsing semantics AND the `capnp` compiler / generators /
runtimes are version-drifted, then observable failure: **the cross-
runtime fixture suite shows at least one disagreement between
typed-reading-then-re-emit-and-byte-compare across the two runtimes**.

**Test shape:**

```
Fixture pack (committed in `rs/ll-core/schema-capnp/tests/fixtures/`):
  - binding-minimal.capnp.bin     (one record, all fields at default)
  - binding-rich.capnp.bin        (one record, every field set, including
                                   nested Range and Hash)
  - ast-deep.capnp.bin            (deeply nested nodeId path-shape)
  - source-with-hash.capnp.bin    (contentHash populated)
  - head-chain-2.capnp.bin        (generation=2, nonzero parentHash)
Plus a sidecar `<name>.expected.json` with the typed-reading expected
field values.

CI step (Rust):
  for each fixture: read → assert fields match expected.json →
  re-emit → assert bytes equal fixture.

CI step (Go, mache):
  same fixtures, same expected.json, same assertion.
```

**Falsifies:** capnp version drift, generator-output mismatch
(layout/accessor change), runtime-library segment-framing differences.

### F8.6.5 — Segment-order canonicality

**Claim:** If `SEGMENT_FILE_SUFFIXES` (`cmd_parse.rs:557-561`) is
reordered AND the parse is re-run on identical source, then observable
behavior: **`rootHash` changes** even though no semantic content
changed.

**Test shape:**

```
1. Parse fixture; record rootHash_1.
2. Locally permute SEGMENT_FILE_SUFFIXES (e.g. swap "ast.capnp" and
   "source.capnp").
3. Recompile, parse same fixture; record rootHash_2.
4. Assert rootHash_1 != rootHash_2. (Confirms order matters.)
5. Then assert: the SEGMENT_FILE_SUFFIXES constant's compile-time
   value matches the schema-version-pinned-canonical-order documented
   in `head.capnp:21-24`. (This is a const-equality assertion, not a
   runtime test.)
```

**Why this is needed:** the suffix order is an *implicit* part of the
substrate contract. Changing it breaks Σ continuity for every existing
file-backed db. The test pins the canonical order as a substrate
invariant, separate from schema evolution.

### Optional sixth — F8.6.6 fileId stability

**Claim:** If a schema's `@0x...` fileId changes, every generated
TypeID changes; if a consumer has serialized a TypeID into its on-disk
state, that consumer fails to recognize records produced under the new
fileId.

**Test shape:** A simple grep/parse step in CI: for each `.capnp` in
`schemas/`, assert the fileId on line 1 matches a committed allowlist:

```
common.capnp  →  @0xb0c0debaadc0deb0
binding.capnp →  @0x9c0c8cd3c5b1329a
ast.capnp     →  @0x9e1e4e1af2b578d9
source.capnp  →  @0x9bd2953355bd438c
head.capnp    →  @0xc7c7ada1403b9f78
```

(Values copied verbatim from `schemas/*.capnp:1` of each file.)

---

## 6. Open questions / edge cases

### ❓ Q1 — Reading A vs Reading B for hash stability (Section 3.5)

The recommendation is Reading B (additive schema change advances Σ;
add `schemaVersion` to `Head`). This needs your confirmation. If
Reading A is preferred (Σ stable under additive changes), the
substrate needs a canonical typed-reading encoder, which is a
substantial design lift and re-introduces the cross-runtime agreement
problem T8 was trying to escape.

### ❓ Q2 — Where does `schemaVersion` live?

Options:
- **(a) Inline in Head.** Add fields `schemaCommonHash @4 :Hash`,
  `schemaBindingHash @5 :Hash`, etc. Concrete, but baked into the
  Head schema (so adding a new schema requires a Head ordinal bump).
- **(b) Sidecar manifest file.** `<db>.manifest.capnp` carrying a
  list of `(filename, hash)` pairs, hashed alongside other segments.
  More flexible, but adds another file to the on-disk layout.
- **(c) `schemaVersion @4 :UInt64` opaque counter.** Smallest change;
  human-readable; doesn't bind specifically to schema bytes.
  Loses the ability to verify "consumer has the exact schema I emitted
  with."

I lean (b) — keeps the head's wire format stable, makes the schema
commitment a first-class segment.

### ❓ Q3 — Should `Head` itself be in the segment-hash chain?

Currently `head.capnp` is NOT in `SEGMENT_FILE_SUFFIXES`
(`cmd_parse.rs:557-561`); it's the *output* of the hash, not an input.
Question: should the previous Head be in the next run's hash input,
making the chain Merkle-explicit (each Head's `rootHash` =
`BLAKE3(prev_head_bytes || new_segments)`)? Currently the chain is
stored in `parentHash` field but the bytes of the previous Head aren't
hashed. This is fine for the current single-host file-backed case but
diverges from the daemon `Controller::current_root` semantics if/when
they merge.

### ❓ Q4 — `BindingRecord.parseGen @6` is currently always 0

The producer (`project.rs:585`) sets `parseGen = 0` with comment
"T8.5 wires it to Σ generation." This is a schema-documented but
producer-incomplete field. ADR-0014 should either:
- (a) Mark this as "T8.7 work" and document the placeholder, OR
- (b) Re-classify it (per §2.4) as "absent value sentinel = 0,
  consumers MUST treat 0 as 'unknown'", OR
- (c) Wire it now in a follow-on bead before T8.6 closes.

### ❓ Q5 — What's the deprecation pathway for fields?

Capnp permits leaving a field as a "hole" but doesn't have a
first-class `[deprecated]` annotation. The current schemas use
docstring conventions (e.g. "MUST be exactly 32 bytes" in
`common.capnp:29`). Should ADR-0014 mandate a `# DEPRECATED:`
docstring prefix as the canonical deprecation marker, with a CI step
that lists all deprecated fields in a manifest?

### ❓ Q6 — What about `Common.Hash` vs raw `Data`?

`common.capnp:28-30` declares `Hash` as a struct wrapping `Data` with
a comment "MUST be exactly 32 bytes." This isn't enforced by the
schema (capnp's `Data` is variable-length). The producer sets exactly
32 bytes; a buggy or adversarial producer could emit different
lengths.

Two options:
- (a) Add a runtime invariant check (`assert!(hash_bytes.len() == 32)`)
  in the consumer-side helper.
- (b) Replace `Hash` with a fixed-size `List(UInt8)` of 32 elements
  using capnp's `[group]` mechanism (less ergonomic, but enforces).
- (c) Leave as-is; document the trust boundary.

For substrate-grade strength, (a) is the cheapest viable answer.
ADR-0014 should specify which.

### ❓ Q7 — Multi-message file framing

The README (§"Event-log file conventions") says "plain back-to-back
capnp segment messages written via `capnp::serialize::write_message`
(Rust) / `capnp.NewEncoder` (Go) / equivalents." Are these
guaranteed to be byte-compatible across runtimes? Capnp has both
**unpacked** (default for `serialize`) and **packed** formats; mixing
them produces unparseable streams. ADR-0014 should pin **unpacked**
explicitly.

### ❓ Q8 — How does this interact with closed-source `ll-core/public-schema/capnp/daemon.capnp`?

`daemon.capnp` (`@0xa1b2c3d4e5f60001`) already serves a similar
producer/consumer role at the daemon UDS layer, with a different
fileId namespace. ADR-0014 should clarify whether the T8 schemas and
the daemon protocol schemas are the **same** ADR scope or distinct.
Currently `T8/capnp-as-protocol` schemas live in
`rs/ll-core/schema-capnp/`; `daemon.capnp` lives in
`rs/ll-core/public-schema/`. The directory split suggests they're
intended as separate ADRs, but the evolution rules in this doc apply
equally to both.

---

## 7. Summary recommendations for ADR-0014

In priority order:

1. **Adopt Reading B for Σ-root hash semantics** (Section 3.5). Add a
   `schemaVersion` commitment (Q2-(b) recommended: sidecar manifest).

2. **Mandate per-field semantic docstrings** with explicit
   canonicalization rules and missing-value sentinels (Section 2.5).

3. **Codify the six evolution rules** as build-time + CI checks:
   - R1: append-only ordinals (F8.6.1)
   - R2: never rename (build-break detection in cross-repo CI)
   - R3: never repurpose (F8.6.2 generalized + docstring policy)
   - R4: minimum reader budget (`traversal_limit_in_words >= 64M`)
   - R5: hash chain monotonicity under additive change (F8.6.3)
   - R6: stable fileId (F8.6.6)

4. **Pin tooling triplets** (Section 4.3): compiler, per-language
   generators, per-language runtimes. Commit generated bindings or
   regen-on-demand, not regen-on-build.

5. **Cross-runtime fixture suite** (F8.6.4) lives in
   `rs/ll-core/schema-capnp/tests/fixtures/` and is consumed by both
   LLO Rust CI and mache Go CI.

6. **Pin segment canonical order** (`SEGMENT_FILE_SUFFIXES`) as a
   substrate constant separate from schema evolution (F8.6.5).

7. **Resolve the eight open questions** (Section 6) before merging
   the ADR.

---

## Appendix A — File / line citation index

For verifiability:

| Claim | Citation |
|---|---|
| Schema files & ordinals | `rs/ll-core/schema-capnp/schemas/{common,binding,ast,source,head}.capnp` (full listings inlined in §0) |
| Producer canonicalization (be6136 fix) | `rs/ll-open/cli-lib/src/cmd_parse.rs:359-361` |
| Producer dual-write (BindingRecord) | `rs/ll-open/lsp/src/project.rs:464-474, 565-604` |
| Producer dual-write (AstNode) | `rs/ll-open/cli-lib/src/cmd_parse.rs:447-449, 733-760` |
| Producer dual-write (SourceFile) | `rs/ll-open/cli-lib/src/cmd_parse.rs:422-431, 705-731` |
| Σ-root chain computation | `rs/ll-open/cli-lib/src/cmd_parse.rs:548-655` |
| Segment canonical order | `rs/ll-open/cli-lib/src/cmd_parse.rs:557-561` |
| Substrate Hash type (BLAKE3 lock) | `rs/ll-core/core/src/substrate.rs:30-83` |
| Controller::current_root | `rs/ll-core/core/src/control.rs:140-165` |
| Schema-as-data-plane README | `rs/ll-core/schema-capnp/README.md:1-22` |
| Tooling pin (Rust capnpc) | `rs/ll-core/schema-capnp/Cargo.toml:8-10` |
| build.rs unpinned compiler | `rs/ll-core/schema-capnp/build.rs:3-5` |
| mache vendored schemas | `~/remotes/art/mache/schemas/{common,binding}.capnp` |
| mache generated Go binding | `~/remotes/art/mache/internal/lsp/bindings/binding.capnp.go:13` (TypeID derivation) |
| Decade BLAKE3 lock | `docs/decades/2026-merkle-cas-substrate.md` §3.4 |

## Appendix B — Why the data-plane reframe formally precludes be6136

**Theorem (informal).** Let `A = SourceFile.canonicalPath` and let
`p, c` be producer/consumer endpoints obeying invariant F1 (Section 2)
on `A`. Then for any pair of file-system paths `x, y` such that
`realpath(x) = realpath(y)`, both endpoints observe `A(x) = A(y)`,
and any join keyed on `A` is canonicalization-stable.

**Proof sketch.** F1 says the producer's emission of `A` is
canonical (i.e. `A = realpath(input)`). Both endpoints read the same
bytes for `A`. Therefore both observe the same canonical
representative. Consumers downstream of `c` join on `A`'s bytes;
since the bytes are identical for `realpath`-equivalent inputs, joins
do not miss. ∎

The structural property here is that the producer's canonicalization
becomes an **observable fact in the wire format** (the docstring on
`SourceFile.canonicalPath` is the contract). In the pre-T8 world, the
canonicalization happened inside `_source.path` writes, and the
consumer's join had to re-derive whether it was applied. The data-
plane reframe shifts canonicalization from a *latent producer-side
discipline* to a *manifest contract on the wire*. be6136-class bugs
become caught at the schema-docstring level (F1 violation = audit), or
at CI level (F8.6.2), instead of at undetectable runtime.

---

*End of analysis.*
