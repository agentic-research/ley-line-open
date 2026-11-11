# Ley Line Open Agent Instructions

This file is the repository-local operating contract for agents working in LLO. It complements the global agent rules and the canonical project documentation; it does not replace either one.

## Start Here

For non-trivial work:

1. Use `rsry` to search for an existing bead before changing code. Create one when no suitable bead exists, record meaningful progress, and close it only when its acceptance criteria are verified. Beads, not GitHub issues or ad hoc investigation logs, are the work ledger.
2. Use `mache.get_overview` before exploring unfamiliar code, then use its symbol and community tools to narrow the relevant seam. If mache is unavailable, record the tooling failure in its existing bead before falling back to targeted repository reads.
3. Use `lectio` when available for repository history and observational context. Tooling absence must not be mistaken for evidence about the code.
4. Read the governing material in this order: `README.md`, `docs/ROADMAP.md`, `docs/TECHNICAL_OVERVIEW.md`, `docs/ARCHITECTURE.md`, `docs/TABLE_CONTRACT.md`, then the relevant ADR, specification, crate README, and tests.

Treat a bead as a pointer, not as proof. Verify every claim against the current source, generated surfaces, and tests.

## Direction

`docs/ROADMAP.md` is the plan: v1.0 is **single host, portable** (one daemon owns one store on one machine, the same on macOS and Linux; epic `ley-line-open-b2e687`). Prefer work that closes a v1 blocker. Multi-host stores, blob-mapped reads and construct-aware merge come after v1 unless the roadmap says otherwise. A pull request that closes a roadmap bead or changes the plan updates `docs/ROADMAP.md` in the same pull request.

Two design rules the roadmap depends on:

