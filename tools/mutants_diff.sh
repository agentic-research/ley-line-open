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
#
# Environment:
#
#   MUTANTS_SHARD=k/n        run one of n shards of every slice.
#   MUTANTS_BASELINE=record  run each slice's unmutated baseline ONCE and write
#                            its result to $MUTANTS_BASELINE_RECORD.
#   MUTANTS_BASELINE=reuse   run the mutants against that record: no baseline,
#                            an explicit timeout derived from the recorded one.
#                            The multi-package generic remainder is exempt
#                            from both and always runs its own baseline.
#
# Unset, every slice runs its own baseline, which is what a local run wants.
# The baseline block below says why.
set -eu

# cargo-mutants builds a copy of rs/ outside the repository, where the
# repository-root .cargo/config.toml is not on cargo's config search path. That
# file points libkrun's guest-init compiler (CC_LINUX) at tools/krun_init_cc.sh;
# without it the vendored libkrun cannot build off Linux. Carry it as an
# absolute path; an explicit CC_LINUX still wins (ley-line-open-f1c46f).
: "${CC_LINUX:=$(CDPATH='' cd -P -- "$(dirname "$0")" && pwd -P)/krun_init_cc.sh}"
export CC_LINUX
# Same reason for the worker signing script the runtime and krun-worker tests
# run (rs/ll-open/krun-worker/tests/hypervisor/mod.rs, ley-line-open-984acf):
# it lives in tools/, outside the copy.
: "${LEYLINE_SIGN_KRUN_WORKER:=$(CDPATH='' cd -P -- "$(dirname "$0")" && pwd -P)/sign_krun_worker.sh}"
export LEYLINE_SIGN_KRUN_WORKER

DIFF=${1:?usage: mutants_diff.sh <diff-file>}
SCOPE=${2:-all}
test -f "$DIFF" || { echo "DIFF file not found: $DIFF" >&2; exit 1; }

# THE record of which mutation scope owns which workspace package.
#
# One table, because this fact previously lived in four places that could drift
# apart: the generic slice's exclude list, the scope->prefix map inside the
# enumeration check, the per-slice `[ "$SCOPE" = ... ]` guards, and (once the
# matrix arrived) a planner deciding which scopes to emit. A prototype planner
# built against a hand-copied fifth list PASSED a case it should have failed —
# a package excluded from the generic slice and owned by no scope — which is the
# same drift class `ley-line-open-cb1e29` collapsed the feature ledger to fix.
#
# Format: `<scope>=<package>[,<package>...]`. Everything below derives from it.
MUTATION_SCOPE_PACKAGES='fs=ll-open/fs runtime=ll-open/runtime,ll-open/krun-worker cli=ll-open/cli-lib,ll-open/cli schema-bridge=ll-open/schema-bridge schema-spec=ll-core/schema-spec storefs=ll-open/storefs'

# Every package owned by SOME package slice — i.e. exactly what the generic
# slice must exclude. Each MUST be claimed by a slice of its own below. A
# package excluded here and covered nowhere would let a diff touching only that
# package enumerate mutants (so the MISCONFIGURED check above passes), run the
# generic slice that skips it, match no other slice, and exit 0 having evaluated
# nothing — the exact false green this script exists to prevent.
# Overridable ONLY so tools/test_mutants_diff.sh can prove the pairing check
# actually fires; production callers must never set it.
derive_generic_excludes() {
    for entry in $MUTATION_SCOPE_PACKAGES; do
        printf '%s ' "${entry#*=}" | tr ',' ' '
    done
}
GENERIC_EXCLUDES=${MUTANTS_GENERIC_EXCLUDES:-$(derive_generic_excludes)}

# Which scope owns a package, or empty if none does.
scope_owning_package() {
    for entry in $MUTATION_SCOPE_PACKAGES; do
        for owned in $(printf '%s' "${entry#*=}" | tr ',' ' '); do
            if [ "$owned" = "$1" ]; then
                printf '%s' "${entry%%=*}"
                return 0
            fi
        done
    done
    return 1
}

# The packages a scope owns, or empty for `generic` (which owns the complement).
packages_in_scope() {
    for entry in $MUTATION_SCOPE_PACKAGES; do
        if [ "${entry%%=*}" = "$1" ]; then
            printf '%s' "${entry#*=}" | tr ',' ' '
            return 0
        fi
    done
    return 1
}

package_changed_in_diff() {
    grep -qE "^--- a/$1/.*\.rs\$|^\+\+\+ b/$1/.*\.rs\$" "$DIFF"
}

# Valid scopes derive from MUTATION_SCOPE_PACKAGES rather than a literal list, so
# adding a scope to the table is the only edit a new slice needs. `generic` owns
# the complement of the table; `all` runs everything, which is what a human
# typing `task mutants:diff` gets; `list-scopes` prints a plan and exits.
#
# Initialised BEFORE the loop: under `set -u` an unknown scope would otherwise
# read an unset variable and abort with "unbound variable" instead of the
# message below that names the valid set.
SCOPE_IS_VALID=no
for entry in $MUTATION_SCOPE_PACKAGES; do
    if [ "$SCOPE" = "${entry%%=*}" ]; then SCOPE_IS_VALID=yes; fi
done
case "$SCOPE" in
    all|generic|list-scopes) SCOPE_IS_VALID=yes ;;
esac
if [ "$SCOPE_IS_VALID" != yes ]; then
    {
        echo "unknown mutation scope: $SCOPE"
        printf 'expected one of: all generic list-scopes'
        for entry in $MUTATION_SCOPE_PACKAGES; do printf ' %s' "${entry%%=*}"; done
        printf '\n'
    } >&2
    exit 1
fi

