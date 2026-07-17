# Restriction AST Review Toy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a small falsification harness showing when fact-specific AST restriction hashes outperform whole-object and structure-only cache keys for review facts.

**Architecture:** Add one toy module to `leyline-sheaf` and one integration test. The module parses a constrained Rust-like syntax into summary fields, computes hashes for whole object, AST shape, and review restrictions, and reports skip/false-skip outcomes for cache policies.

**Tech Stack:** Rust 2024, `sha2` already present in `leyline-sheaf`, cargo integration tests.

## Global Constraints

- Keep the harness toy-scoped; do not wire daemon behavior.
- Do not add production dependencies.
- Use deterministic canonical strings before hashing.
- Acceptance command: `cargo test -p leyline-sheaf --test restriction_review_toy`.

---

### Task 1: Red Test for Review Restriction Policies

**Files:**
- Create: `rs/ll-open/sheaf/tests/restriction_review_toy.rs`
- Modify: `rs/ll-open/sheaf/src/lib.rs`
- Create: `rs/ll-open/sheaf/src/restriction_review.rs`

**Interfaces:**
- Consumes: none.
- Produces: `leyline_sheaf::restriction_review::{compare_review_cache, CachePolicy, ReviewFactKind}`.

- [x] **Step 1: Write the failing test**

Create `rs/ll-open/sheaf/tests/restriction_review_toy.rs` with fixtures for whitespace, callee swap, unwrap introduction, public signature change, import change, and branch condition change. The tests should assert that the structure-only policy has false skips on identifier-sensitive edits and that the review-restriction policy has zero false skips on the fixtures.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p leyline-sheaf --test restriction_review_toy`

Expected: FAIL because `leyline_sheaf::restriction_review` is not exported yet.

- [x] **Step 3: Write minimal implementation**

Add `pub mod restriction_review;` to `rs/ll-open/sheaf/src/lib.rs`.

Create `rs/ll-open/sheaf/src/restriction_review.rs` with:

- `ReviewFactKind`
- `CachePolicy`
- `PolicyOutcome`
- `ScenarioReport`
- `compare_review_cache(before: &str, after: &str) -> ScenarioReport`

The module should canonicalize toy AST fields and compute SHA-256 hashes.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p leyline-sheaf --test restriction_review_toy`

Expected: PASS.

- [x] **Step 5: Commit**

Commit message:

```bash
[restriction-ast-review-toy-9f0af5] test(sheaf): add restriction-addressed review cache toy
```

## Self-Review

- Spec coverage: the plan covers the toy parser, cache key comparison, and falsification fixtures.
- Placeholder scan: no placeholder requirements remain.
- Type consistency: the test imports match the planned public module and function names.
