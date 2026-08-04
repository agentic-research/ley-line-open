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
SCOPE=${2:-all}
test -f "$DIFF" || { echo "DIFF file not found: $DIFF" >&2; exit 1; }
case "$SCOPE" in
    all|runtime|cli) ;;
    *) echo "unknown mutation scope: $SCOPE (expected all, runtime, or cli)" >&2; exit 1 ;;
esac

# Decided from the diff itself, not from cargo-mutants' output, so "did this
# change Rust?" and "did enumeration work?" stay independent questions. Folding
# them together is what makes the misconfigured case look like the skipped one.
if ! grep -qE '^--- a/.*\.rs$|^\+\+\+ b/.*\.rs$' "$DIFF"; then
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

if [ "$SCOPE" != all ]; then
    case "$SCOPE" in
        runtime) scope_prefix='ll-open/runtime' ;;
        cli) scope_prefix='ll-open/cli-lib' ;;
    esac
    scope_n=$(printf '%s\n' "$listing" | grep -cE "^$scope_prefix/.*:[0-9]+:[0-9]+:" || true)
    if [ "${scope_n:-0}" -eq 0 ]; then
        {
            echo "MISCONFIGURED: mutation scope '$SCOPE' enumerated zero candidates"
            echo "               from that package. Mutants in another changed package"
            echo "               cannot make this focused run pass."
        } >&2
        exit 1
    fi
fi

echo "mutating $n candidate(s) from the diff"

# The generic slice uses `-C --lib`: cargo-mutants builds in a scratch copy, so
# any test reading repo files outside its own crate fails there and takes the
# whole run with it. mcp-descriptor's schema_conformance does exactly that
# (vendored schema, committed server.json, `git ls-files`), and it is right to —
# those assertions are about the repo, not the crate.
#
# Runtime and CLI-library behavior is intentionally tested through public
# integration tests, however. Running those packages through the generic slice
# made every forwarding and lifecycle mutant survive without executing its
# actual contract tests. They therefore get package-specific slices without
# `--lib`; the generic slice excludes them so each mutant is evaluated once.
#
# `-E 'replace main'`: a binary's `main` is killed by subprocess tests
# (mcp-descriptor's tests/cli.rs asserts exit codes and stdout), which `-C --lib`
# excludes by construction. Reporting it missed is TRUE but would fail every PR
# touching any main.rs, and a gate people learn to ignore has already failed.
#
# Exit 4 is not "a mutant survived" — it is "the baseline suite failed in the
# scratch tree", so nothing was tested. They need opposite fixes: one is a
# missing test, the other is a test that cannot run here.
# Packages the generic slice excludes. Each MUST be claimed by a slice of its
# own below. A package that is excluded here and covered nowhere would let a
# diff touching only that package enumerate mutants (so the MISCONFIGURED
# check above passes), run the generic slice that skips it, match no other
# slice, and exit 0 having evaluated nothing — the exact false green this
# script exists to prevent. `assert_excluded_packages_were_covered` enforces
# the pairing instead of trusting whoever edits the exclude list next.
# Overridable ONLY so tools/test_mutants_diff.sh can prove the pairing check
# actually fires; production callers must never set it.
GENERIC_EXCLUDES=${MUTANTS_GENERIC_EXCLUDES:-'ll-open/fs ll-open/runtime ll-open/cli-lib ll-open/cli ll-open/schema-bridge'}

overall=0
ran_slice=0
covered_packages=''
claim_package() {
    covered_packages="$covered_packages $1"
}

package_changed_in_diff() {
    grep -qE "^--- a/$1/.*\.rs\$|^\+\+\+ b/$1/.*\.rs\$" "$DIFF"
}

assert_excluded_packages_were_covered() {
    for package in $GENERIC_EXCLUDES; do
        package_changed_in_diff "$package" || continue
        case " $covered_packages " in
            *" $package "*) continue ;;
        esac
        {
            echo "MISCONFIGURED: the diff changes $package, which the generic"
            echo "               slice excludes and no package slice covers."
            echo "               Nothing mutated it. Add a slice for that"
            echo "               package or stop excluding it — do not let this"
            echo "               exit 0."
        } >&2
        exit 1
    done
}