# A shard is `k/n` with 0 <= k < n, as cargo-mutants numbers them. A typo
# here would otherwise run a slice of nothing and pass.
if [ -n "${MUTANTS_SHARD:-}" ]; then
    shard_k=${MUTANTS_SHARD%/*}
    shard_n=${MUTANTS_SHARD#*/}
    case "$shard_k$shard_n" in
        *[!0-9]*|'') shard_ok=no ;;
        *) if [ "$shard_k" -lt "$shard_n" ]; then shard_ok=yes; else shard_ok=no; fi ;;
    esac
    if [ "$shard_ok" != yes ] || [ "$MUTANTS_SHARD" != "$shard_k/$shard_n" ]; then
        echo "MUTANTS_SHARD must be k/n with 0 <= k < n, got '$MUTANTS_SHARD'" >&2
        exit 1
    fi
fi

# `list-scopes` prints the scopes that have work for this diff, one per line,
# and exits. It runs no cargo and takes milliseconds.
#
# It exists so .github/workflows/mutants.yml can build a DYNAMIC matrix. The
# slices are independent, but running them as one serial job put the promotion
# gate at 80m16s against a 90-minute ceiling — 11% headroom, thinner than the
# run-to-run variance that cancelled `task ci` twice the same day
# (`ley-line-open-908b68`). Lifting the ceiling again only moves the cliff.
#
# Dynamic rather than a fixed five-leg matrix because a scoped run that matches
# nothing is a deliberate hard MISCONFIGURED below: `task mutants:diff cli`
# typed by hand must not pass having mutated nothing. Emitting only the scopes
# with work preserves that strictness instead of loosening it to suit CI.
if [ "$SCOPE" = list-scopes ]; then
    # The generic slice owns the COMPLEMENT of the table, so it has work
    # whenever the diff touches Rust outside every owned package. Built from
    # GENERIC_EXCLUDES so it cannot disagree with what the slice excludes.
    # Built by accumulation, not `tr ' ' '|'`: the exclude list carries a
    # trailing space, which `tr` turns into an empty final alternative and grep
    # rejects with "empty (sub)expression".
    generic_pattern=''
    for package in $GENERIC_EXCLUDES; do
        generic_pattern="${generic_pattern:+$generic_pattern|}$package"
    done
    plan=''
    if grep -E '^(---|\+\+\+) [ab]/.*\.rs$' "$DIFF" \
        | grep -qvE "[ab]/($generic_pattern)/"; then
        plan='generic'
    fi
    for entry in $MUTATION_SCOPE_PACKAGES; do
        scope=${entry%%=*}
        for owned in $(printf '%s' "${entry#*=}" | tr ',' ' '); do
            if package_changed_in_diff "$owned"; then
                case " $plan " in
                    *" $scope "*) ;;
                    *) plan="${plan:+$plan }$scope" ;;
                esac
            fi
        done
    done

    # The pairing guarantee, asserted HERE because no single matrix leg can see
    # it. Run serially, `assert_excluded_packages_were_covered` catches a
    # package the generic slice excludes and no slice claims. Split across legs,
    # each leg knows only its own scope, so that check is vacuous in every one
    # of them — and a package owned by nobody surfaces as a green matrix of
    # skipped legs, the precise false green this script exists to prevent.
    # Planning is the one place the whole picture exists.
    for package in $GENERIC_EXCLUDES; do
        package_changed_in_diff "$package" || continue
        owner=$(scope_owning_package "$package" || true)
        # `-n` guard first. An unowned package yields an EMPTY owner, and an
        # empty needle matches the plan string unconditionally — so without
        # this the assertion silently `continue`s on exactly the case it exists
        # to catch. That is the bug shape that let an earlier prototype of this
        # planner pass an unowned package.
        if [ -n "$owner" ]; then
            case " $plan " in
                *" $owner "*) continue ;;
            esac
        fi
        {
            echo "MISCONFIGURED: the diff changes $package, which the generic"
            echo "               slice excludes and no mutation scope owns."
            echo "               A matrix built from this plan would skip it and"
            echo "               report green having mutated nothing."
        } >&2
        exit 1
    done

    for scope in $plan; do echo "$scope"; done
    exit 0
fi

# ONE unmutated baseline per (run, scope) (bead ley-line-open-35780f).
#
# cargo-mutants builds and tests the unmutated tree before it mutates anything,
# and a slice split into n shards did that n times: same commit, same packages,
# same answer. Over one week that was 10.7 of 96 runner-hours (GitHub #607).
#
#   record  run each slice's baseline once and write one line per slice to
#           $MUTANTS_BASELINE_RECORD. cargo-mutants has no baseline-only mode
#           and runs no baseline for an empty mutant list, so the run carries
#           one probe mutant. The probe's own outcome is ignored; the shards
#           test that mutant again.
#   reuse   run the slice with `--baseline=skip --timeout <recorded>`.
#
# `--baseline=skip` makes the record load-bearing. cargo-mutants trusts a
# skipped baseline: were the unmutated tests failing, every mutant would be
# reported CAUGHT by failures it did not cause. So reuse refuses to run on
# anything but a recorded pass for THIS diff and THIS slice — a missing record,
# a record of another diff, a missing slice and a failed slice all exit 4, the
# code a broken baseline has always had here.
#
# A recorded baseline is ONE package's. cargo-mutants baselines the packages
# of the mutants a run draws, so a shard of a multi-package slice used to
# build only its own packages, and a joint baseline builds them all together,
# where cargo unifies their features. A tree that breaks only when some of
# them build alone would pass the joint baseline and then fail in the shard,
# with every mutant there reported caught. This gate has met that class twice
# (bead ley-line-open-c1f8bc; the binary-only shard). The one multi-package
# slice, the generic remainder, therefore keeps its own baseline in every
# shard (`slice_baseline=own` at its call), and recording refuses any other
# slice that lists two packages. Recording the remainder per package instead
# would save little: slice sharding hands each shard a contiguous run of
# packages, so its shards together already baseline each package about once.
#
# The timeout DEPARTS from cargo-mutants' rule on purpose. cargo-mutants
# (27.1.0 `timeouts.rs`) sets `max(minimum, ceil(test seconds x multiplier))`
# from the baseline it has just run on the same machine, with the multiplier
# `timeout_multiplier` from .cargo/mutants.toml or 5 and the minimum
# CARGO_MUTANTS_MINIMUM_TEST_TIMEOUT or 20. Here the baseline ran on another
# runner, and a mutant whose tests are cut off is counted as detected, so a
# slower shard runner could turn a surviving mutant into a timeout. Shards get
# that value x 1.5, rounded up. Measured on the runtime slice (runs
# 45089874724, 45220086882): baseline tests 31 to 37 s, the slowest test run
# that finished 121 s (3.9 x the baseline on its runner), surviving mutants'
# 70 to 87 s; baselines of one tree differed 1.2 x in test time and 1.5 x in
# build time between runners. 7.5 x the baseline leaves room for a shard
# runner 1.9 x slower than the baseline's. The cost is that each mutant that
# does hang takes half as long again to stop. Without an explicit timeout a
# skipped baseline falls back to a flat 300 s. There is no build timeout, as
# before.
BASELINE_MODE=${MUTANTS_BASELINE:-run}
BASELINE_RECORD=${MUTANTS_BASELINE_RECORD:-}
case "$BASELINE_MODE" in
    run|record|reuse) ;;
    *)
        echo "MUTANTS_BASELINE must be record or reuse (or unset), got '$BASELINE_MODE'" >&2
        exit 1
        ;;
