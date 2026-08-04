# Ley Line Open Agent Instructions

This file is the repository-local operating contract for agents working in LLO. It complements the global agent rules and the canonical project documentation; it does not replace either one.

## Start Here

For non-trivial work:

1. Use `rsry` to search for an existing bead before changing code. Create one when no suitable bead exists, record meaningful progress, and close it only when its acceptance criteria are verified. Beads, not GitHub issues or ad hoc investigation logs, are the work ledger.
2. Use `mache.get_overview` before exploring unfamiliar code, then use its symbol and community tools to narrow the relevant seam. If mache is unavailable, record the tooling failure in its existing bead before falling back to targeted repository reads.
3. Use `lectio` when available for repository history and observational context. Tooling absence must not be mistaken for evidence about the code.
4. Read the governing material in this order: `README.md`, `docs/TECHNICAL_OVERVIEW.md`, `docs/ARCHITECTURE.md`, `docs/TABLE_CONTRACT.md`, then the relevant ADR, specification, crate README, and tests.

Treat a bead as a pointer, not as proof. Verify every claim against the current source, generated surfaces, and tests.

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

| Name | Authoritative for | Construction | Status |
| --- | --- | --- | --- |
| `Controller.current_root` | One exact serialized SQLite arena snapshot | BLAKE3 over the complete serialized byte image | Shipped |
| `Head.rootHash` | The Cap'n Proto segments produced by one parse run | Tagged fold over canonical segment addresses | Shipped |
| Blob hash | One content-addressed blob or CDC payload | BLAKE3 over that payload | Shipped |
| SQL projection ABI | Consumer-visible relational shape and behavior | Tables, columns, constraints, semantics, fixtures | Shipped; not an identity domain |
| `manifestRoot` | CDC transport/dedup structure | Defined by ADR-0032 | Proposed only |
| `logicalRoot` | Derived-view validity | Defined by ADR-0032 | Proposed only |

`current_root`, `Head.rootHash`, and blob hashes do not name the same bytes and are not interchangeable. Proposed identities must not appear in shipped APIs, documentation, or tests as though they already govern authority.

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

For concurrent implementation work, make bead file scopes explicit and use separate worktrees. Do not assign overlapping edits to the same schema, generated artifact, compatibility manifest, task definition, or architecture document. Read-only reviews are safe to parallelize.

## Verification

Run the narrowest test that proves the changed contract, then the relevant repository gates. Common gates include:

- `task runtime:test` for embedded runtime and execution contracts.
- `task compat:check`, `task gen:server-json:check`, and `task schema:consumer-fixture-test` for wire/schema changes.
- `task check` for the standard repository checks.
- `task smells` when architecture or dependency shape changes.
- `task ci` when the change warrants the full CI-equivalent surface.
- `git diff --check` for every change.

Report what was run, what was not run, and why. A passing producer test is not evidence that downstream consumers still agree.

## Findings and Handoff

Review findings must include severity (`BLOCKER`, `COMMENT`, or `NOTE`), an exact location, the violated invariant, the failure mechanism, evidence, a closing test, and confidence. Separate shipped behavior from proposals and inference.

Before handing work off, update the bead with the files changed, verification evidence, and remaining risks. Do not claim completion from prose review alone when an executable closing test is possible.
