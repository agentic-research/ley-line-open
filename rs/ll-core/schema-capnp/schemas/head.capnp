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

  # Unified code-fact IR (ADR-0026 / mache ADR-0023): count of
  # `fact_edges` rows whose `dst` is NULL because a reference/call token
  # did not resolve to a definition symbol in this db. A binding-fidelity
  # ratchet: the W5 gate asserts this stays <= baseline, so a producer
  # change that silently drops resolution shows up as a rising count
  # rather than a silently-zeroed downstream JOIN (the be6136 lesson).
  # Zero for a db with no IR tables. Counted over the whole db post-
  # COMMIT, so it reflects the full graph, not just this run's delta.
  unboundFacts @4 :UInt64;
}