esac
if [ "$BASELINE_MODE" != run ] && [ -z "$BASELINE_RECORD" ]; then
    echo "MUTANTS_BASELINE=$BASELINE_MODE needs MUTANTS_BASELINE_RECORD=<file>" >&2
    exit 1
fi
if [ "$BASELINE_MODE" = record ] && [ -n "${MUTANTS_SHARD:-}" ]; then
    echo "MUTANTS_BASELINE=record covers every shard; it cannot run with MUTANTS_SHARD" >&2
    exit 1
fi

# A record names the diff it was made for. The same slice arguments recur
# across diffs, so the slice key alone would accept another run's baseline.
baseline_diff_id=$(cksum < "$DIFF" | awk '{ print $1 "-" $2 }')
if [ "$BASELINE_MODE" = record ]; then
    mkdir -p "$(dirname -- "$BASELINE_RECORD")"
    printf 'diff %s\n' "$baseline_diff_id" > "$BASELINE_RECORD"
fi
if [ "$BASELINE_MODE" = reuse ]; then
    if [ ! -f "$BASELINE_RECORD" ]; then
        {
            echo "BASELINE MISSING: no baseline record at $BASELINE_RECORD."
            echo "                  The baseline for this scope did not finish, so"
            echo "                  NOTHING was mutated. This is not a pass."
        } >&2
        exit 4
    fi
    if ! grep -qx "diff $baseline_diff_id" "$BASELINE_RECORD"; then
        {
            echo "BASELINE MISSING: $BASELINE_RECORD records the baseline of a"
            echo "                  different diff, so NOTHING was mutated."
        } >&2
        exit 4
    fi
fi

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

# Zero mutants from a Rust-touching diff has two causes that need opposite
# responses, and the difference is not visible in cargo-mutants' output — it
# prints "No mutants to filter" for both.
#
#   1. The paths do not resolve. This is the trap: a diff generated from the
#      repo root carries `b/rs/ll-core/...`, matches nothing, and the naive
#      implementation exits 0 forever.
#   2. The paths resolve fine and the changed lines simply hold nothing
#      mutable — a `#[cfg(test)] mod tests` edit, a doc comment, a `use`, an
#      integration test under `tests/`. Nothing to mutate is the correct
#      answer here, not a failure.
#
# Deciding between them by asking the filesystem tests the actual claim the
# error message makes ("your paths are not workspace-relative") rather than a
# proxy for it. This script's cwd IS the cargo-mutants working directory, so a
# workspace-relative path is exactly one that resolves from here.
#
# Sets `resolved` (a count) and `unresolved` (a newline-terminated list) for
# the diff's changed Rust files whose path begins with $1 — pass '' for all of
# them. The whole-diff check and the per-scope check below ask the same
# question of different subsets, so they share the one answer.
resolve_changed_paths() {
    changed=$(sed -n "s|^+++ b/\($1.*\.rs\)\$|\1|p" "$DIFF")
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
}

if [ "${n:-0}" -eq 0 ]; then
    resolve_changed_paths ''

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

if [ "$SCOPE" != all ] && [ "$SCOPE" != generic ]; then
    # Derived from MUTATION_SCOPE_PACKAGES, not a second hand-written map. `cli`
    # owns two packages, so match either — a cli-only diff enumerating mutants
    # in ll-open/cli alone is still work this scope did.
    scope_pattern=$(packages_in_scope "$SCOPE" | tr ' ' '|' | sed 's/|$//')
    scope_n=$(printf '%s\n' "$listing" | grep -cE "^($scope_pattern)/.*:[0-9]+:[0-9]+:" || true)
    if [ "${scope_n:-0}" -eq 0 ]; then
        # The same two causes as the whole-diff check, asked of this scope's
        # packages alone. Another package's mutants say nothing about them:
        # the planner emits a scope for ANY Rust change in its package, and
        # cargo-mutants never enumerates integration tests, so a diff that
        # touches ll-open/runtime only under tests/ reaches here with the
        # whole-diff count above zero. That is what the first integration ->
        # main promotion through the matrix hit (ley-line-open-908b68), and
        # it is case 2, not a misconfiguration.
        scope_resolved=0
        scope_unresolved=""
        for owned in $(packages_in_scope "$SCOPE"); do
            resolve_changed_paths "$owned/"
            scope_resolved=$((scope_resolved + resolved))
            scope_unresolved="$scope_unresolved$unresolved"
        done
        if [ "$scope_resolved" -eq 0 ]; then
            {
                echo "MISCONFIGURED: mutation scope '$SCOPE' enumerated zero candidates"
                echo "               from its package(s), and none of the diff's paths"
                echo "               there resolve from $(pwd) — where cargo mutants"
                echo "               runs. Mutants in another changed package cannot"
                echo "               make this focused run pass."
                echo "               Regenerate with: git diff --relative=rs"
                echo "--- unresolved paths ---"
                printf '%s' "$scope_unresolved" | sed 's/^/    /'
            } >&2
            exit 1
        fi
        echo "NO MUTABLE LINES: scope '$SCOPE' — the diff's $scope_resolved Rust file(s)"
        echo "                  in its package(s) resolve, but the lines it changes"
        echo "                  hold nothing cargo-mutants can mutate — integration"
        echo "                  tests, test modules, comments or imports. This is"
        echo "                  NOT a pass; no mutation testing ran in this scope."
        exit 0
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