- **A write's behavior follows its path's class, not a mode.** Authored paths are proposals (merged, durable, in `storeRoot`). Derived paths, the ones the tree's ignore rules match, are never proposals; they live in per-run scratch at native speed (`39998b`).
- **Accepted means durable in `live.db`.** Publication of the arena is a derived, coalesced step (#478); a reply promises only what one durable append guarantees (`98d0c3`).

## Review Routing

Use the repository-owned review workflow for changes or claims that cross an architectural seam:

- `/leyline-review <target>` — orchestrates the smallest useful set of specialist reviews and synthesizes their findings across producers, persistence, verifiers, consumers, and tests.
- `identity-domain-auditor` — use for hashes, roots, addresses, TypeIDs, canonicalization, identity migrations, or claims that two identifiers are equivalent.
- `publication-state-adversary` — use for snapshot publication, CDC activation, crash consistency, generation/root transitions, and partial-write or recovery behavior.
- `cross-runtime-contract-auditor` — use for Cap'n Proto, SQL projection ABI, daemon wire changes, Rust/Go/TypeScript consumers, compatibility metadata, and generated bindings.
- `execution-boundary-adversary` — use for embedded runtimes, confinement, capabilities, brokers, lifecycle state, backend selection, host/guest protocols, and fail-closed behavior.

All four specialists are defined in `.claude/agents/` and are read-only. They load `.claude/skills/leyline-review-kit/SKILL.md`, which defines the shared evidence and finding contract. `.claude/skills/leyline-review/SKILL.md` owns orchestration and synthesis.

Dispatch only the lenses the target actually needs. Specialists may run in parallel because they do not edit. Generic reviewers for security, theory, types, paradigms, or empirical validation remain useful complements; they do not replace the LLO-specific contract reviewers above.

## Identity and Authority Vocabulary

Do not use “root,” “hash,” or “identity” without naming the domain and authoritative bytes.

Every keyed fold below is the ADR-0032 partition fold (`BLAKE3_derive_key("leyline partition fold v1", …)` with the scheme named in the row, `rs/ll-core/core/src/partition.rs`) unless the row names its own `derive_key` context. Plain BLAKE3 over bytes (σ_bytes) is the one unkeyed construction: the blob hash, the CDC chunk hash, `current_root` and the rootfs digest are instances of it; every other construction is keyed, except `run_id` v1, which violates ADR-0047 law (c) until M3. ADR-0047 §3 is the conformance table for the constructions it lists, the rootfs digest among them, with its canonical bytes and known-answer vector (`execution/v1/test-vectors/rootfs-manifest.json`).

| Name | Authoritative for | Construction | Status |
| --- | --- | --- | --- |
| `Controller.current_root` | One exact serialized SQLite arena snapshot | BLAKE3 over the complete serialized byte image (`rs/ll-core/core/src/control.rs`) | Shipped |
| `Head.rootHash` | The Cap'n Proto segments produced by one parse run (a run receipt: truncated per run, carries absolute paths and mtimes) | Keyed fold `leyline/segment-root/v1` over the canonical bytes of the run's `source.capnp`, `ast.capnp` and `bindings.capnp` segment files, in that order (`cmd_parse.rs`, `hash_segment_files`) | Shipped |
| `Head.storeRoot` | The store's manifest: every `_tree` file entry as (relative path, git mode, blob hash) and every empty directory, with no parser input | Directory Merkle tree `leyline/store-tree/v1` over the committed `_tree` table: each directory node is a `RowSet` fold with empty params over its children in name-byte order, child `i` contributing `(BLAKE3(name), 2i, len(name))` and `(blob hash or child node address, 2i+1, git mode)`; recomputed by `leyline verify-head` (`rs/ll-open/cli-lib/src/store_root.rs`) | Shipped (ADR-0047 D-ii, step M1, v0.26.0; replaces the retired `Head.treeRoot`, whose presence refuses a head as head digest v2) |
| `Head.parserId` | The derivation the projection was computed under | Keyed `RowSet` fold `leyline/parser-id/v1` over the IR and projection schema versions, the extraction, injection and query-set epochs, and every compiled grammar's digest (`store_root.rs`, `ParserInputs::parser_id`); it moves whenever one of those moves (merkle-ast-v3 moved it) | Shipped (ADR-0040 D1) |
| Head digest | The signed claim a `Head` makes: generation, run receipt, previous receipt, store, parser and previous claim together | `derive_key("leyline head digest v3", generation_le8 ‖ rootHash ‖ parentHash ‖ storeRoot ‖ parserId ‖ P)`, `P` = `0x00`, or `0x01 ‖ parentDigest`; no claim is built while `storeRoot` or `parserId` is unstamped (`rs/ll-core/core/src/head_digest.rs`) | Shipped (v3, ADR-0047 M1); v1 and v2 are removed, and a head carrying `retiredTreeRoot` is refused (`HeadFormatError::RetiredDigestV2`) |
| `node_hash` | One deduplicated syntax subtree: κ kind (grammar kind for an anonymous token), terminal text, and child hashes in source order; location-free | merkle-ast-v3: `derive_key("leyline/ast-node/v3", tag ‖ uvarint(len(kind)) ‖ kind ‖ …)` (`cmd_parse.rs`, `hash_leaf`/`hash_internal`); stored once per subtree in `node_content` | Shipped (ADR-0047 D-i, step M2, v0.26.0; `IR_SCHEMA_VERSION` `merkle-ast-v3`) |
| Blob hash | One content-addressed blob, CDC chunk or CAS payload | BLAKE3 over that payload (σ_bytes) | Shipped |
| `run_id` | One authorized run: canonical `RunSpec` digest, grant id and replay key | `"run-" ‖ hex(BLAKE3("cloister/execution/v1/run-id" ‖ 0x00 ‖ u64le-length-prefixed fields))` (`rs/ll-open/runtime/src/authorization.rs`, `derive_run_id`); pinned by `execution/v1/test-vectors/run-id.json` | Shipped (v1, prefix-in-preimage, which violates ADR-0047 law (c)) |
| `run_id` v2 | The same run | `derive_key("cloister/execution/v1/run-id/v2", …)`, rendered `run-v2-<hex>` | Proposed only (ADR-0047 step M3, `ley-line-open-ab2093`) |
| Rootfs digest | One execution rootfs. v2: every directory with its mode, every regular file with its mode and BLAKE3, every symbolic link with its target bytes. v1 (frozen): its file list with each file's mode and BLAKE3 | BLAKE3 over the exact bytes of `rootfs.manifest.json`, versioned by its `version` field (`runtime/src/artifacts/rootfs_manifest.rs`): v2 `{"version":2,"entries":[{"type":"dir"|"file"|"symlink",…},…]}`, accepted in its canonical bytes only; v1 `{"version":1,"files":[{"path","mode","blake3"},…]}`, read in any byte form, written by no LLO producer. It is also the rootfs directory's name beneath the CAS root; verified on every resolve (`backends/libkrun/plan.rs`, `DirectoryRootfsResolver`) and named first in `RunReceipt.inputRoots`. `leyline rootfs import-oci` produces v2; the image's OCI SHA-256 digests are provenance and never compared with it | Shipped (v2 since `ley-line-open-4f08ea`; v1 frozen) |
| `content_id`, `nid`, `file_id`, `dir_id`, `name_id`, `kind_id` | Nothing outside one arena generation: each is a locator, an integer row key the arena assigned | Arena-local SQLite integers, one newtype per kind (`rs/ll-core/schema/src/locator.rs`), with no serde or Cap'n Proto encoding; `content_id` is the `node_content` row key, and the subtree's address is that row's `node_hash` | Shipped; a locator, not an identity: never on the wire, in a signed claim or in a cache key |
| SQL projection ABI | Consumer-visible relational shape and behavior | Tables, columns, constraints, semantics, fixtures (`projection-v13`) | Shipped; not an identity domain |
| Blob manifest address | A blob's CDC chunk list, as its own construction beside the blob hash | Keyed `ByteStream` fold `leyline/blob-manifest/v1` with the CDC parameters as params | Proposed only (ADR-0047 D-iv, step M4) |
| `manifestRoot` | CDC transport/dedup structure | Defined by ADR-0032 | Proposed only |
| `logicalRoot` | Derived-view validity | Defined by ADR-0032 | Proposed only |

`current_root`, `Head.rootHash`, `Head.storeRoot`, `node_hash`, the rootfs digest and blob hashes do not name the same bytes and are not interchangeable; `storeRoot` is the only one of them that names a source tree, and it names it independently of the parser (the parser-qualified identity is the pair `(storeRoot, parserId)`, bound by head digest v3 and `_meta.derived_from`). A locator is not an identity. Proposed identities must not appear in shipped APIs, documentation, or tests as though they already govern authority.

## Change Discipline

Trace every contract change through this seam:

`contract -> producer -> persistence -> verifier -> consumer -> test`

A change is incomplete when any required edge is missing. In particular:

- Edit normative schemas or generators first; do not hand-edit generated outputs.
- Regenerate every checked-in consumer surface and compatibility artifact affected by a schema change.
- Preserve unknown-field, framing, canonicalization, and versioning behavior across runtimes.
- Publication must be atomic from the reader's point of view. Bytes, generation, and the authoritative root must not expose a mixed state.
- Execution paths must fail closed. Confinement evidence must cover actual paths, sockets, network access, inherited descriptors, lifecycle transitions, and cleanup—not merely configuration intent.
- Keep policy ownership in Cloister and mechanism ownership in LLO. Do not duplicate backend selection or authorization semantics across the boundary.
- Never edit `CHANGELOG.md` in a change. Add one fragment, `changes/<bead-id>.md` or `changes/<bead-id>-<slug>.md`, whose first line is `Section: <Security|Breaking|Added|Changed|Fixed|Removed>` and whose remaining lines are the entry in the existing CHANGELOG style (`changes/README.md` has the template). Fragments never conflict between PRs; `tools/bump_release_version.sh` assembles them into the release's section, Security first, and deletes them. `task ci` lints them (`task lint:changelog-fragments`).

For concurrent implementation work, make bead file scopes explicit and use separate worktrees. Do not assign overlapping edits to the same schema, generated artifact, compatibility manifest, task definition, or architecture document. Read-only reviews are safe to parallelize.

## Verification

Run the narrowest test that proves the changed contract, then the relevant repository gates. Common gates include:

- `task runtime:test` for embedded runtime and execution contracts.
- `task compat:check`, `task gen:server-json:check`, and `task schema:consumer-fixture-test` for wire/schema changes.
- `task check` for the standard repository checks.
- `task smells` when architecture or dependency shape changes.
- `task ci` when the change warrants the full CI-equivalent surface.
- `task rs:mutants:pr` before every push that changes Rust: it mutation-tests the working-tree diff against `origin/integration`, the same diff the PR's mutation job tests.
- `task release:hypervisor:record` (Apple silicon, with `LEYLINE_LIBKRUNFW_PATH` and `LEYLINE_GUEST_TOOLS`) builds in `rs/target-hypervisor`, so it runs beside `task ci` without sharing a cargo target directory.
- `git diff --check` for every change.

Report what was run, what was not run, and why. A passing producer test is not evidence that downstream consumers still agree.

## Findings and Handoff

Review findings must include severity (`BLOCKER`, `COMMENT`, or `NOTE`), an exact location, the violated invariant, the failure mechanism, evidence, a closing test, and confidence. Separate shipped behavior from proposals and inference.

Before handing work off, update the bead with the files changed, verification evidence, and remaining risks. Do not claim completion from prose review alone when an executable closing test is possible.
