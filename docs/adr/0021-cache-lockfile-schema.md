# ADR-0021 — Cache lockfile schema as a substrate primitive (capnp source, TOML on-disk, OCI JSON wire)

- **Status:** Proposed (2026-05-21)
- **Tracking bead:** ley-line-open-`<filed alongside this ADR>`
- **Pairs with:**
  - ADR-0014 (capnp as protocol — anchors cross-process IDL discipline)
  - ley-line-open-783d72 (BlobStore trait — the CAS this lockfile references)
  - ley-line-open-bb0316 (FsBlobStore — multi-blob layout this lockfile assumes)
  - ley-line-open-ae7a35 (leyline-sheaf — dependency topology this lockfile serializes)
  - cloister ADR-0025 (Bidi TOML ↔ capnp — the on-disk rendering path)
  - cloister `docs/seams/portable-units.md` (Portable Units — OCI envelope this lockfile rides inside)
  - mache ADR-0020 (Consumer: mache adopts this schema for the portable-cache feature)

## Context

The substrate needs a manifest shape that maps **(input hash + processor version + schema version) → CAS chunk hash** across the dependency topology a sheaf already tracks. Multiple consumers want this same shape:

- **mache-aeb262** — portable code-intel db. Inputs are source files + parser version; outputs are per-file parse chunks. Topology comes from `leyline-sheaf`.
- **ley-line-open-dffb77** — me-bundle. Inputs are transcripts / agent-logs / bead-db files; outputs are signet-signed chunks. Topology is a flat list per producer.
- **future agent-corpus consumers** — observation chunks keyed by source events.

Each consumer needs the same primitive: a content-addressed lockfile naming chunks in the substrate's CAS. If each consumer invents its own format, they fragment around the same problem. If the substrate provides the schema, all consumers compose.

Mache ADR-0020 (sibling repo) originally landed the schema in mache. User correction 2026-05-21: LLO already owns the cache substrate (BlobStore, sheaf, daemon protocol) — the schema belongs here, where any substrate consumer can adopt it.

## Decision

**Ship `cache.capnp` alongside `daemon.capnp` in `rs/ll-core/public-schema/capnp/` as the canonical lockfile schema. Capnp is source-of-truth; TOML is the on-disk hand-readable serialization via cloister's bidi pipeline (ADR-0025); JSON-on-OCI is the wire form when chunks are pushed to a `build-cache/v1` provider.**

### Schema (capnp source)

```capnp
@0xf00d…;
using Hash = import "/common.capnp".Hash;

struct CacheLockfile {
  meta @0 :Meta;
  sources @1 :List(SourceEntry);
  topology @2 :List(TopologyEdge);
  root @3 :Hash;            # the assembled-output hash; chains to the chunk graph
}

struct Meta {
  producer @0 :Text;        # "mache@0.x" / "me-bundle@0.x" / consumer identifier
  producerVersion @1 :Text;
  schemaVersion @2 :Text;   # capnp triplet pin per ADR-0014
  inputProcessors @3 :List(ProcessorVersion);  # e.g. tree-sitter-go@0.21.0
  generatedAt @4 :UInt64;
}

struct ProcessorVersion {
  kind @0 :Text;            # "tree-sitter-go" / "blake3" / "signet-sign" / …
  version @1 :Text;
}

struct SourceEntry {
  path @0 :Text;            # repo-relative or producer-relative
  inputHash @1 :Hash;       # BLAKE3 of the input bytes
  chunkHash @2 :Hash;       # CAS hash of the derived chunk
  kind @3 :Text;            # producer-defined: "go-source", "transcript-turn", …
}

struct TopologyEdge {
  from @0 :Text;            # path/key in sources
  toSource @1 :Text;        # depends on this entry's chunk
}

struct Hash {
  algo @0 :Text;            # "blake3" v1; reserves room for sha-256/512 later
  bytes @1 :Data;           # 32 bytes for BLAKE3
}
```

### TOML on-disk (cloister bidi pipeline ADR-0025)

```toml
[meta]
producer         = "mache"
producer_version = "0.x.y"
schema_version   = "<capnp triplet pin per ADR-0014>"
generated_at     = 1748345600

[[meta.input_processors]]
kind    = "tree-sitter-go"
version = "0.21.0"

[[sources]]
path       = "src/auth.go"
input_hash = "blake3:abc..."
chunk_hash = "blake3:111..."
kind       = "go-source"

[[topology]]
from      = "src/main.go"
to_source = "src/auth.go"

root = "blake3:9f8e..."
```

### JSON-on-OCI wire form