overall=0
ran_slice=0
covered_packages=''
claim_package() {
    covered_packages="$covered_packages $1"
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

# MUTANTS_BASELINE=record for one slice. "$@" is the slice's arguments.
record_baseline() {
    # The first mutant of the slice's own unsharded listing, selected by its
    # exact name with `--re`. `--list --json` names each mutant and its
    # package.
    if ! listed=$(cargo mutants --in-diff "$DIFF" "$@" --list --json 2>/dev/null); then
        echo "MISCONFIGURED: could not list this slice's mutants to record its baseline: $*" >&2
        exit 1
    fi
    probe=''
    packages=0
    if [ -n "$listed" ]; then
        if ! probe=$(printf '%s\n' "$listed" | jq -r '.[0].name // empty') \
            || ! packages=$(printf '%s\n' "$listed" | jq -r '[.[].package] | unique | length'); then
            echo "MISCONFIGURED: cargo mutants --list --json did not print a mutant list: $*" >&2
            exit 1
        fi
    fi
    if [ -z "$probe" ]; then
        echo "baseline record: no mutants in this slice ($*)"
        printf 'slice %s none\n' "$slice_key" >> "$BASELINE_RECORD"
        return 0
    fi
    if [ "$packages" -ne 1 ]; then
        {
            echo "MISCONFIGURED: this slice mutates $packages packages, and one recorded"
            echo "               baseline would build them together. A shard builds only"
            echo "               the packages it draws. Give the slice its own baseline"
            echo "               (slice_baseline=own): $*"
        } >&2
        exit 1
    fi
    # One build directory: the baseline and the probe are all this run builds,
    # so more would only copy the tree again.
    set -- "$@" --re "^$(printf '%s' "$probe" | sed 's/[][\.*^$+?(){}|]/\\&/g')\$" --jobs 1
    # cargo-mutants exits 0 on "No mutants to filter" before it opens its
    # output directory. The outcomes read below must be THIS invocation's, not
    # the slice before's, whose `Baseline: Success` would otherwise be recorded
    # for a slice whose baseline never ran.
    rm -rf mutants.out
    set +e
    cargo mutants --in-diff "$DIFF" "$@"
    rc=$?
    set -e
    # A pass is exactly one baseline, reported as passed, and the one probe's
    # outcome after it. Anything else is not the run that was asked for.
    baseline_summary=$(jq -r '
        [.outcomes[] | select(.scenario == "Baseline")] as $baselines
        | [.outcomes[] | select(.scenario != "Baseline")] as $probes
        | if ($baselines | length) != 1 then "reported \($baselines | length) times"
          elif $baselines[0].summary != "Success" then $baselines[0].summary
          elif ($probes | length) != 1 then "Success, with \($probes | length) probe outcomes"
          else "Success" end' mutants.out/outcomes.json 2>/dev/null) || baseline_summary=''
    : "${baseline_summary:=absent}"
    if [ "$rc" -eq 4 ] || [ "$baseline_summary" != Success ]; then
        printf 'slice %s failed\n' "$slice_key" >> "$BASELINE_RECORD"
        {
            echo "BASELINE BROKEN: the unmutated tree failed its own tests in the"
            echo "                 mutants scratch build (cargo-mutants exit $rc,"
            echo "                 baseline $baseline_summary), so NOTHING will be"
            echo "                 mutated in this slice. This is not a surviving"
            echo "                 mutant. Usually a test reading files outside its crate."
        } >&2
        exit 4
    fi
    # The probe's outcome is not this run's result: 2 is a probe that
    # survived and 3 one that timed out, and the shards report both. Anything
    # else is cargo-mutants failing to run.
    case "$rc" in
        0|2|3) ;;
        *)
            echo "cargo mutants exited $rc while recording this slice's baseline" >&2
            exit "$rc"
            ;;
    esac
    baseline_build=$(jq -r '[.outcomes[] | select(.scenario == "Baseline")][0].phase_results[] | select(.phase == "Build") | .duration' mutants.out/outcomes.json)
    baseline_test=$(jq -r '[.outcomes[] | select(.scenario == "Baseline")][0].phase_results[] | select(.phase == "Test") | .duration' mutants.out/outcomes.json)
    case "$baseline_test" in
        ''|*[!0-9.]*)
            echo "MISCONFIGURED: the recorded baseline has no test duration ('$baseline_test')" >&2
            exit 1
            ;;
    esac
    # cargo-mutants' value for this baseline (27.1.0 timeouts.rs
    # `test_timeout`), then half as much again for a shard on a slower runner.
    # The block above the baseline modes says why.
    multiplier=$(sed -n 's/^[[:space:]]*timeout_multiplier[[:space:]]*=[[:space:]]*\([0-9.]*\).*/\1/p' .cargo/mutants.toml 2>/dev/null | head -n 1)
    timeout=$(awk -v test="$baseline_test" -v multiplier="${multiplier:-5}" \
        -v minimum="${CARGO_MUTANTS_MINIMUM_TEST_TIMEOUT:-20}" 'BEGIN {
            scaled = test * multiplier
            timeout = int(scaled)
            if (timeout < scaled) timeout++
            if (timeout < minimum) timeout = int(minimum)
            slack = timeout * 1.5
            timeout = int(slack)
            if (timeout < slack) timeout++
            print timeout
        }')
    printf 'slice %s ok %s %s %s\n' "$slice_key" "$timeout" "$baseline_build" "$baseline_test" >> "$BASELINE_RECORD"
    echo "BASELINE RECORDED: unmutated build ${baseline_build}s + test ${baseline_test}s; shards run --baseline=skip --timeout $timeout"
}

