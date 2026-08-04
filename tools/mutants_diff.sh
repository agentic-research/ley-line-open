#!/bin/sh
# Mutation-test only the lines a diff touches (`ley-line-open-085dff`).
#
# Run from rs/ (the Taskfile sets `dir: rs`). Usage: mutants_diff.sh <diff-file>
#
# FOUR outcomes, deliberately distinguished. Three of them exit 0 in a naive
# implementation and are indistinguishable from success:
#
#   tested N        mutants ran; survivors fail the gate.
#   SKIPPED         the diff changes no Rust. Nothing ran — reported as
#                   skipped, never as a pass.
#   MISCONFIGURED   the diff DOES change Rust but enumerated zero mutants.
#                   Hard failure.
#   BASELINE BROKEN the unmutated tree failed its own tests in the scratch
#                   build, so nothing was mutated at all. Not a survivor.
#
# MISCONFIGURED is the trap this exists for. cargo-mutants runs in `rs/`, so the
# diff's paths must be workspace-relative (`a/ll-core/...`). A diff produced
# from the repo root carries `a/rs/ll-core/...`, matches nothing, prints
# "No mutants to filter", and EXITS 0 — a permanently green check that tests
# nothing, from a path prefix. Generate with `git diff --relative=rs`.
set -eu

DIFF=${1:?usage: mutants_diff.sh <diff-file>}
test -f "$DIFF" || { echo "DIFF file not found: $DIFF" >&2; exit 1; }

# Decided from the diff itself, not from cargo-mutants' output, so "did this
# change Rust?" and "did enumeration work?" stay independent questions. Folding
# them together is what makes the misconfigured case look like the skipped one.
if ! grep -qE '^\+\+\+ b/.*\.rs$' "$DIFF"; then
    echo "SKIPPED: this diff changes no Rust source, so there is nothing to"
    echo "         mutate. This is NOT a pass — no mutation testing ran."
    exit 0
fi

listing=$(cargo mutants --in-diff "$DIFF" --list 2>&1 || true)
n=$(printf '%s\n' "$listing" | grep -cE '^[^ ].*:[0-9]+:[0-9]+:' || true)

if [ "${n:-0}" -eq 0 ]; then
    # Zero mutants from a Rust-touching diff has two causes that need opposite
    # responses, and the difference is not visible in cargo-mutants' output —
    # it prints "No mutants to filter" for both.
    #
    #   1. The paths do not resolve. This is the trap: a diff generated from
    #      the repo root carries `b/rs/ll-core/...`, matches nothing, and the
    #      naive implementation exits 0 forever.
    #   2. The paths resolve fine and the changed lines simply hold nothing
    #      mutable — a `#[cfg(test)] mod tests` edit, a doc comment, a `use`.
    #      Nothing to mutate is the correct answer here, not a failure.
    #
    # Deciding between them by asking the filesystem tests the actual claim the
    # error message makes ("your paths are not workspace-relative") rather than
    # a proxy for it. This script's cwd IS the cargo-mutants working directory,
    # so a workspace-relative path is exactly one that resolves from here.
    changed=$(sed -n 's|^+++ b/\(.*\.rs\)$|\1|p' "$DIFF")
    resolved=0
    unresolved=""
    # Here-doc rather than a pipe: a pipeline would run this loop in a subshell
    # and the counters would not survive it. Read line-wise, not word-wise, so
    # a path containing a space is still one path.
    while IFS= read -r path; do
        [ -n "$path" ] || continue
        if [ -f "$path" ]; then
            resolved=$((resolved + 1))
        else
            unresolved="${unresolved}${path}
"
        fi
    done <<EOF
$changed
EOF

    if [ "$resolved" -eq 0 ]; then
        {
            echo "MISCONFIGURED: the diff changes Rust source but enumerated zero"
            echo "               mutants, and none of its paths resolve from"
            echo "               $(pwd) — where cargo mutants runs."
            echo "               Paths must be workspace-relative: 'b/ll-core/...',"
            echo "               not 'b/rs/ll-core/...'."
            echo "               Regenerate with: git diff --relative=rs"
            echo "--- unresolved paths ---"
            printf '%s' "$unresolved" | sed 's/^/    /'
            echo "--- cargo mutants said ---"
            printf '%s\n' "$listing"
        } >&2
        exit 1
    fi

    echo "NO MUTABLE LINES: the diff's $resolved Rust file(s) resolve, but the"
    echo "                  lines it changes hold nothing cargo-mutants can"
    echo "                  mutate — test modules, comments or imports. This is"
    echo "                  NOT a pass; no mutation testing ran."
    exit 0
fi