run_slice() {
    mode=$1
    shift
    ran_slice=1
    set +e
    if [ "$mode" = lib ]; then
        cargo mutants --in-diff "$DIFF" -C --lib -E 'replace main' "$@"
    else
        cargo mutants --in-diff "$DIFF" -E 'replace main' "$@"
    fi
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
    # cargo-mutants returns exit 3 whenever any mutant times out, even when
    # other mutants also survived. Its report is therefore authoritative over
    # that ambiguous exit code: a non-empty missed.txt must fail the gate.
    if [ -s mutants.out/missed.txt ]; then
        missed=$(wc -l < mutants.out/missed.txt | tr -d ' ')
        echo "MISSED: $missed surviving mutant(s) in this slice" >&2
        overall=2
        return 0
    fi
    # Exit 3 with an empty survivor report means timeouts only. The allowlist
    # tasks' stance applies here too: a mutant the harness had to STOP is
    # detected, not missed.
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
if [ "$SCOPE" = all ]; then
    run_slice lib \
        --exclude 'll-open/fs/**' \
        --exclude 'll-open/runtime/**' \
        --exclude 'll-open/cli-lib/**' \
        --exclude 'll-open/cli/**' \
        --exclude 'll-open/schema-bridge/**'
fi

# schema-bridge is the same case runtime and cli-lib are, in its purest form:
# it has no `#[cfg(test)]` module ANYWHERE. Every assertion about the emitters
# lives in `tests/` — `doc_projection.rs`, `execution_v1.rs`, `map_field.rs`
# and the rest — so the lib-only generic slice ran zero of them and reported
# every emitter mutant as missed. It failed this PR for 7 of them, `write_doc`
# among them, each one killed outright by `tests/doc_projection.rs`.
#
# `--test-workspace=false` keeps that from costing anything: the crate's own
# suite is what runs, so no other crate's tests can break this baseline.
if [ "$SCOPE" = all ] && package_changed_in_diff ll-open/schema-bridge; then
    claim_package ll-open/schema-bridge
    run_slice integration --package leyline-schema-bridge --test-workspace=false
fi

if [ "$SCOPE" = all ] || [ "$SCOPE" = runtime ]; then
  if package_changed_in_diff ll-open/runtime; then
    claim_package ll-open/runtime
    run_slice integration --package leyline-runtime --test-workspace=false
  fi
fi

if [ "$SCOPE" = all ] || [ "$SCOPE" = cli ]; then
  if package_changed_in_diff ll-open/cli-lib; then
    claim_package ll-open/cli-lib
    run_slice integration \
        -C --lib \
        -C --test -C execution_client \
        -C --test -C execution_transport \
        --package leyline-cli-lib --test-workspace=false
  fi
fi

if [ "$SCOPE" = all ] && package_changed_in_diff ll-open/fs; then
    claim_package ll-open/fs
    run_slice lib --package leyline-fs --test-workspace=false \
        --no-default-features --features cdc,splice,validate
fi

# `ll-open/cli` is the thin clap binary: its only mutant is `replace main`,
# which `-E` excludes because subprocess tests, not lib tests, kill it. It is
# claimed here so the pairing check stays honest — the moment that crate grows
# a mutable helper, the claim is a lie and the exclude list needs a real slice.
if package_changed_in_diff ll-open/cli; then
    claim_package ll-open/cli
    echo "NOTE: ll-open/cli carries only 'replace main', which -E excludes;"
    echo "      its behavior is gated by leyline-cli-lib's slice."
fi

if [ "$SCOPE" = all ]; then
    assert_excluded_packages_were_covered
fi

if [ "$SCOPE" != all ] && [ "$ran_slice" -eq 0 ]; then
    {
        echo "MISCONFIGURED: mutation scope '$SCOPE' matched no changed Rust package"
        echo "               and therefore ran no mutation slice. This is not a pass."
    } >&2
    exit 1
fi

exit "$overall"