# `own` for a slice that spans packages; set at the call, reset after it.
slice_baseline=shared

run_slice() {
    mode=$1
    shift
    ran_slice=1
    # cargo-mutants writes `mutants.out/missed.txt` for the run it actually
    # performs. A slice that enumerates ZERO mutants never writes it at all —
    # it prints "No mutants to filter" and exits — so the check below would
    # read whatever the PREVIOUS slice, or a previous local invocation, left
    # behind.
    #
    # That is not hypothetical: a clean diff reported "MISSED: 13 surviving
    # mutant(s)" against a slice whose very next line was "Found 5 mutants to
    # test", because a whole-file `cargo mutants` run minutes earlier had left
    # 13 entries in the file. CI did not see it only because a fresh checkout
    # starts with no `mutants.out` at all.
    #
    # Clearing it first makes the check able to see only THIS slice's result,
    # which is the difference between a gate and a rumour.
    rm -f mutants.out/missed.txt
    if [ "$mode" = lib ]; then
        set -- -C --lib -E 'replace main' "$@"
    elif [ "$mode" = bins ]; then
        set -- -C --bins -E 'replace main' "$@"
    else
        set -- -E 'replace main' "$@"
    fi
    # What names this slice in a baseline record: every argument that decides
    # what its baseline builds and tests, and nothing that differs between the
    # recording run and a shard (the diff's path, the shard, the baseline
    # flags added below).
    slice_key=$(printf '%s\n' "$*" | cksum | awk '{ print $1 "-" $2 }')
    if [ "$slice_baseline" = own ] && [ "$BASELINE_MODE" != run ]; then
        echo "this slice spans packages: every shard runs its own baseline"
        if [ "$BASELINE_MODE" = record ]; then
            return 0
        fi
    elif [ "$BASELINE_MODE" = record ]; then
        record_baseline "$@"
        return 0
    elif [ "$BASELINE_MODE" = reuse ]; then
        recorded=$(awk -v key="$slice_key" '$1 == "slice" && $2 == key { line = $0 } END { print line }' "$BASELINE_RECORD")
        # A record line is `slice <key> <status> <timeout> <build s> <test s>`.
        recorded_status=''
        recorded_timeout=''
        recorded_test=''
        IFS=' ' read -r _ _ recorded_status recorded_timeout _ recorded_test <<RECORDED
$recorded
RECORDED
        case "$recorded_status" in
            ok)
                echo "baseline reused: unmutated tests took ${recorded_test}s in this run's baseline; --baseline=skip --timeout $recorded_timeout"
                set -- "$@" --baseline=skip --timeout "$recorded_timeout"
                ;;
            none)
                # The recording run enumerated no mutant here, so it had no
                # baseline to run. No shard should find one either; if one
                # does, cargo-mutants runs its own baseline, as it always did.
                echo "baseline record: no mutants in this slice"
                ;;
            failed)
                {
                    echo "BASELINE BROKEN: the unmutated tree failed its own tests in this"
                    echo "                 run's baseline for this slice, so NOTHING was"
                    echo "                 mutated. This is not a surviving mutant; the"
                    echo "                 baseline job's log has the failure."
                } >&2
                exit 4
                ;;
            *)
                {
                    echo "BASELINE MISSING: $BASELINE_RECORD has no baseline for this"
                    echo "                  slice ($slice_key: $*), so NOTHING"
                    echo "                  was mutated. This is not a pass."
                } >&2
                exit 4
                ;;
        esac
    fi
    # MUTANTS_SHARD=k/n runs one of n shards of the slice. CI splits a slow
    # slice across runners this way instead of lifting a job's time limit; a
    # slice too slow to fit is a slice doing too much (mutants.yml).
    if [ -n "${MUTANTS_SHARD:-}" ]; then
        set -- "$@" --shard "$MUTANTS_SHARD"
    fi
    set +e
    cargo mutants --in-diff "$DIFF" "$@"
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
# leyline-fs's covered modules (blob_chunked.rs, chunk_pool.rs, gc.rs) are
# exactly that shape, so
# fs files route to their own invocation carrying the same feature set as
# `mutants:fs`/`mutants:fs-gc`. If another crate ever hides covered code
# behind non-default features, it needs the same routing — a default-features
# run structurally cannot test it.
if [ "$SCOPE" = all ] || [ "$SCOPE" = generic ]; then
    # cargo-mutants selects only the packages the diff touches, and cargo
    # REJECTS a `--features` name that none of the selected packages declares.
    # A STATIC list is therefore wrong, and was already wrong before these
    # entries were added: `--features hcl` broke every diff that did not touch
    # leyline-ts with "the package 'X' does not contain this feature: hcl" — a
    # BASELINE BROKEN whose message says nothing about the code under test.
    # Build the list from the packages actually in the diff instead.
    #
    # Each row is a non-default feature gating COVERED code in a generic-slice
    # package. Without it, cargo-mutants mutates the gated lines while compiling
    # them OUT, so every mutant in that module survives having tested nothing.
    # It reports MISSED, which reads as "you are missing a test" when the truth
    # is "this gate never saw the module."
    #
    # WHICH features route here is not this script's private knowledge: the
    # `mutants=enable` rows of tools/feature-ledger.txt carry them, with each
    # row's measured phantom-MISSED history beside it. The ledger is shared
    # with check_feature_reachability.sh precisely so the two gates cannot
    # drift apart — this script knowing three features while that one knew
    # nine is the shape that left witchcraft.rs's 28 phantoms unread for five
    # weeks (bead ley-line-open-cb1e29). Resolved relative to THIS script,
    # never the cwd: tools/test_mutants_diff.sh invokes us from a fixture
    # directory.
    ledger=$(CDPATH='' cd -P -- "$(dirname "$0")" && pwd -P)/feature-ledger.txt
    if [ ! -f "$ledger" ]; then
        {
            echo "MISCONFIGURED: $ledger is missing — cannot decide which"
            echo "               features route into the generic slice."
        } >&2
        exit 1
    fi
    enable_rows=$(awk '!/^[[:space:]]*(#|$)/ && $4 == "enable" { print $1, $2 }' "$ledger")
    # Positive control, the same rule the reachability gate applies to its own
    # resolves: `ll-core/core interrupt` is a stable enable row (25/25 phantom
    # MISSED before it was wired). Its absence means the parse broke, not that
    # the ledger emptied — and a silently empty list would recreate exactly
    # the phantom-survivor regime the rows exist to end. If that row ever
    # legitimately leaves the ledger, point this control at another one.
    if ! printf '%s\n' "$enable_rows" | grep -qx 'll-core/core interrupt'; then
        {
            echo "MISCONFIGURED: no 'll-core/core interrupt' enable row parsed"
            echo "               from $ledger — the ledger parse broke, or the"
            echo "               ledger regressed."
        } >&2
        exit 1
    fi
    # The packages in the diff that carry enable rows, each named once.
    featured_packages=''
    while read -r pkg feat; do
        [ -n "$pkg" ] || continue
        package_changed_in_diff "$pkg" || continue
        case " $featured_packages " in
            *" $pkg "*) ;;
            *) featured_packages="${featured_packages:+$featured_packages }$pkg" ;;
        esac
    done <<LEDGER_ROWS
