@0xc7c7ada1403b9f78;

# Go codegen annotations (inert for capnpc-rust; consumed by capnpc-go).
using Go = import "/go.capnp";
$Go.package("head");
$Go.import("github.com/agentic-research/ley-line-open/clients/go/leyline-schema/head");

# Head — Σ root pointer for a file-backed db.
#
# T8.5 (decade ley-line-open-9d30ac, thread T8/capnp-as-protocol).
# Producer: rs/ll-open/cli-lib/src/cmd_parse.rs (post-parse), and
# eventually rs/ll-open/cli-lib/src/cmd_daemon.rs::snapshot_to_arena
# once daemon switches to file-backed live db (5f7100-15a).
#
# This is the file-backed analogue of the daemon-side
# `Controller::current_root` (T2.1). Each successful parse run hashes
# its capnp event segment(s) and writes a new Head with the resulting
# root and a parent_hash chained to the previous Head. The chain is
# the Σ history.
#
# Lives at `${db}.head.capnp` next to the .db.

using Common = import "common.capnp";

struct Head {
  # BLAKE3-32 of the segment(s) this run produced.
  # Concatenation order is canonical: source.capnp || ast.capnp ||
  # bindings.capnp (lexicographic by suffix). Empty segments hash as
  # the empty input.
  rootHash @0 :Common.Hash;

  # Previous root — zero on the first parse run for this db.
  # rootHash(parse_n) == parentHash(parse_{n+1}).
  parentHash @1 :Common.Hash;

  # Monotonic counter — first parse = 1; increments per run.
  # Mirrors `Controller::generation` (T2.1) for the file-backed path.
  generation @2 :UInt64;

  # Total bytes that contributed to rootHash. Sanity field — lets
  # consumers detect a torn-write or partial-segment scenario without
  # re-hashing.
  segmentBytes @3 :UInt64;

  # Unified code-fact IR (ADR-0027 / mache ADR-0023): count of
  # `fact_edges` rows whose `dst` is NULL because a reference/call token
  # did not resolve to a definition symbol in this db. A binding-fidelity
  # ratchet: the W5 gate asserts this stays <= baseline, so a producer
  # change that silently drops resolution shows up as a rising count
  # rather than a silently-zeroed downstream JOIN (the be6136 lesson).
  # Zero for a db with no IR tables. Counted over the whole db post-
  # COMMIT, so it reflects the full graph, not just this run's delta.
  unboundFacts @4 :UInt64;

  # Signature over the canonical head digest — NOT over rootHash alone.
  # The digest is BLAKE3(generation LE-8 ‖ rootHash ‖ parentHash); see
  # `leyline_core::head_digest`. Binding all three stops a signature being
  # replayed at another generation or grafted onto a forked chain. Ed25519
  # (64 bytes), matching the frozen leyline-net/v1 manifest so the in-flight
  # manifest and the at-rest head share one scheme and one trust root.
  #
  # Empty when the head is unsigned. Additive field: an unset field does not
  # change canonical bytes for existing instances (ADR-0014 §1), so adding
  # this does not advance Σ root for unchanged data.
  signature @5 :Data;

  # Canonical key identifier of the signing key, so a verifier can select the
  # right public key. The ONE derivation signet ratified across the substrate
  # (signet ADR-012 / bead signet-248d17):
  #   kid = lowercasehex(SHA-256(canonical SPKI DER)[:16])   — 32 hex chars.
  # Stored as the 32-byte ASCII hex string so it is byte-identical to notme's
  # JWKS `kid` and cloister's resolved kid. It SELECTS a key; it never confers
  # authority — a verifier still checks the signature (ADR R1 parity). Empty
  # when unsigned. Additive field (ADR-0014 §1): unset does not change
  # canonical bytes.
  signerKid @6 :Data;

  # ADR-0040 D1 — the tree identity, beside the run receipt above.
  #
  # `rootHash` names the segments ONE RUN wrote (truncated per run, so an
  # incremental parse's root covers only the files it re-parsed, and the
  # source record carries absolute paths and mtimes). `treeRoot` names the
  # TREE: a tagged fold (scheme `leyline/tree-root/v1`, `PartitionSpec`
  # RowSet domain) over every `_source` row as `(BLAKE3(relative path), 2i,
  # len)` and `(content_hash, 2i+1, 0)` with `i` the rank in path order,
  # with `parserId` as the spec's params. Computed after COMMIT from the
  # whole `_source` table, so it is the same for a cold parse, an
  # incremental parse, a scoped reparse, a second directory, and a touched
  # mtime, and differs when one byte of one file differs. Anyone holding
  # the arena recomputes it (`leyline verify-head`); a mismatch is refused.
  #
  # Additive per ADR-0014 §1: a head that does not set these fields has the
  # same canonical bytes as before they existed. Empty = not stamped
  # (written by a pre-ADR-0040 producer).
  treeRoot @7 :Common.Hash;

  # ADR-0040 D1 — the identity of the derivation `treeRoot` was computed
  # under: a tagged fold (scheme `leyline/parser-id/v1`) over the IR schema
  # version, the projection schema version, the extraction, injection and
  # query-set epochs (the last covers arena-resident query overrides by
  # content hash), and every compiled grammar's node-kind and field tables.
  # Two arenas with the same source under different `parserId`s are
  # different derivations and must not share derived caches.
  parserId @8 :Common.Hash;
}
