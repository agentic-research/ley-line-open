---
name: leyline-review-kit
description: Use when reviewing ley-line-open changes that cross an identity, authority, snapshot publication, CDC, wire compatibility, schema generation, confinement, or execution/v1 boundary.
user-invocable: false
allowed-tools: "Read,Grep,Glob,Bash,mcp__mache__*,mcp__rsry__*"
---

# Leyline Review Kit

## Core contract

Review one declared LLO seam from its producer through storage, verification,
publication, and every in-scope consumer. Treat similarly shaped hashes,
schemas, tables, and receipts as distinct until a canonical source proves an
equivalence.

Remain read-only. Report findings and file beads when authorized; do not patch
the reviewed implementation.

## Bootstrap

1. Search `rsry` for the target bead and related findings. Create no duplicate.
2. Call mache `get_overview` before exploring unfamiliar code. If mache cannot
   resolve the workspace, record the exact failure on an existing tooling bead
   and fall back to narrow read-only searches.
3. Read `docs/ARCHITECTURE.md` and `docs/TECHNICAL_OVERVIEW.md` before treating
   README prose, generated code, or a test name as authoritative.
4. Read the lens-specific contract:
   - identity: `docs/adr/0032-declared-decompositions.md` and
     `docs/TABLE_CONTRACT.md`
   - publication: `docs/TABLE_CONTRACT.md`, the relevant CDC/snapshot design,
     and the owning crate README
   - wire: schema README, normative schema, generator, committed consumer, and
     compatibility fixture
   - execution: `rs/ll-core/schema-spec/execution/v1/README.md`,
     `docs/superpowers/specs/2026-07-31-execution-v1-design.md`, and
     `rs/ll-open/runtime/`

## Authority ledger

Keep these claims separate throughout the review:

| Surface | Authority |
|---|---|
| `Controller.current_root` | BLAKE3 of one serialized SQLite arena byte image |
| `Head.rootHash` | Tagged fold over canonical Cap'n Proto parse segments |
| blob hash | Identity of one CAS/CDC payload |
| SQL projection ABI | Queryable shape; a contract, not an identity domain |
| `manifestRoot`, `logicalRoot` | Proposed only; never describe as shipped |

Never infer one row from another. Require an explicit co-attestation or mapping
when two authorities must travel together.

## Review procedure

1. Pin the exact diff, bead, PR, path, or claim under review.
2. Inventory every contract, producer, persistence boundary, verifier,
   generated artifact, consumer, and test examined.
3. Trace the seam end to end. Identify the first point at which an invalid,
   stale, partial, reordered, or cross-domain value could be accepted.
4. Check negative, fault-injection, concurrency, restart, and cross-runtime
   evidence where the seam makes those dimensions relevant.
5. Run the narrowest owning tests first. Use `task check` or `task ci` only when
   the blast radius justifies the full repository gate.
6. Separate changed-line findings, pre-existing observations, proposed-only
   behavior, and verified-safe mechanisms.

## Finding contract

For each finding provide:

- **Severity:** `BLOCKER`, `COMMENT`, or `NOTE`
- **Location:** repository-relative `file:line`
- **Violated authority or invariant**
- **Reachable failure:** concrete input, ordering, or state transition
- **Evidence:** primary source plus command or test result
- **Closing test:** the smallest falsifiable regression or conformance gate
- **Confidence:** high, medium, or low

End with the reviewed inventory, commands run, residual unverified risks, and a
plain verdict. “No findings” is valid when the inventory demonstrates coverage.