$enable_rows
LEDGER_ROWS
    # One package's enable-row features, comma-joined.
    features_of() {
        printf '%s\n' "$enable_rows" \
            | awk -v pkg="$1" '$1 == pkg { printf "%s%s", sep, $2; sep = "," }'
    }

    # File-level exclusions come from the same ledger (`exclude-file:` rows).
    # Today that is witchcraft.rs alone — its row's prose says why exclusion
    # beats enabling the feature (the engine needs a live Embedder + T5 assets
    # CI lacks) and what unlocks removal (ley-line-open-b23c41). Claim 3 of
    # check_feature_reachability.sh still compiles the module with its tests,
    # so excluded code cannot rot unseen. A broken parse here fails loud, not
    # green: the un-excluded module enumerates its phantoms and reddens the
    # gate.
    exclude_file_args=''
    for excluded in $(awk '!/^[[:space:]]*(#|$)/ && $4 ~ /^exclude-file:/ { sub("exclude-file:", "", $4); print $4 }' "$ledger"); do
        exclude_file_args="$exclude_file_args --exclude $excluded"
    done

    # Each package's features reach ONLY that package's build, so a package
    # carrying enable rows gets an invocation of its own, confined to its
    # files with `--file`. One invocation carrying the union is wrong: cargo
    # rejects a `--features` name none of the SELECTED packages declares,
    # and cargo-mutants selects only the packages whose mutants it runs —
    # which, once a slice is sharded, is a subset of the diff's packages.
    # Package-qualified names (`leyline-ts/hcl`) do not help; cargo refuses
    # those too for an unselected package. The v0.21.0 promotion diff hit it
    # in every generic shard: shard 0 held only leyline-core and leyline-sign
    # mutants, was handed `hcl,pyproject` for leyline-ts, and reported
    # BASELINE BROKEN having mutated nothing (bead ley-line-open-c1f8bc).
    #
    # $exclude_file_args expands unquoted by design: it is a flag list, and
    # ledger paths carry no whitespace.
    for pkg in $featured_packages; do
        pkg_features=$(features_of "$pkg")
        echo "generic slice: $pkg with its own features: $pkg_features"
        # shellcheck disable=SC2086
        run_slice lib --file "$pkg/**" --features "$pkg_features" $exclude_file_args
    done

    # A generic-slice package with no library target cannot run under
    # `-C --lib`: cargo answers "no library targets found in package" and the
    # slice reports BASELINE BROKEN. A package selected alongside a library
    # package hides this, so it surfaces exactly when sharding puts a
    # binary-only package's mutants in a shard of their own — the v0.21.0
    # promotion's two `rs/tools/smell-projector` mutants did. Such a package
    # keeps its unit tests in its binary, so it runs them with `-C --bins`, in
    # an invocation of its own. Found from the diff's paths: the nearest
    # directory holding a Cargo.toml is the package, and it is binary-only
    # when it has neither `src/lib.rs` nor a `[lib]` table.
    owned_packages=$(derive_generic_excludes)
    bin_only_packages=''
    while IFS= read -r path; do
        [ -n "$path" ] || continue
        dir=$(dirname -- "$path")
        while [ "$dir" != . ] && [ "$dir" != / ] && [ ! -f "$dir/Cargo.toml" ]; do
            dir=$(dirname -- "$dir")
        done
        # `.` is the workspace manifest, not a package of the generic slice.
        [ "$dir" != . ] && [ -f "$dir/Cargo.toml" ] || continue
        case " $owned_packages $featured_packages $bin_only_packages " in
            *" $dir "*) continue ;;
        esac
        if [ ! -f "$dir/src/lib.rs" ] && ! grep -q '^\[lib\]' "$dir/Cargo.toml"; then
            bin_only_packages="${bin_only_packages:+$bin_only_packages }$dir"
        fi
    done <<CHANGED_PATHS
$(sed -n 's|^+++ b/\(.*\.rs\)$|\1|p' "$DIFF")
CHANGED_PATHS
    for pkg in $bin_only_packages; do
        echo "generic slice: $pkg has no library target; its binary's tests run"
        # shellcheck disable=SC2086
        run_slice bins --file "$pkg/**" $exclude_file_args
    done

    # Everything else in the generic slice, with no feature flags: the
    # packages a scope owns (from the table, never the test override) and
    # the packages mutated above are excluded.
    set --
    for pkg in $owned_packages $featured_packages $bin_only_packages; do
        set -- "$@" --exclude "$pkg/**"
    done
    # The one slice that spans packages: it keeps its own baseline.
    slice_baseline=own
    # shellcheck disable=SC2086
    run_slice lib $exclude_file_args "$@"
    slice_baseline=shared
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
if { [ "$SCOPE" = all ] || [ "$SCOPE" = schema-bridge ]; } && package_changed_in_diff ll-open/schema-bridge; then
    claim_package ll-open/schema-bridge
    run_slice integration --package leyline-schema-bridge --test-workspace=false
