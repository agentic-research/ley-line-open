---
name: leyline-review
description: Use when reviewing a ley-line-open PR, bead, diff, path, or architectural claim that may cross identity, snapshot publication, CDC, schema, wire compatibility, confinement, or execution/v1 seams.
argument-hint: "[PR|bead|path|ref-range] [--lens identity|publication|wire|execution|all]"
allowed-tools: "Read,Grep,Glob,Bash,Agent,mcp__mache__*,mcp__rsry__*"
---

# Leyline Review

## Purpose

Orchestrate the smallest set of independent LLO reviewers that covers the
declared change, then synthesize their evidence at the seams. Keep the review
read-only: do not edit source, post to GitHub, or close findings without explicit
authorization.

**REQUIRED SUB-SKILL:** Apply `leyline-review-kit` to context gathering,
finding shape, severity, and final verification.

## Resolve the target

Interpret `$ARGUMENTS` as a PR, bead, repository path, git ref range, or explicit
claim. Resolve an omitted target from the current branch or active bead. If more
than one target remains plausible, ask for confirmation before dispatching.

Build one context packet containing:

- exact target and changed files
- target bead/PR intent and acceptance criteria
- canonical contract paths selected by `leyline-review-kit`
- pre-existing dirty files that reviewers must not attribute to the target
- allowed read-only verification commands

## Select lenses

Use `--lens` when provided. Otherwise select by changed surface:

| Lens | Dispatch | Signals |
|---|---|---|
| identity | `identity-domain-auditor` | roots, hashes, `ContentAddressed`, authority, attestation, SQL identity claims |
| publication | `publication-state-adversary` | arena, snapshot, controller, CDC, manifest, GC, FUSE/NFS, hot-swap |
| wire | `cross-runtime-contract-auditor` | Cap'n Proto, daemon JSON, schema generation, fixtures, Go/TS consumers, compatibility |
| execution | `execution-boundary-adversary` | execution/v1, confinement, catalog, evidence, grants, backend, worker, cleanup |

Dispatch every selected named agent in parallel because all four are read-only.
Pass the same context packet plus the lens-specific changed files. Do not dispatch
`all` merely because the repository is complex; use all four only when the
change crosses all four seams or the user explicitly requests a full review.

If a named agent is unavailable, run that lens serially using its repository
definition as the contract and disclose the fallback.

## Synthesize

1. Spot-check every `BLOCKER` against the current target and primary source.
2. Deduplicate symptoms that share one violated invariant.
3. Build a seam matrix mapping contract → producer → persistence → verifier →
   consumer → test. Mark uncovered cells explicitly.
4. Resolve disagreements by authority order: normative schema/ADR and shipped
   code, then independent fixtures/tests, then prose and inference.
5. Separate target-introduced findings, pre-existing observations, and residual
   unverified risks.

Return findings first, ordered by severity, followed by the seam matrix,
reviewed inventory, commands run, disagreements, and a plain verdict. A clean
review must still show which negative, concurrency, restart, and cross-runtime
dimensions were verified or left open.
