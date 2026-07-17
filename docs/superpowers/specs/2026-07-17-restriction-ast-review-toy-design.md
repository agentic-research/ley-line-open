# Restriction AST Review Toy Design

## Goal

Build a small falsification harness for restriction-addressed caching over CAS:
given code snippets, derive a toy AST summary, derive review facts from that
summary, and compare cache policies by whether they skip or recompute review
facts soundly.

## Scope

This is a toy measurement harness, not daemon integration. It lives in
`leyline-sheaf` because the question is about sheaf-shaped cache invalidation
and restriction maps, but it does not alter `SheafCache` behavior.

The harness models these cache keys:

- `object_hash`: whole source identity.
- `ast_shape_hash`: structure-only AST shape, intentionally identifier-blind.
- `review_restriction_hash`: the fact-specific observable boundary used by a
  review fact.

The harness models these review facts:

- `UsesUnwrap`
- `PublicSignatureChanged`
- `CallTargetChanged`
- `ImportSurfaceChanged`
- `BranchConditionChanged`

## Architecture

`rs/ll-open/sheaf/src/restriction_review.rs` exposes a focused toy API:

- parse a Rust-like snippet into a deterministic `ToyAst`.
- derive `ReviewFacts`.
- compute `CacheKeys`.
- compare two snippets with `compare_review_cache`.

`rs/ll-open/sheaf/tests/restriction_review_toy.rs` drives the falsification
cases. The test output is the artifact: a table of edit scenarios showing which
policies would skip, whether the review facts changed, and whether a skip would
be false.

## Invariants

- Whitespace/comment-only edits should change the whole-object hash but leave
  review restrictions stable and safely skippable.
- Identifier-blind AST shape should falsely skip pure callee swaps and `unwrap`
  introductions.
- Fact-specific review restrictions should not falsely skip any fixture where
  review facts change.
- No approximate embedding threshold participates in the decision.

## Non-Goals

- No tree-sitter integration.
- No daemon op.
- No production cache eviction change.
- No claim that the toy parser is a real Rust parser.
