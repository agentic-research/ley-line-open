@0xca7eca7eca7eca7e;

# Go codegen annotations (inert for capnpc-rust; consumed by capnpc-go
# to produce clients/go/leyline-schema/cache/cache.capnp.go). Mirrors
# common.capnp / binding.capnp pattern.
using Go = import "/go.capnp";
$Go.package("cache");
$Go.import("github.com/agentic-research/ley-line-open/clients/go/leyline-schema/cache");

# Cache lockfile — content-addressed manifest pointing into the BlobStore
# substrate (ley-line-open-783d72 + ley-line-open-bb0316). Producer-
# agnostic; consumed by mache (mache-aeb262 portable code-intel db),
# me-bundle (ley-line-open-dffb77 portable identity-rooted state), and
# any future substrate consumer that needs (input_hash, processor) →
# chunk_hash mappings.
#
# See `docs/adr/0021-cache-lockfile-schema.md` for the full rationale,
# the TOML on-disk and OCI JSON wire forms, and consumer-side surface
# expectations.
#
# Ordinal discipline: append at next ordinal with default. Never rename,
# never remove — leave a hole. ADR-0014 §3 schema-evolution contract.

# common.Hash (32-byte BLAKE3, Σ §3.4) is the canonical hash primitive.
# Locked to BLAKE3 by substrate decision — NOT a placeholder for
# arbitrary digest. See common.capnp.
using Common = import "common.capnp";

# Top-level lockfile entry. One CacheLockfile per (producer, scope).
# Scope is producer-defined: per-repo (mache), per-identity (me-bundle),
# per-session (agent-corpus), etc.
struct CacheLockfile {
  meta @0 :Meta;
  sources @1 :List(SourceEntry);
  topology @2 :List(TopologyEdge);
  # Assembled-output hash. For mache: the materialized .db blob hash.
  # For me-bundle: the manifest-root hash signet signs over. For
  # agent-corpus: the observation-lattice root. Producer-defined
  # semantics; substrate doesn't interpret it.
  root @3 :Common.Hash;
}

# Identifying metadata. Pins versions so a lockfile generated with
# producer X version 0.4.5 against schema 1.2.3 either restores
# deterministically or fails loudly — never silently corrupts.
struct Meta {
  # Short-name (v1 convention, ADR-0021 open question 2). Reverse-DNS
  # reserved for v2 if collisions appear. Examples: "mache",
  # "me-bundle", "agent-corpus".
  producer @0 :Text;
  producerVersion @1 :Text;
  # Capnp triplet pin per ADR-0014 §3. Restore implementations MUST
  # verify they support this schemaVersion before consuming the lockfile.
  schemaVersion @2 :Text;
  # Every processor that touched an input on the producer side. Restore
  # requires the same processors (or compatible-and-verified
  # equivalents) to reproduce chunk_hash from input_hash.
  inputProcessors @3 :List(ProcessorVersion);
  # ms-precision UTC epoch. Diagnostic only — restore semantics depend
  # on hashes, not timestamps. Stored for cache-pruning policy + audit.
  generatedAtMs @4 :UInt64;
}

# Pinned tool version used during production. Examples:
#   { kind: "tree-sitter-go",   version: "0.21.0" }
#   { kind: "blake3",            version: "1.5.0" }
#   { kind: "signet-sign",       version: "0.3.0" }
struct ProcessorVersion {
  kind @0 :Text;
  version @1 :Text;
}

# One source → one chunk in the cache. The relationship is:
#
#   chunk_hash = processor(input bytes, ProcessorVersion)
#
# where `processor` is identified by `meta.inputProcessors`. Restore
# either:
#
#   1. fetches chunk_hash from CAS and serves it (hot path), OR
#   2. fetches input bytes, re-runs processor, asserts the produced
#      chunk hash matches chunk_hash, then serves (fallback path).
#
# Mismatched chunk_hash on re-production is a hard fail — the
# processor or input drift signals lockfile staleness.
struct SourceEntry {
  # Producer-defined identifier. For mache: repo-relative source path
  # (`src/auth.go`). For me-bundle: bundle-relative manifest path
  # (`transcripts/2026-05-21.jsonl`). For agent-corpus: observation
  # key. Substrate doesn't interpret.
  path @0 :Text;
  # BLAKE3 of the input bytes pre-processor, post-LFS-checkout. No
  # normalization (CRLF, trailing-newline). If the consumer wants
  # normalization, do it in a pre-processor that becomes a
  # ProcessorVersion entry.
  inputHash @1 :Common.Hash;
  # CAS hash of the derived chunk in the BlobStore. The thing restore
  # actually fetches.
  chunkHash @2 :Common.Hash;
  # Producer-defined vocabulary. mache uses `"go-source"`,
  # `"rust-source"`, etc. (one per language). me-bundle uses
  # `"transcript-turn"`, `"bead-snapshot"`, `"mailbox-message"`, etc.
  # ADR-0021 open question 3: kept free-form Text in v1.
  kind @3 :Text;
}

# Dependency edge in the sheaf topology. `from` and `toSource` are both
# `path` values from `sources`. The edge means: when `from`'s
# input_hash changes, `toSource`'s chunk_hash MUST be reproduced.
#
# Consumer semantics:
#   mache: leyline-sheaf edges (Go file imports another Go file → edge)
#   me-bundle: typically empty (transcript chunks are independent)
#   agent-corpus: observation derivation order
struct TopologyEdge {
  from @0 :Text;
  toSource @1 :Text;
}