fi

# schema-spec is a spec-artifact crate whose behavior is asserted against its
# vectors: `tests/verify_*` read the spec directories beside `src/` and run the
# independent Python references there. The lib-only generic slice ran none of
# them, so every parser and resolver mutant the vectors kill read as missed
# (bead ley-line-open-52c177: 154 of them on deployment_config). Every file
# those tests read lives inside the crate, so the scratch copy holds it.
if { [ "$SCOPE" = all ] || [ "$SCOPE" = schema-spec ]; } && package_changed_in_diff ll-core/schema-spec; then
    claim_package ll-core/schema-spec
    run_slice integration --package leyline-schema-spec --test-workspace=false
fi

if [ "$SCOPE" = all ] || [ "$SCOPE" = runtime ]; then
  if package_changed_in_diff ll-open/runtime; then
    claim_package ll-open/runtime
    # `backends/cgroup.rs` takes the filesystem magic of a cgroup root from a
    # per-platform file (bead ley-line-open-da36d2): `cgroup_fs_linux.rs`
    # (`statfs(2)`) or `cgroup_fs_other.rs` (no cgroup v2, so a refusal). A
    # platform gate is a feature gate cargo-mutants cannot see, so each file
    # is excluded where it does not compile — there every mutant would build
    # unchanged and read as a phantom MISSED — and mutated where it does.
    # `broker/sys_macos.rs` and `broker/sys_other.rs` (bead
    # ley-line-open-52b5b3) are the same kind of pair: the connection broker's
    # ACL and peer-credential queries, one file per platform. So are
    # `backends/spawn/macos.rs` (`posix_spawn`) and `backends/spawn/linux.rs`
    # (`Command` + `pre_exec`) (bead ley-line-open-528525): on Linux the macOS
    # file's mutants built unchanged and read as six phantom MISSED.
    if [ "$(uname -s)" = Linux ]; then
        run_slice integration --package leyline-runtime --test-workspace=false \
            --exclude 'll-open/runtime/src/backends/cgroup_fs_other.rs' \
            --exclude 'll-open/runtime/src/broker/sys_macos.rs' \
            --exclude 'll-open/runtime/src/backends/spawn/macos.rs'
    else
        run_slice integration --package leyline-runtime --test-workspace=false \
            --exclude 'll-open/runtime/src/backends/cgroup_fs_linux.rs' \
            --exclude 'll-open/runtime/src/broker/sys_other.rs' \
            --exclude 'll-open/runtime/src/backends/spawn/linux.rs'
    fi
  fi
  # The worker crate (bead ley-line-open-395e43) is the runtime's libkrun half:
  # its api.rs and worker.rs are asserted by its own tests/, which the
  # lib-only generic slice would never run.
  if package_changed_in_diff ll-open/krun-worker; then
    claim_package ll-open/krun-worker
    run_slice integration --package leyline-krun-worker --test-workspace=false
  fi
fi

if [ "$SCOPE" = all ] || [ "$SCOPE" = cli ]; then
  if package_changed_in_diff ll-open/cli-lib; then
    claim_package ll-open/cli-lib
    # `--features vec`: WITHOUT it this slice is blind to `daemon/embed.rs`.
    # `daemon/mod.rs` gates `pub mod embed;` behind `#[cfg(feature = "vec")]`
    # and `vec` is not in cli-lib's defaults, so a default-features run
    # compiles the module OUT — while cargo-mutants, which parses with syn and
    # does not evaluate cfgs, still enumerates every mutant inside it. Each one
    # then "builds" in 0s, every test trivially passes, and it is reported
    # MISSED: a phantom survivor on healthy code, indistinguishable in the log
    # from a real missing assertion. The projection-v5 PR surfaced six of them.
    #
    # This is the same hazard the generic slice's feature block above documents
    # — the difference is that the generic slice learned it from
    # tools/feature-ledger.txt and THIS slice never did, because the ledger
    # only feeds the generic run and the generic run excludes cli-lib. A ledger
    # row alone therefore cannot fix it; the feature has to be named here.
    #
    # Measured: `embed.rs:193` reports "1 missed ... 0s build" under default
    # features and "3 caught" with `--features vec`; `cargo test -p
    # leyline-cli-lib --lib embedding_pass` runs 0 tests by default and 5 with
    # it. The whole binary set below compiles with the feature and its deps are
    # already in Cargo.lock, so this costs a longer link, not a download.
    #
    # lsp_enrich_pipeline: kills the whole-function replacement mutants on
    # `enrich_files_with_client` (four survived on the PR that added the
    # test — nothing observed that function's counts or writes before it).
    # syntax_tree_single_copy_test: the only test that decodes the
    # `.ast.capnp` segment record by record against `_ast`, so it kills a
    # `serialize_ast_node_record -> Ok(())` stub (the segment `Head.rootHash`
    # folds). It replaced pointer_store_dual_write_test when projection-v8
    # retired the pointer store (bead f30fdf).
    # injection_extraction_test: the only tests that observe an INJECTED
    # subtree's nid. `inj_injected_nid_scheme_pinned` asserts the injected nid
    # lands in its host file's range, which is what kills `base | ordinal`
    # mutated to `base & ordinal` — `base`'s low 24 bits are zero, so `&`
    # collapses every injected nid to 0 and the fact rows leave the host's
    # range entirely. Verified before routing: the mutation leaves the lib
    # suite green and fails five tests in this binary.
    # watcher_enrichment_gap1_test: the only tests that call
    # `cmd_daemon::run_watcher_enrichment` and observe its phase change and
    # `daemon.enrichment.*` emits. `replace run_watcher_enrichment with ()`
    # survived on the sheaf-feature PR (bead 39e239); verified before
    # routing: that mutation fails all three tests in this binary.
    # head_policy_env: the only test that sets the head trust variables and
    # reads them back through `HeadPolicy::from_env`. It is its own binary so
    # setting `LEYLINE_HEAD_TRUSTED_KEYS` cannot race a lib test that
    # verifies a head (bead b68fa6). It kills `from_env -> Ok(Default)`, which
    # survives the lib suite.
    #
    # `durable/apple.rs` (`F_BARRIERFSYNC`) and `durable/other.rs` (`fsync`)
    # are platform gates (bead `ley-line-open-98d0c3`): the file a runner's
    # platform does not build would report every mutant as a phantom MISSED,
    # so it is excluded there and mutated where it compiles, where
    # `durable::tests::order_reports_a_descriptor_it_cannot_sync` kills it.
    if [ "$(uname -s)" = Darwin ]; then
        durable_unbuilt='ll-open/cli-lib/src/durable/other.rs'
    else
        durable_unbuilt='ll-open/cli-lib/src/durable/apple.rs'
    fi
    run_slice integration \
        --exclude "$durable_unbuilt" \
        --features vec \
        -C --lib \
        -C --test -C execution_client \
        -C --test -C execution_transport \
        -C --test -C cdc_activation_consumer_test \
        -C --test -C cdc_command_test \
        -C --test -C lsp_enrich_pipeline \
        -C --test -C syntax_tree_single_copy_test \
        -C --test -C injection_extraction_test \
        -C --test -C watcher_enrichment_gap1_test \
        -C --test -C head_policy_env \
        --package leyline-cli-lib --test-workspace=false
  fi
