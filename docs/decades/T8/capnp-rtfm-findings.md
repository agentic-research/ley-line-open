# Cap'n Proto RTFM — research findings for ADR-0014

**Status:** Research dossier (NOT the ADR). Background for
`docs/adr/0014-capnp-as-protocol.md`.
**Decade:** `ley-line-open-9d30ac` (Σ Merkle-CAS substrate)
**Thread:** `T8/capnp-as-protocol`
**Bead:** `ce8fd1` (T8.6)
**Date:** 2026-05-08
**Author:** Claude (research dispatch)
**Sibling doc:** `docs/decades/T8/adr-0014-design-analysis.md`

This file is verdict + sources + implication-for-T8.6, section by
section. The synthesis at the end is opinionated and load-bearing.

---

## 1. Cap'n Proto canonical encoding

### Verdict

**Cap'n Proto has a published canonical form, and it IS byte-stable
across additive schema changes for messages that don't set the new
fields.** The encoding spec's truncation rule was deliberately
engineered for exactly this property — adding a field that an instance
doesn't set leaves the canonical bytes unchanged. The Go runtime's
public doc string says this in plain English; the Rust runtime
implements it via `Builder::set_root_canonical`. This invalidates the
math-friend's premise in ADR-0014 §3.5.4 ("capnp explicitly does NOT
guarantee canonical encoding") — that claim is wrong as written.
However, the canonical form has *non-trivial preconditions*: messages
must be canonicalized at write time (or canonicalize-on-read before
hashing), all runtimes must produce byte-identical canonical output
(largely true post-0.5; see §1's note on the 0.5 wire-format change),
and the segment-table prefix must be stripped before hashing. The
substrate is currently NOT producing canonical messages — it's
hashing whatever `serialize::write_message` emits, which is the
unpacked default form, not canonical form.

### Sources (verbatim)

**(1.a) The encoding spec's canonicalization section.** From
`https://github.com/capnproto/capnproto/blob/master/doc/encoding.md`,
lines 358–390 (also published at
`https://capnproto.org/encoding.html#canonicalization`):

> ## Canonicalization
>
> Cap'n Proto messages have a well-defined canonical form. Cap'n Proto
> encoders are NOT required to output messages in canonical form, and
> in fact they will almost never do so by default. However, it is
> possible to write code which canonicalizes a Cap'n Proto message
> without knowing its schema.
>
> A canonical Cap'n Proto message must adhere to the following rules:
>
> * The object tree must be encoded in preorder (with respect to the
>   order of the pointers within each object).
> * The message must be encoded as a single segment. (When signing or
>   hashing a canonical Cap'n Proto message, the segment table shall
>   not be included, because it would be redundant.)
> * **Trailing zero-valued words in a struct's data or pointer
>   segments must be truncated. Since zero represents a default value,
>   this does not change the struct's meaning. This rule is important
>   to ensure that adding a new field to a struct does not affect the
>   canonical encoding of messages that do not set that field.**
> * Similarly, for a struct list, if a trailing word in a section of
>   all structs in the list is zero, then it must be truncated from
>   all structs in the list. (All structs in a struct list must have
>   equal sizes, hence a trailing zero can only be removed if it is
>   zero in all elements.)
> * Any struct pointer pointing to a zero-sized struct should have an
>   offset of -1.
> * Canonical messages are not packed. However, packing can still be
>   applied for transmission purposes; the message must simply be
>   unpacked before checking signatures.
>
> Note that Cap'n Proto 0.5 introduced the rule that struct lists must
> always be encoded using C = 7 in the list pointer. … Therefore, the
> rules have been changed in 0.5, but data written by previous
> versions may not be possible to canonicalize.

The third bullet is the load-bearing one. Bold added; the text
verbatim-states the design intent that additive schema changes do not
move the canonical bytes.

**(1.b) The Go runtime's public commitment.** From
`https://github.com/capnproto/go-capnp/blob/main/canonical.go` (also
mirrored at `go-capnproto2/canonical.go`):

```go
// Canonicalize encodes a struct into its canonical form: a single-
// segment blob without a segment table.  The result will be identical
// for equivalent structs, even as the schema evolves.  The blob is
// suitable for hashing or signing.
func Canonicalize(s Struct) ([]byte, error) {
```

Plain English, in the runtime's authoritative source: *"identical for
equivalent structs, even as the schema evolves."* Notably, the
function *operates on a typed Struct without taking a schema* — the
truncation is done by scanning data backwards for the last non-zero
word and pointers backwards for the last non-null pointer
(`canonicalStructSize`), which is purely structural.

**(1.c) The Rust runtime's API.** From
`https://github.com/capnproto/capnproto-rust/blob/master/capnp/src/message.rs`
lines 327–344 and 553–568:

```rust
/// Gets the canonical form of this message. Works by copying the message
/// twice. For a canonicalization method that only requires one copy, see
/// `message::Builder::set_root_canonical()`.
pub fn canonicalize(&self) -> Result<alloc::vec::Vec<crate::Word>> { … }

/// Sets the root to a canonicalized version of `value`. If this was the
/// first action taken on this `Builder`, then a subsequent call to
/// `get_segments_for_output()` should return a single segment,
/// containing the full canonicalized message.
pub fn set_root_canonical<T: Owned>(&mut self, value: impl SetterInput<T>) -> Result<()> { … }
```

Both APIs exist and are public. There is no canonicalize CLI in
`capnp-tool` per `https://capnproto.org/capnp-tool.html`, but
`capnp convert` can convert between encodings.

**(1.d) Issue #2171 confirmation.** Issue
`https://github.com/capnproto/capnproto/issues/2171` ("Canonical/
Deterministic serialization") was closed by Kenton Varda (capnp's
author) with a single comment: *"Yes, this is covered here:
https://capnproto.org/encoding.html#canonicalization"*. Authoritative
ack that canonical form is the answer.

**(1.e) The Google Group thread caveat.** Kenton's response in
`https://groups.google.com/g/capnproto/c/3MIU8xZBX1Q` reinforces one
constraint that interacts with §1.a: *"You cannot change a field or
method parameter's type or default value."* I.e., the truncation
guarantee assumes default-value stability — once a field's default is
set, it must never change, or older readers will mis-interpret the
absence-as-default. This is a no-op for our schemas (we use type
defaults: 0 for ints, "" for Text, null for pointers). We never
declare custom defaults. Good.

**(1.f) The 0.5 wire-format caveat.** From the encoding spec's
trailing paragraph (verbatim above): pre-0.5 capnp messages may not
canonicalize. We are on capnp 1.3.0+ everywhere
(`rs/ll-core/schema-capnp/README.md:107`,
`rs/ll-core/schema-capnp/Cargo.toml:8` `capnpc = "0.20"`), so this is
historical, not active risk.

### Implication for T8.6

**This is the most consequential finding in the dossier and it
flips the math-friend's recommendation.** ADR-0014 §3.5.4 says
"Reading A is a trap" because "capnp explicitly does NOT guarantee
canonical encoding." That premise is incorrect: canonical encoding is
specified, implemented in C++/Rust/Go, and the Go runtime's public
contract literally promises *byte-stability across schema evolution*
for equivalent structs.

Therefore Reading A (Σ root stable under additive schema changes) is
**not** a trap — it is achievable by:

1. Producer canonicalizes each record before writing
   (`Builder::set_root_canonical` in Rust; same exists in Go).
2. Σ root is computed over the canonical bytes (with segment-table
   stripped per §1.a bullet 2: *"the segment table shall not be
   included"*).
3. Adding a field at `@N` with default-zero value does not change the
   canonical bytes for instances that don't set it.

This does NOT eliminate the need for `schemaVersion` — see §3 — but it
*does* mean the substrate has a real choice between Reading A and
Reading B, with engineering work either way. It's not a forced move.

The ADR should make Reading A vs. B an *explicit* commitment with
named tradeoffs, not paper over it. The strongest argument FOR
Reading A: cross-runtime fixture round-trip (F8.6.4) becomes trivial
to enforce via canonical bytes. The strongest argument FOR Reading B:
canonical form requires extra engineering at the producer call sites
(every `BindingRecord`/`AstNode`/`SourceFile` write must use
`set_root_canonical`, not the default `serialize::write_message`),
and we currently use the latter. Migrating is a focused but
non-trivial change — see §6 for the SegmentLog interface alternative.

---

## 2. Real-world long-lived capnp users — schema evolution practices

### Verdict

The two big public capnp deployments — Cloudflare workerd and
Sandstorm — both use **append-only ordinals + inline OBSOLETE comments
+ never-rename** as their evolution discipline. Workerd adds an
extra mechanism that's worth lifting: **annotation-driven
compatibility flags** (`$compatEnableFlag`, `$compatEnableDate`,
`$compatDisableFlag`) define a temporal "compatibility date" under
which a feature is on/off. This is workerd's published answer to
"how do you evolve a wire schema over five years without breaking
deployed clients" — and it has no analog in vanilla capnp. Sandstorm
relies on the structural discipline alone and on its `@obsolete`
boolean annotation for permission/role lists. Neither codebase
publishes a "schema-version field" embedded in messages. Neither uses
canonical form for content addressing.

### Sources

**(2.a) Workerd's compatibility-date system.** From
`https://github.com/cloudflare/workerd/blob/main/src/workerd/io/compatibility-date.capnp`
(file id `@0x8b3d4aaa36221ec8`):

```capnp
struct CompatibilityFlags @0x8f8c1b68151b6cef {
  # Flags that change the basic behavior of the runtime API, especially for
  # backwards-compatibility with old bugs.

  annotation compatEnableFlag @0xb6dabbc87cd1b03e (field) :Text;
  annotation compatDisableFlag @0xd145cf1adc42577c (field) :Text;
  # An enable-flag is used to enable the feature before it becomes the default…
  # A disable-flag is used when a worker needs to keep long-term backwards
  # compatibility with one bug but doesn't want to hold back everything else.

  annotation compatEnableDate @0x91a5d5d7244cf6d0 (field) :Text;
  # The compatibility date (date string, like "2021-05-17") after which this
  # flag should always be enabled.

  formDataParserSupportsFiles @0 :Bool
      $compatEnableFlag("formdata_parser_supports_files")
      $compatEnableDate("2021-11-03")
      $compatDisableFlag("formdata_parser_converts_files_to_strings");
  # Our original implementations of FormData made the mistake of turning
  # files into strings.
}
```

The schema currently runs from `@0` to `@114` (114 ordinals appended
over five years), with obsolete fields preserved in place (per
`https://deepwiki.com/cloudflare/workerd/4-compatibility-system`).
**Critical pattern:** versioning lives in *annotations on individual
fields*, not as a separate `schemaVersion` integer. The wire schema
itself is the version manifest.

**(2.b) Workerd's stability disclaimers via inline comment.** From
`https://github.com/cloudflare/workerd/blob/main/src/workerd/server/workerd.capnp`
(file id `@0xe6afd26682091c01`): no file-level versioning policy, but
fields like `localDisk` carry inline:

> `# EXPERIMENTAL; SUBJECT TO BACKWARDS-INCOMPATIBLE CHANGE`

And deprecated fields use:

> `# DEPRECATED: Please switch to ES modules syntax instead`

Plus union-member naming: `obsolete @7 :Text;`. **No first-class
`@deprecated` annotation in the language; only docstring convention.**

**(2.c) Sandstorm's grain.capnp.** From
`https://github.com/sandstorm-io/sandstorm/blob/master/src/sandstorm/grain.capnp`
(file id `@0xc8d91463cfc4fb4a`):

```capnp
interface AppPersistent @0xaffa789add8747b8 (AppObjectId) {
  save @0 () -> (objectId :AppObjectId, label :Util.LocalizedText);
}
```

Comment in the schema: *"It is important that new versions of the app
only add new permissions, never remove existing ones, since
permission IDs are indexes into the list."* — the convention is
documented inline, not enforced by the schema language. Apps that
receive an unknown SturdyRef are advised to *"return a dummy callback
that does nothing."*

**(2.d) Cap'n Proto's own self-describing schema** (`schema.capnp`).
The compiler's reflection schema lives at
`https://github.com/capnproto/capnproto/blob/master/c%2B%2B/src/capnp/schema.capnp`
and follows the same append-only discipline: file ID
`@0xa93fc509624c72d9` is fixed since 2014; new node types
(`Brand`, `Type::AnyPointer`) were added at higher ordinals. No
versioning field on the schema itself; the schema's version is
implicit in the capnp toolchain version.

**(2.e) Workerd's authoritative summary.** Per DeepWiki's article
`https://deepwiki.com/cloudflare/workerd/4-compatibility-system`:

> Rather than traditional version numbers, workerd maintains backward
> compatibility by using compatibility dates as its version number —
> the date corresponding to the maximum "compatibility date"
> supported by that version, allowing workers to be configured to a
> past date so workerd will emulate the API as it existed on that date.

### Implication for T8.6

The published precedent says: **don't put `schemaVersion :UInt64` in
your top-level struct.** Workerd considered this and chose not to;
their version axis is per-field annotations + a date. Sandstorm chose
not to; their version axis is interface-level capability evolution.
Neither bakes a monotone counter into their wire format.

Concretely for ADR-0014, this changes the answer to ADR-0014 Q2
("where does `schemaVersion` live?"):

- **Option (a) inline in Head** (`schemaCommonHash @4`, etc.) — has
  no precedent in workerd or sandstorm. Custom design.
- **Option (b) sidecar manifest** — has weak precedent (workerd's
  `compatibility-date.capnp` *is* a sidecar, but it's a per-flag
  manifest, not a per-emission manifest). Workable.
- **Option (c) opaque counter** — has no precedent and provides
  weakest verifiability.
- **Option (d, new — workerd-style)**: an annotation-driven
  schema-version axis. Each schema field that's added carries
  `$introducedInSubstrateGen(N)` or similar. Wire format never
  changes; consumers consult the annotation library to know which
  fields are "new since gen N." This is the most idiomatic capnp
  answer.

I'd argue (d) is what the precedent demands, but it's significantly
more engineering than (b). **My recommendation for ADR-0014: pick (b)
because it's tractable now, and document (d) as a future migration
path.**

---

## 3. The `schemaVersion` convention in the ecosystem

### Verdict

Cap'n Proto has **no first-class schema-version-on-the-wire convention**
beyond:

1. The 64-bit fileId (`@0x...`), which is per-file and stable for the
   life of that file. It is NOT a version number — it's an identity
   for the schema *file*. Renaming the file or moving types preserves
   it; changing the schema's *content* does not advance it.
2. The 64-bit type ID (`@0x...`) on each struct/interface/annotation,
   derived by default from MD5(parent_id || name). Same identity
   semantics — not a version.
3. Annotations as a versioning mechanism (workerd's pattern, §2.a).

There is **no idiomatic capnp pattern** for "embed a schema version
counter in the message." The closest patterns are: workerd's
compatibility-date annotations on fields, and Sandstorm's
*interface-level* capability evolution (a new schema is a new
interface with a new typeID, called via runtime negotiation).

### Sources

**(3.a) The schema language doc on file IDs and type IDs.** From
`https://capnproto.org/language.html`:

> A Cap'n Proto file must have a unique 64-bit ID, and each type and
> annotation defined therein may also have an ID. … This default ID
> is derived by taking the first 8 bytes of the MD5 hash of the
> parent scope's ID concatenated with the declaration's name. … In
> general, you would only specify an explicit ID for a declaration
> if that declaration has been renamed or moved and you want the ID
> to stay the same for backwards-compatibility.

So `@0xb0c0debaadc0deb0` (our `common.capnp`) is *file identity*,
not file version. If `common.capnp` is renamed or moved, the
explicit ID keeps it the same. If `common.capnp`'s contents evolve
additively, the ID does *not* change — that's the design intent.

**(3.b) The schema language doc on ordinals.** Same source:

> The @N annotations show how the protocol evolved over time, so that
> the system can make sure to maintain compatibility with older
> versions. Fields (and enumerants, and interface methods) must be
> numbered consecutively starting from zero in the order in which
> they were added.

Ordinals carry evolution-order metadata. They are *not* a version
number — a consumer reading `@5` doesn't know it's looking at "v6 of
the schema"; it knows the field was added 6th. To map this to a
version, you'd need an external manifest.

**(3.c) Workerd's annotation pattern.** Already quoted in §2.a. This
is the only published "schema-versioning" idiom I found. Note that
workerd's pattern *requires* the consumer to know which annotations
exist, which means the consumer needs a *recent enough* schema to
know what `compatEnableDate` means in the first place. This is a
bootstrap-the-bootstrap problem they tolerate because all consumers
are themselves workerd binaries built from the same source tree.

For our case (Rust LLO + Go mache + future TS workerd cache + Swift
control-room), all consumers must agree on the annotation library
*before* annotation-driven versioning can work. The schema files
themselves are the agreement, but each runtime must regenerate
bindings against the same revision.

**(3.d) Issue #2171 again.** Kenton points only at canonicalization,
not at any version-on-the-wire convention. That absence is itself
load-bearing.

### Implication for T8.6

**Drop the `schemaVersion :UInt64` in `Head` proposal.** It has no
ecosystem precedent and fights against capnp's mental model (the
schema *is* the version; the fileId is the file's identity).

Instead:

- **Manifest sidecar (ADR-0014 Q2 option b)** is the right fit —
  each Σ generation lists `(filename, BLAKE3-of-canonical-schema-bytes)`
  pairs. This makes the schema a first-class substrate object,
  hashable as data. Concretely: `<db>.manifest.capnp` next to the
  segment files, hashed *into* the rootHash chain. Consumers
  verifying a Σ root can independently confirm "I have the same
  schemas the producer used."

- This *also* gives us a clean answer to Q3 ("should Head be in the
  hash chain"). It shouldn't (Head is the output), but the manifest
  *should* be — because the manifest is producer-side metadata
  asserting "I emitted under these schemas," and a verifier needs
  that assertion bound to the rootHash.

- **For long-term ADR-0014 evolution: emulate workerd's annotation
  pattern.** Document in §"Schema Evolution" that future per-field
  annotations like `$introducedInGen(N)` are the preferred
  versioning axis once we have a multi-runtime annotation library.
  Until then, the manifest is the answer.

---

## 4. Tooling-pin precedent

### Verdict

The community precedent for pinning capnp toolchain is **uneven and
loose**. The Rust ecosystem pins generator + runtime as one crate
pair (`capnp` + `capnpc`, both 0.x). The C++ ecosystem (workerd) uses
Bazel modules and pins by commit. The Go ecosystem `@latest` is
common (mache hit this). The capnp project itself does NOT publish a
formal cross-language compatibility table. The strongest published
guidance: *capnpc the Rust crate explicitly states it requires the
`capnp` C++ binary on PATH but does not specify a version range.*
Cross-runtime byte-compatibility is presumed-tested via the C++
test suite, not explicitly contracted.

### Sources

**(4.a) capnpc's own README.** From
`https://docs.rs/capnpc/latest/capnpc/`:

> This library allows you to do Cap'n Proto code generation within a
> Cargo build. You still need the `capnp` binary (implemented in
> C++). (If you use a package manager, try looking for a package
> called `capnproto`.)

No version guidance. No compatibility matrix.

**(4.b) capnpc-rust source.** From
`https://github.com/capnproto/capnpc-rust` (now folded into
capnproto-rust): no version-pinning of the C++ binary, no minimum
version stated in the crate metadata.

**(4.c) Workerd's Bazel pin.** Workerd uses Bazel module system with
`MODULE.bazel`/`WORKSPACE.bzlmod` referencing a specific capnp-cpp
build (per
`https://github.com/cloudflare/workerd/blob/main/BUILD.bazel`). They
pin by Bazel module hash, which transitively pins the capnp version.
This is a strong-pin pattern but Bazel-specific.

**(4.d) Capnp cross-runtime testing.** From
`https://github.com/capnproto/go-capnp/blob/main/canonical_test.go`:
the Go runtime ships canonical-form tests, but the test fixtures are
Go-internal — there's no published cross-runtime fixture suite.
*"Encode in Rust, decode in Go, assert equal"* is not a published
test pattern in either repo. We'd be ahead of the ecosystem to ship
one (which is what F8.6.4 in the math-friend's analysis proposes).

**(4.e) Issue #182 on capnp-rust.** Per
`https://github.com/capnproto/capnproto-rust/issues/182`
("Distribution of the capnp tool for Rust projects"): there is an
open community concern about not pinning the capnp binary. No
resolution. The community workaround is to install via OS package
manager (`brew install capnp` on macOS) and trust the version is
recent enough.

### Implication for T8.6

The math-friend's pin recommendation (§4.3) is the right thing to do
*and* it's slightly ahead of community practice. Specifically:

- **(4.3.1) Vendor the compiler version** — yes, per workerd's Bazel
  pattern adapted to a `tools/install-capnp.sh`. Document required
  version `>= 1.0`, tested against `1.3.0`. We are doing more pinning
  than the community requires, which is a feature for a substrate.

- **(4.3.2) Pin generators by exact version** — yes. `capnpc =
  "=0.20.0"` (Rust), Go: `go install
  capnp.org/go/capnp/v3/capnpc-go@v3.0.X` with an exact tag, not
  `@latest`. The mache PR-1 Copilot review caught this; the fix is
  to commit a `tools/versions.toml` or similar.

- **(4.3.3) Pin runtimes** — yes. `capnp = "=0.20.0"` (Rust). For
  Go, set `require capnproto.org/go/capnp/v3 v3.0.X` exactly, not a
  semver range.

- **(4.3.4) Cross-runtime fixture suite** — F8.6.4 is the right test
  and *the ecosystem doesn't have one*. We ship it; mache's CI and
  LLO's CI both verify against shared `tests/fixtures/*.bin` files
  with sidecar `*.expected.json`. This is novel work, not a port.

- **Commit generated bindings.** mache already does. LLO should too,
  per ADR-0014. Reasoning is solid: developer A's `capnp` binary
  may differ from developer B's; committing the artifact eliminates
  this. The Rust generator can be gated behind a `regen` Cargo
  feature (so devs don't accidentally re-run it on every build).

---

## 5. The Persistent / SturdyRef story

### Verdict

**SturdyRef is an RPC-layer concept (Level 4 RPC), and it does not
generalize to "any persistent reference into a capnp event log."**
The schema is parameterized over `(SturdyRef, Owner)` types that are
*realm-specific* — each deployment chooses its own format, so there's
no canonical SturdyRef-bytes shape. Sandstorm's app-layer
`AppPersistent` is the closest precedent for "a stable handle into a
log," but it's still RPC-restricted: a SturdyRef is opaque to the
host, and only the implementing capability knows how to interpret it.
**SturdyRef is NOT useful for our `head.capnp` `parentHash` chain.**
Our parentHash is a content hash (BLAKE3 of canonical bytes), which
is a *capability-free* substrate primitive. SturdyRefs add live-cap
restoration semantics we don't need.

### Sources

**(5.a) The persistent.capnp file.** From
`https://github.com/capnproto/capnproto/blob/master/c++/src/capnp/persistent.capnp`,
file id `@0xb8630836983feed7`:

```capnp
interface Persistent@0xc8cb212fcd9f5691(SturdyRef, Owner) {
  # Interface implemented by capabilities that outlive a single
  # connection. A client may save() the capability, producing a
  # SturdyRef. The SturdyRef can be stored to disk, then later used to
  # obtain a new reference to the capability on a future connection.
  #
  # The exact format of SturdyRef depends on the "realm" in which the
  # SturdyRef appears. A "realm" is an abstract space in which all
  # SturdyRefs have the same format and refer to the same set of
  # resources. … Since the format of SturdyRef is realm-dependent,
  # it is not defined here.

  save @0 SaveParams -> SaveResults;
}
```

The doc comment makes the realm-specificity explicit: *"Since the
format of SturdyRef is realm-dependent, it is not defined here. An
application should choose an appropriate realm for itself."* So
"SturdyRef" is not a standardized bytes shape — it's an
abstraction for capability persistence with deployment-specific
encoding.

**(5.b) Schema evolution across SturdyRef versions.** The
persistent.capnp file does NOT specify what happens when a SturdyRef
minted under schema v1 is restored under schema v2. The realm
implementer is responsible for that. Sandstorm's grain.capnp says:
"if you are asked to restore a callback you don't recognize, return a
dummy callback that does nothing." That's a realm-level convention,
not a capnp guarantee.

**(5.c) The sealing mechanism.** Persistent.capnp documents
`sealFor :Owner` which makes a SturdyRef restorable only by the
specified owner. This is an authentication primitive, orthogonal to
schema evolution.

### Implication for T8.6

**Don't try to model `Head.parentHash` as a SturdyRef.** The
content-hash chain we have today is the right primitive for a
substrate; SturdyRefs add live-RPC and per-realm format semantics
that aren't relevant to a file-backed event log. The math-friend's
analysis is right to keep `parentHash :Common.Hash` (32-byte BLAKE3)
rather than swap in a typed SturdyRef.

The only place SturdyRef *might* matter for T8 is if/when we add
live RPC across the substrate (e.g. mache calls back to LLO daemon
to fetch a missing segment). At that point, the call would be
modeled as `interface SegmentLog { fetch @0 (root :Hash) -> (segment
:Data); }` (see §6 next), and the cap to `SegmentLog` *could* be
made Persistent so that mache can save a long-lived handle. But
that's a future design — not relevant to ADR-0014.

---

## 6. The `interface` / `Persistent` capability vs. plain struct distinction

### Verdict

Pure-struct schemas are the **right call for T8 right now**. Adding
an `interface` introduces a Level 1+ RPC dependency: live capability
hosting, two-way communication, error propagation, capability
lifetimes. None of this is needed for "read a file off disk and
parse it." The substrate is a content-addressed log, not an RPC
graph. However, the math-friend's hypothetical `interface SegmentLog
{ fetch @0 (root :Hash) -> (segment :Data); }` is a real future
design that would make sense once the substrate goes distributed
(daemon hosting segments fetched on demand rather than all
local-disk). For now, capabilities buy us nothing; for then, they
buy us standard RPC patterns including SturdyRefs (per §5).

### Sources

**(6.a) Cap'n Proto language doc on interface vs. struct.** From
`https://capnproto.org/language.html`: structs are passive data;
interfaces require an RPC implementation. Capnp's README at
`https://capnproto.org/` is explicit that "Cap'n Proto can be used
just for serialization, just for RPC, or both." Workerd uses both;
mache's binding-log reader uses just-serialization (per
`~/remotes/art/mache/internal/lsp/bindings/binding.capnp.go`).

**(6.b) Workerd's worker-interface.capnp.** From
`https://github.com/cloudflare/workerd/blob/main/src/workerd/io/worker-interface.capnp`:
this file *does* declare interfaces (`WorkerLoaderRpc`,
`HibernatableWebSocket`) — but only because the workerd runtime has
live RPC between processes. The schema files used purely for
configuration (`workerd.capnp`, `compatibility-date.capnp`) are
struct-only.

**(6.c) Sandstorm's split.** grain.capnp has both — `MainView` is an
interface (live RPC into a grain), but most data types like
`UserPermissions`, `PowerboxAction` are structs.

### Implication for T8.6

**No change to current schemas needed.** Keep struct-only. ADR-0014
should commit to struct-only as a substrate property: *"Σ schemas
declare data, not capabilities. Live behavior (daemon RPC, distributed
segment fetch) lives in a separate schema set under a separate file
ID, addressed by future ADRs."* This is a clean scope boundary.

The directory split already implied this — `rs/ll-core/schema-capnp/`
holds substrate schemas (data); `rs/ll-core/public-schema/` holds
`daemon.capnp` (already mixing data and live UDS protocol). ADR-0014
should explicitly enumerate which of the two scopes it covers (the
substrate-data scope only, per ADR-0014 Q8) and defer the daemon
protocol's evolution rules to a sibling ADR.

---

## 7. Hash stability — content-addressed systems with structured serialization

### Verdict

Content-addressed systems split into two camps:

1. **Canonical-encoding camps** (IPLD/DAG-CBOR, ATproto/DRISL): the
   serialization format has a *deterministic* canonical form (sorted
   map keys, shortest integer encoding, no indefinite-length items).
   Adding an optional field to a record's logical schema **does**
   change the CID because the canonical bytes include the new field
   if it's set, and exclude it if it's not. **No version bookkeeping
   is required at the wire level — the CID itself is the version.**
2. **Best-effort-determinism camps** (protobuf): explicitly *not*
   canonical, even within a single binary version. Hashes of
   serialized protos are documented as fragile. Users are told
   to define their own canonical encoder if they need stable hashes.

Capnp sits between these: it has a documented canonical form (camp 1)
but doesn't apply it by default (camp 2 in practice). The choice for
T8 is whether to opt into camp 1 by canonicalizing at write-time, or
stay in camp 2 by hashing the unpacked default form.

### Sources

**(7.a) IPLD DAG-CBOR.** From
`https://ipld.io/specs/codecs/dag-cbor/spec/`:

> DAG-CBOR is a codec that implements the IPLD Data Model as a
> subset of CBOR, plus some additional constraints for hash
> consistent representations. … DAG-CBOR requires that there exist
> a single, canonical way of encoding any given set of data, and
> that encoded forms contain no superfluous data that may be ignored
> or lost in a round-trip decode/encode.

Canonical rules: keys sorted bytewise lex, integers shortest
encoding, no indefinite-length items. **Adding a field to an IPLD
record changes the CID** — IPLD doesn't claim "schema evolution
preserves CIDs." It treats every byte change as a new content
address, and uses *separate* mechanisms (IPLD Schemas) to express
"v2 is a refinement of v1."

**(7.b) ATproto Lexicon evolution rules.** From
`https://atproto.com/specs/lexicon`:

> - Any new fields must be optional
> - Non-optional fields can not be removed. A best practice is to
>   retain all fields in the Lexicon and mark them as deprecated if
>   they are no longer used.
> - Types can not change
> - Fields can not be renamed
>
> If larger breaking changes are necessary, a new Lexicon name must
> be used.

These are *almost identical* to capnp's evolution rules — append
fields, never rename, never repurpose, never reuse, breaking change
= new identity. **What ATproto explicitly does NOT promise**: that
adding an optional field to a Lexicon preserves the CID of records
that don't set it. The doc dodges this question. In practice,
DAG-CBOR's truncation rule for empty maps means an *omitted* optional
field doesn't appear in the bytes, so the CID is stable for
not-set instances — same property as capnp canonical form, achieved
via different mechanism (omit-on-absent vs. truncate-trailing-zeros).

ATproto also adds:

> It can be ambiguous when a Lexicon has been published and becomes
> "set in stone." At a minimum, public adoption and implementation
> by a third party, even without explicit permission, indicates that
> the Lexicon has been released and should not break compatibility.

This is policy guidance — when does a schema cross the "you can no
longer break it" threshold?

**(7.c) Protobuf's official non-position.** From
`https://protobuf.dev/programming-guides/serialization-not-canonical/`:

> protobuf serialization is not (and cannot be) canonical.
> Deterministic serialization is not canonical. The serializer can
> generate different output for many reasons:
> - Schema changes
> - Application changes
> - Different build flags (optimization vs. debug)
> - Protobuf library updates
>
> Users who need canonical serialization, e.g. persistent storage in
> a canonical form, fingerprinting, etc, should define their own
> canonicalization specification and implement the serializer using
> reflection APIs rather than relying on this API.

Explicit warning: don't hash serialized protos. **This is the
contrast that makes capnp's canonical form valuable.** Capnp does
provide what protobuf refuses to.

### Implication for T8.6

**The math-friend's intuition that we have to pick Reading A vs.
Reading B is correct, and the two readings map to the IPLD/ATproto
camp (Reading A, with canonical encoding) vs. a hybrid (Reading B
with schema-version manifest).**

The IPLD/ATproto precedent strongly recommends: **commit to
canonical encoding at the producer call site**, treat the CID as the
version, and make schema evolution a *separate* concern handled at
the typed-reading level. This is Reading A done right, with
ecosystem precedent on its side.

The only friction: producer-side migration. Today our producer calls
`capnp::serialize::write_message(&mut f, &msg)` (the default
non-canonical form). Switching to canonical means:

1. Build the message via `set_root_canonical` instead of `set_root`,
   OR
2. Read the message back via `Reader::canonicalize()` and write
   *that*.

Both work. (1) is preferred — single-pass, no extra copy. The
migration is local to each producer call site:
- `cmd_parse.rs:705-731` (`write_source_file_record`)
- `cmd_parse.rs:733-760` (`write_ast_node_record`)
- `project.rs:565-604` (`write_binding_record`)
- `cmd_parse.rs:619-656` (`write_head_after_parse`)

Four call sites, each a small mechanical change. Plus a CI test
that asserts canonical-form bytes for a fixture (F8.6.4).

---

## Synthesis — the 3 things ADR-0014 must commit to, grounded in real precedent

The math-friend's analysis was rigorous but rested on one factually
incorrect premise — that capnp doesn't have canonical encoding. The
spec at `https://capnproto.org/encoding.html#canonicalization` says
otherwise; the Go runtime's public `Canonicalize` doc says
*"identical for equivalent structs, even as the schema evolves";* the
Rust runtime exposes `set_root_canonical`. The substrate **can** opt
into byte-stability across additive schema changes if it canonicalizes
at the producer.

**Therefore ADR-0014 must commit, in this order:**

### 1. Reading A — canonical encoding at the producer, Σ root over canonical bytes

**Reverse the math-friend's recommendation.** Adopt Reading A, not
Reading B. Concretely:

- Every producer call site (4 in current code) uses
  `Builder::set_root_canonical` (Rust) / equivalent in Go.
- `hash_segment_files` in `cmd_parse.rs:592-605` hashes the canonical
  bytes of each record concatenated, *with segment-table prefixes
  stripped* per the spec's bullet 2 (*"the segment table shall not
  be included"*). Today's code hashes the raw file bytes including
  segment headers — this needs adjustment.
- An additive schema change (append a field at `@N` with default
  value) will not advance Σ root for instances that don't set the
  new field. The substrate is *byte-stable across schema evolution
  for unchanged data*.
- Cross-runtime fixture round-trip (F8.6.4) becomes a strong CI
  invariant: any divergence between Rust and Go canonical bytes
  is a bug in the runtime, not a substrate concern.

**Why this beats Reading B:** It matches the published precedent
(IPLD, ATproto) of "the CID is the version." It eliminates the need
for a `schemaVersion` manifest at the substrate level — which has no
precedent in workerd or sandstorm anyway. It makes Σ a true
content-addressed log, where two producers running compatible
schemas on identical data emit identical bytes.

**Why we have to commit explicitly:** Today's producer uses
`serialize::write_message` (non-canonical default). Switching is a
4-call-site mechanical change but it has to happen for the property
to hold. ADR-0014 must specify "canonical bytes only" as a
substrate invariant, with a CI gate that fails any new producer call
site that uses the non-canonical path.

### 2. Append-only-additive evolution, enforced by CI, no first-class versioning field

**Codify the workerd / sandstorm pattern.** The schema is its own
version manifest:

- Append fields at next `@N`, never rename, never repurpose, never
  reuse. (math-friend's R1, R2, R3.)
- Fileids (`@0xb0c0debaadc0deb0` etc.) are stable for the life of
  the file. CI gate: an allowlist of (filename, fileId) pairs, any
  drift fails the build. (math-friend's R6 / F8.6.6.)
- Deprecated fields use a `# DEPRECATED:` docstring prefix
  (workerd convention, §2.b). Optional: a CI step that lists all
  deprecated fields in a generated manifest.
- **No `schemaVersion :UInt64` field on `Head` or anywhere else.**
  Capnp gives us the fileId + ordinals + canonical form; that's the
  version surface.
- Future migration path (deferred to a follow-on ADR): adopt
  workerd-style `$introducedInGen(N)` annotations on individual
  fields once we have a multi-runtime annotation library.

**Why this beats math-friend Q2:** Manifest sidecars and inline
schemaVersion fields both have weak precedent. Workerd and sandstorm
both decline to put a version counter in their wire format. The
schemas themselves, addressed by their canonical bytes, are the
manifest. We can verify "consumer has the same schemas as producer"
by *hashing the .capnp source files* and comparing — no need for a
new wire-format field. (See math-friend's Q2-(b) reduced to its
data-only essence: not a new struct, just a build-time check.)

### 3. Pin the toolchain triplet, ship the cross-runtime fixture suite

**Adopt the math-friend's §4.3 pin policy verbatim:** compiler,
generators, runtimes pinned to exact versions; generated bindings
committed; cross-runtime fixtures in
`rs/ll-core/schema-capnp/tests/fixtures/` consumed by both LLO Rust
CI and mache Go CI. We are slightly ahead of community practice
here (the capnp project itself doesn't ship cross-runtime fixtures),
which is appropriate for a substrate.

**Why this beats the alternatives:** workerd's Bazel-pin pattern is
strong but requires Bazel; community Rust pins (`capnpc = "0.20"`)
are too loose; community Go practice (`@latest`) is what mache PR-1's
review caught. Exact pins + committed bindings + canonical-form
fixtures collapse the version-drift surface to a single, auditable
gate.

---

## Appendix — citation index for verifiability

| Claim | Source |
|---|---|
| Canonical form spec (truncation rule) | `https://github.com/capnproto/capnproto/blob/master/doc/encoding.md#canonicalization` (lines 358–390); also `https://capnproto.org/encoding.html#canonicalization` |
| Go `Canonicalize` schema-evolution promise | `https://github.com/capnproto/go-capnp/blob/main/canonical.go` lines 1–13 |
| Rust `set_root_canonical` API | `https://github.com/capnproto/capnproto-rust/blob/master/capnp/src/message.rs` lines 553–568 |
| Rust `canonicalize` (two-copy) | same file, lines 327–344 |
| Issue #2171 (Kenton's confirmation) | `https://github.com/capnproto/capnproto/issues/2171` |
| Workerd compatibility-date.capnp | `https://github.com/cloudflare/workerd/blob/main/src/workerd/io/compatibility-date.capnp` |
| Workerd compatibility system overview | `https://deepwiki.com/cloudflare/workerd/4-compatibility-system` |
| Sandstorm grain.capnp / AppPersistent | `https://github.com/sandstorm-io/sandstorm/blob/master/src/sandstorm/grain.capnp` |
| Persistent / SturdyRef definition | `https://github.com/capnproto/capnproto/blob/master/c++/src/capnp/persistent.capnp` |
| Schema language / file IDs / ordinals | `https://capnproto.org/language.html` |
| capnpc README (binary-on-PATH note) | `https://docs.rs/capnpc/latest/capnpc/` |
| capnp-rust issue on tool distribution | `https://github.com/capnproto/capnproto-rust/issues/182` |
| IPLD DAG-CBOR canonical form | `https://ipld.io/specs/codecs/dag-cbor/spec/` |
| ATproto Lexicon evolution rules | `https://atproto.com/specs/lexicon` |
| ATproto data model (DRISL) | `https://atproto.com/specs/data-model` |
| Protobuf "not canonical" position | `https://protobuf.dev/programming-guides/serialization-not-canonical/` |
| Go canonical_test.go | `https://github.com/capnproto/go-capnp/blob/main/canonical_test.go` |

---

*End of findings.*
