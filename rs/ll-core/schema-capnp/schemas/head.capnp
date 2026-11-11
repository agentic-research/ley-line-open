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

  # The count of `node_refs` rows whose `token` has no `node_defs` row in
  # this db: references that resolve to no definition. Recounted over the
  # whole db by every parse pass that changes `node_refs` or `node_defs`
  # (a pass that changes neither carries the count recorded with them), and
  # stored with the rows as `_meta.unbound_facts` (docs/TABLE_CONTRACT.md).
  # It depends on the extraction epoch: an epoch that emits more refs (epoch
  # 6, value refs) raises it with no change in resolution. Zero for a db
  # with no refs tables. A diagnostic; no gate in LLO ratchets it.
  unboundFacts @4 :UInt64;

  # Signature over the canonical head digest — NOT over rootHash alone.
  # The digest (v3) is BLAKE3_derive_key("leyline head digest v3",
  # generation LE-8 ‖ rootHash ‖ parentHash ‖ storeRoot ‖ parserId ‖ P),
  # P = 0x00 without a parentDigest, 0x01 ‖ parentDigest with one; see
  # `leyline_core::head_digest`. Binding all six stops a signature being
  # replayed at another generation, grafted onto a forked chain, or cited for
  # a store or derivation it did not attest. A head that does not stamp
  # storeRoot and parserId has no claim and is never signed; a signature on
  # one does not verify. v1 (the first three fields only) and v2 (treeRoot
  # in storeRoot's position) are removed: a v1 or v2 signature does not
  # verify under v3, and a head carrying `retiredTreeRoot` is refused as a
  # v2 head before any signature is checked (ADR-0047 M1). Ed25519
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

  # RETIRED by ADR-0047 M1 (bead `ley-line-open-73c6ee`): was `treeRoot`,
  # the flat `leyline/tree-root/v1` fold with `parserId` as its params, so a
  # grammar or epoch bump renamed an unchanged store. No producer writes it.
  # A head that carries it is a head digest v2 head: every reader refuses it
  # with a typed error (`HeadFormatError::RetiredDigestV2`) and never reads
  # it as a claim, a parent digest or a store identity. The ordinal is kept
  # (ADR-0014: never reused) so the refusal can see it.
  retiredTreeRoot @7 :Common.Hash;

  # ADR-0040 D1 — the identity of the derivation the projection was
  # computed under: a tagged fold (scheme `leyline/parser-id/v1`) over the
  # IR schema version, the projection schema version, the extraction,
  # injection and query-set epochs (the last covers arena-resident query
  # overrides by content hash), and every compiled grammar's node-kind and
  # field tables. Two arenas with the same store under different
  # `parserId`s are different derivations and must not share derived
  # caches. It is NOT folded into `storeRoot` (ADR-0047 law C7): the
  # parser-qualified identity is the pair, bound by the head digest and by
  # `_meta.derived_from`.
  parserId @8 :Common.Hash;

  # Bead ley-line-open-b68fa6 — the v3 head digest of the previous head's
  # claim (`leyline_core::head_digest`), bound into this head's signature.
  # `parentHash` names only the previous run's segments, and a no-op rerun
  # writes the same segments in every arena, so it does not identify which
  # chain this head extends; this field names the previous claim, store
  # included. Empty when there is no previous head or the previous head had
  # no claim (it stamped no storeRoot/parserId); the digest encodes that
  # absence with a presence byte, never as zero. A v3 head never chains onto
  # a v2 head: the v2 head is refused, so its digest is never computed.
  #
  # Additive per ADR-0014 §1: unset does not change canonical bytes.
  parentDigest @9 :Common.Hash;

  # ADR-0047 D-ii / M1 — the store identity, beside the run receipt above.
  #
  # `rootHash` names the segments ONE RUN wrote (truncated per run, so an
  # incremental parse's root covers only the files it re-parsed, and the
  # source record carries absolute paths and mtimes). `storeRoot` names the
  # STORE: a directory Merkle tree over the store's manifest `_tree`
  # (ADR-0045 T1/T2), with no parser input (ADR-0047 law C7: a grammar,
  # epoch or projection bump does not rename an unchanged store).
  #
  # Each directory node is a `PartitionSpec` fold (domain RowSet, scheme
  # `leyline/store-tree/v1`, params EMPTY, `canonVersion` 1; ADR-0032 D2)
  # over its direct children, sorted by the child name's bytes (a name is
  # one path component: no `/`, not empty, not `.` or `..`). Child `i`
  # contributes two entries:
  #   (BLAKE3(name bytes), 2i,   byte length of the name)
  #   (child,              2i+1, git mode of the child)
  # where `child` is the file's blob hash (`source_blobs` key; a symlink's
  # blob is its target) for a file (`0o100644`, `0o100755`, `0o120000`), and
  # the child directory's node address for a directory (`0o040000`). An
  # empty directory is the fold of no entries. `storeRoot` is the root
  # directory's node address; a subtree's node address is the `storeRoot`
  # of a store holding exactly that subtree. The manifest's directory
  # entries are implied by their children, so a store is a function of its
  # files and its empty directories. Reference:
  # `rs/ll-open/cli-lib/src/store_root.rs` `StoreTree::root`; known-answer
  # vectors: `rs/ll-core/schema-spec/sigma/v1/test-vectors/constructions.json`
  # `storeRoot`. Computed after COMMIT from the whole `_tree`, so it is the
  # same for a cold parse, an incremental parse, a scoped reparse, a second
  # directory, a touched mtime and a parser upgrade, and differs when one
  # byte, one path or one mode differs. One edit refolds only the directory
  # nodes on its path. Anyone holding the arena recomputes it (`leyline
  # verify-head`); a mismatch is refused.
  #
  # SIGNED: the head signature's digest (`head_digest`, context
  # "leyline head digest v3") binds `storeRoot` and `parserId` separately,
  # with `generation`, `rootHash`, `parentHash` and `parentDigest`. A head
  # that does not stamp both has no claim and is never signed.
  #
  # Additive per ADR-0014 §1: a head that does not set it has the same
  # canonical bytes as before it existed. Empty = not stamped.
  storeRoot @10 :Common.Hash;
}
