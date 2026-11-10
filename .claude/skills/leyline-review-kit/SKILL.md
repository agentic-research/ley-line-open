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

Keep these claims separate throughout the review. `AGENTS.md` "Identity and
Authority Vocabulary" gives each row's exact construction and code location;
ADR-0047 §3 is the conformance table for the constructions it lists.

| Surface | Authority | Status |
|---|---|---|
| `Controller.current_root` | BLAKE3 of one serialized SQLite arena byte image | Shipped |
| `Head.rootHash` | Keyed fold `leyline/segment-root/v1` over one parse run's canonical Cap'n Proto segments; a run receipt | Shipped |
| `Head.storeRoot` | Parser-free directory Merkle root `leyline/store-tree/v1` over `_tree` (path, git mode, blob hash) | Shipped (ADR-0047 M1); the retired `treeRoot` refuses a head |
| `Head.parserId` | Keyed fold `leyline/parser-id/v1` over schema versions, epochs and grammar digests; bound beside `storeRoot`, never inside it | Shipped |
| head digest | `derive_key("leyline head digest v3", …)` over generation, `rootHash`, `parentHash`, `storeRoot`, `parserId`, parent digest | Shipped (v3); v1 and v2 removed |
| `node_hash` | `derive_key("leyline/ast-node/v3", …)` over one syntax subtree (κ kind for a named node, grammar kind for an anonymous token; merkle-ast-v3); location-free | Shipped (ADR-0047 M2) |
| blob hash | Plain BLAKE3 (σ_bytes) of one CAS/CDC payload; σ_bytes is the one unkeyed construction (also `current_root` and the rootfs digest) | Shipped |
| `run_id` | `run-` ‖ BLAKE3(`cloister/execution/v1/run-id` ‖ 0x00 ‖ length-prefixed spec digest, grant id, replay key) | Shipped (v1); keyed v2 is ADR-0047 M3, proposed |
| rootfs digest | BLAKE3 of the exact `rootfs.manifest.json` bytes; the CAS directory name and `RunReceipt.inputRoots[0]` | Shipped |
| locators (`content_id`, `nid`, `file_id`, `dir_id`, `name_id`, `kind_id`) | Arena-local row keys; not an identity, never on the wire, in a signed claim or in a cache key | Shipped (locator) |
| SQL projection ABI | Queryable shape (`projection-v13`); a contract, not an identity domain | Shipped |
| blob manifest address, `manifestRoot`, `logicalRoot` | Proposed only (ADR-0047 M4, ADR-0032); never describe as shipped | Proposed |

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