echo "mutating $n candidate(s) from the diff"

# `--test-workspace=false`: each mutant is tested by its OWN crate's suite,
# lib and integration alike, rather than by the whole workspace's.
#
# This replaces a blanket `-C --lib` (`ley-line-open-7675fe`). The problem
# `-C --lib` solved is real: cargo-mutants builds in a scratch copy, and a test
# reading repo files outside its crate fails there and takes the run with it.
# But it solved that by never running ANY integration test, which made the gate
# structurally blind to crates whose assertions live in `tests/` — and, worse,
# made it report them as failures. schema-bridge has no `#[cfg(test)]` module at
# all, so the gate called all 7 of its mutants MISSED while
# `tests/doc_projection.rs` kills every one of them.
#
# Measured per crate, in the scratch tree, own-tests-only:
#
#   leyline-mcp-descriptor   baseline FAILED (exit 4)  -> needs `-C --lib`
#   leyline-fs               baseline FAILED (exit 4)  -> needs `-C --lib`
#   leyline-schema-bridge    ok — 7/7 caught
#   leyline-schema-capnp     ok
#
# So the workaround is scoped to the two crates that measurably need it instead
# of taxing every crate that does not. Per-crate testing is also what keeps a
# broken baseline local: under `--test-workspace=true` mcp-descriptor's tests
# run for every mutant in the workspace, so one unrunnable crate breaks the
# baseline for all of them.
#
# NARROWER IN ONE DIRECTION, stated rather than hidden: a mutant whose only
# killer lives in a DIFFERENT crate's tests is now reported missed. That is a
# real trade for no longer missing every integration test in the mutant's own
# crate, which is where this repo keeps most of its assertions.
#
# `-E 'replace main'`: a binary's `main` is killed by subprocess tests
# (mcp-descriptor's tests/cli.rs asserts exit codes and stdout), which the
# `-C --lib` slices exclude by construction. Reporting it missed is TRUE but
# would fail every PR touching any main.rs, and a gate people learn to ignore
# has already failed.
#
# Exit 4 is not "a mutant survived" — it is "the baseline suite failed in the
# scratch tree", so nothing was tested. They need opposite fixes: one is a
# missing test, the other is a test that cannot run here.
overall=0
run_slice() {
    set +e
    cargo mutants --in-diff "$DIFF" -E 'replace main' --test-workspace=false "$@"
    rc=$?
    set -e
    if [ "$rc" -eq 4 ]; then
        {
            echo "BASELINE BROKEN: the unmutated tree failed its own tests in the"
            echo "                 mutants scratch build, so NOTHING was mutated."
            echo "                 This is not a surviving mutant. Usually a test"
            echo "                 reading files outside its crate."
        } >&2
        exit 4
    fi
    # Exit 3 = timeouts only — the allowlist tasks' stance applies here too:
    # a mutant the harness had to STOP is detected, not missed. Exit 2
    # (genuinely missed mutants) is what fails the gate.
    if [ "$rc" -eq 3 ]; then
        echo "TIMEOUT(s) only in this slice — stopped mutants count as detected"
        return 0
    fi
    if [ "$rc" -ne 0 ]; then
        overall=$rc
    fi
}

# Feature-gated code is INVISIBLE to a default-features run: a mutant inside
# `#[cfg(feature = "cdc")]` builds in 0s (the mutated code never compiles),
# every test trivially passes, and it is reported MISSED — a false survivor
# on healthy code. This gate failed PR #306 with 24 such phantoms while the
# feature-correct allowlist run caught all 86 real ones in the same file.
# leyline-fs's covered modules (chunked.rs, gc.rs) are exactly that shape, so
# fs files route to their own invocation carrying the same feature set as
# `mutants:fs`/`mutants:fs-gc`. If another crate ever hides covered code
# behind non-default features, it needs the same routing — a default-features
# run structurally cannot test it.
run_slice --exclude 'll-open/fs/**' --exclude 'll-core/mcp-descriptor/**'
if grep -qE '^\+\+\+ b/ll-open/fs/.*\.rs$' "$DIFF"; then
    run_slice --package leyline-fs -C --lib \
        --no-default-features --features cdc,splice,validate
fi
# Its `schema_conformance` reads the vendored schema, the committed server.json
# and `git ls-files` — assertions about the repo, not the crate, and right to
# be. They cannot run in a scratch copy with no `.git`, so this crate is the
# one that keeps the lib-only restriction.
if grep -qE '^\+\+\+ b/ll-core/mcp-descriptor/.*\.rs$' "$DIFF"; then
    run_slice --package leyline-mcp-descriptor -C --lib
fi

exit "$overall"
