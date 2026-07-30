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
    {
        echo "MISCONFIGURED: the diff changes Rust source but enumerated zero"
        echo "               mutants. Almost always the diff's paths are not"
        echo "               workspace-relative — cargo mutants runs in rs/ and"
        echo "               needs 'a/ll-core/...', not 'a/rs/ll-core/...'."
        echo "               Regenerate with: git diff --relative=rs"
        echo "--- cargo mutants said ---"
        printf '%s\n' "$listing"
    } >&2
    exit 1
fi

echo "mutating $n candidate(s) from the diff"

# `-C --lib`: same reason mutants:fs carries it. cargo-mutants builds in a
# scratch copy, so any test reading repo files outside its own crate fails there
# and takes the whole run with it. mcp-descriptor's schema_conformance does
# exactly that (vendored schema, committed server.json, `git ls-files`), and it
# is right to — those assertions are about the repo, not the crate.
#
# LIMITATION, stated rather than hidden: diff-scoped mutation therefore covers
# LIB tests only. A mutant whose only killer is an integration test will survive
# and be reported as missed. Read that as "the lib tests do not pin this", not
# as proof nothing does.
#
# `-E 'replace main'`: a binary's `main` is killed by subprocess tests
# (mcp-descriptor's tests/cli.rs asserts exit codes and stdout), which `-C --lib`
# excludes by construction. Reporting it missed is TRUE but would fail every PR
# touching any main.rs, and a gate people learn to ignore has already failed.
#
# Exit 4 is not "a mutant survived" — it is "the baseline suite failed in the
# scratch tree", so nothing was tested. They need opposite fixes: one is a
# missing test, the other is a test that cannot run here.
overall=0
run_slice() {
    set +e
    cargo mutants --in-diff "$DIFF" -C --lib -E 'replace main' "$@"
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
run_slice --exclude 'll-open/fs/**'
if grep -qE '^\+\+\+ b/ll-open/fs/.*\.rs$' "$DIFF"; then
    run_slice --package leyline-fs --test-workspace=false \
        --no-default-features --features cdc,splice,validate
fi

exit "$overall"