fi

if { [ "$SCOPE" = all ] || [ "$SCOPE" = storefs ]; } && package_changed_in_diff ll-open/storefs; then
    claim_package ll-open/storefs
    # The host mount (`host/`, bead `ley-line-open-3e73dc`) is gated on
    # storefs's non-default `mount` feature and its FUSE adapter on `fuse`
    # (which implies `mount`, bead `ley-line-open-9d4712`), so the slice names
    # `fuse`: without it every adapter mutant compiles out and reports a
    # phantom MISSED. Lib
    # tests only (`run_slice lib`): they drive the adapters below the kernel.
    # `tests/host_mount.rs` mounts through the kernel's NFS or FUSE client,
    # and a mutation run must never mount.
    #
    # `host/fuse.rs` is excluded: it is the `fuser::Filesystem` glue, whose
    # requests and replies only a `/dev/fuse` session can build (fuser keeps
    # `Request::new` and `ReplySender` private), so lib tests cannot reach it
    # and every mutant there would report MISSED. Every decision it makes
    # lives in `host/fuse_abi.rs`, which this slice mutates; the glue is
    # exercised through the kernel by `tests/host_mount.rs` and
    # `tools/cli_smoke.sh --mount`. The gap is `ley-line-open-5241ee`.
    #
    # `host/macos.rs` is `#[cfg(target_os = "macos")]` (a `sysctlbyname(3)`
    # read): a platform gate is a feature gate cargo-mutants cannot see either,
    # so on any other runner every mutant in it builds in 0s and reports a
    # phantom MISSED. It is excluded there and mutated on macOS, where its test
    # checks it against `sw_vers`.
    if [ "$(uname -s)" = Darwin ]; then
        run_slice lib --package leyline-storefs --test-workspace=false --features fuse \
            --exclude 'll-open/storefs/src/host/fuse.rs'
    else
        run_slice lib --package leyline-storefs --test-workspace=false --features fuse \
            --exclude 'll-open/storefs/src/host/fuse.rs' \
            --exclude 'll-open/storefs/src/host/macos.rs'
    fi
fi

if { [ "$SCOPE" = all ] || [ "$SCOPE" = fs ]; } && package_changed_in_diff ll-open/fs; then
    claim_package ll-open/fs
    # This is the ONLY slice that passes `--no-default-features`, so it names
    # every feature whose code it mutates: `cdc` (the chunked read path and
    # GC), `validate`, and `verify` (verify-on-fault serving: `verified.rs`
    # and the `#[cfg(feature = "verify")]` loaders in `graph.rs`). A gated
    # module the flags compiled out would still have every mutant enumerated —
    # cargo-mutants parses with syn and never evaluates cfg — and each would
    # build in 0s, pass every test, and be reported MISSED: a phantom survivor
    # on healthy code (bead `ley-line-open-b23c41`). `verify` does not ship
    # (`ley-line-open-b6a4dd`), but `fs:test:cdc` runs the crate's tests with
    # it, so its mutants are killable and the slice enables it.
    #
    # Kept in step by tools/check_mutants_cfg_coverage.sh, which fails if a
    # cfg-gated module is neither enabled by its slice nor excluded here.
    run_slice lib --package leyline-fs --test-workspace=false \
        --no-default-features --features cdc,validate,verify
fi

# `ll-open/cli` is the thin clap binary: its only mutant is `replace main`,
# which `-E` excludes because subprocess tests, not lib tests, kill it. It is
# claimed here so the pairing check stays honest — the moment that crate grows
# a mutable helper, the claim is a lie and the exclude list needs a real slice.
cli_binary_only=0
if package_changed_in_diff ll-open/cli; then
    claim_package ll-open/cli
    echo "NOTE: ll-open/cli carries only 'replace main', which -E excludes;"
    echo "      its behavior is gated by leyline-cli-lib's slice."
    cli_binary_only=1
fi

# The planner gives a binary-only diff the `cli` leg so the pairing check has a
# claim to satisfy; that leg has, by policy, nothing to mutate. Say so and stop,
# as NO MUTABLE LINES does, instead of calling the plan misconfigured.
if [ "$SCOPE" = cli ] && [ "$ran_slice" -eq 0 ] && [ "$cli_binary_only" -eq 1 ]; then
    echo "NO MUTABLE LINES: scope 'cli' — the diff changes only ll-open/cli, whose"
    echo "                  one mutant is excluded. This is NOT a pass; no"
    echo "                  mutation testing ran in this scope."
    exit 0
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