When a `build-cache/v1` provider receives a lockfile, it lives as an OCI artifact:
- Lockfile manifest: `application/vnd.leyline.cache-lockfile.v1+json`
- Each chunk: separate OCI blob, addressed by digest
- Lockfile references chunks via standard OCI digests

`oras pull` / `cosign verify` / cloister's OCI registry endpoint (ADR-0009 path) all work without consumer-side changes.

## Why this triplet (vs alternatives)

Briefly — full reasoning in mache ADR-0020's "Alternatives considered":

- Pure JSON / pure TOML: loses IDL discipline; ADR-0014 establishes capnp anchors cross-process contracts.
- Pure capnp on disk: not diff-friendly; reviewers can't inspect.
- Cargo.lock-shape (TOML, no codegen): works for single-tool worlds; doesn't compose with the Portable Units / matchmaker substrate.

This triplet matches existing prior art in the ecosystem:
- `daemon.capnp` → JSON wire (already shipped)
- `cluster.capnp` → TOML on-disk (cloister ADR-0025)
- Now: `cache.capnp` → both rendering paths

## Consumer surface

Consumers import the capnp schema and pick a rendering target:

```rust
// Rust (LLO-internal consumers — agent-corpus, me-crate, etc.)
use leyline_public_schema::cache::{CacheLockfile, SourceEntry, Hash};

// Go (mache, future Go consumers via clients/go/leyline-schema/cache/)
import "github.com/agentic-research/ley-line-open/clients/go/leyline-schema/cache"

// TS (cloister, via schema-bridge zod codegen per ADR-0022)
import { CacheLockfile } from "@leyline/schema/cache";
```

Each consumer decides:
- Where its lockfile lives on disk (gitignored vs committed; per-repo vs per-bundle)
- How its `producer` / `kind` strings are namespaced
- Topology semantics (mache: sheaf edges; me-bundle: flat per-producer; agent-corpus: TBD)

Consumer-side decisions are NOT part of this ADR. They live in consumer-side ADRs (mache ADR-0020 today; ll-open me-bundle ADR when filed).

## Consequences

### Positive

- One schema serves mache, me-bundle, agent-corpus, and any future substrate consumer.
- Drift between consumers is impossible at the schema layer — codegen makes the contract enforceable.
- TOML on-disk reuses cloister's bidi pipeline (ADR-0025) for free.
- OCI JSON wire interops with the existing cloister OCI registry endpoint AND with Portable Units' artifact distribution model (cloister-a99b45).
- Future consumers don't relitigate the format question.

### Negative

- Three renderings to keep aligned (capnp source ↔ TOML ↔ OCI JSON). A test gate verifies round-trip: TOML→capnp→TOML and capnp→OCI-JSON→capnp.
- Schema evolution follows ADR-0014 §3 capnp triplet pin discipline. Lockfiles record the schema version they were generated against; stale lockfile + new schema = loud error, not silent corruption.

### Neutral

- No new substrate dependencies. Capnp pipeline already used; schema-bridge already exists; OCI registry already shipped.

## What this ADR does NOT decide

- **Where each consumer's lockfile lives.** mache decides for mache (see mache ADR-0020); me-bundle decides for me-bundle (separate ADR).
- **`build-cache/v1` capability interface.** That's a cloister-side spec — `cloister-spec/build-cache/v1/` — filed when a consumer needs remote transport (mache-aeb262 Phase 3 is the likely first).
- **Topology semantics per consumer.** This ADR lets each consumer populate `topology` however its substrate dictates (sheaf edges vs flat list vs DAG); semantics are consumer-defined.
- **Multi-arch.** v1 assumes one lockfile per `(input set, processor versions)`; cross-arch composition is v2.

## Open questions

1. **`common.capnp` Hash type — does it exist yet?** If not, this ADR introduces it. If yes, reference the existing type. Verify against `rs/ll-core/public-schema/capnp/` before implementation.
2. **`producer` namespacing convention.** Reverse-DNS (`org.example.tool`) or short-name (`mache`)? Lean: short-name in v1; promote to reverse-DNS if collisions appear.
3. **Should `kind` be an enum at the schema level?** Lean: free-form `Text` in v1 — consumers define their own vocabulary. Promote to per-consumer enum if multiple consumers want to interop on `kind` strings.

## Out of scope

- The actual `build-cache/v1` capability spec (cloister-side, filed when needed).
- Consumer CLI surfaces (`mache push`, `me-bundle pack`, etc.).
- Cryptographic signing of lockfiles (consumer-side concern; mache may not sign, me-bundle MUST sign via signet).
