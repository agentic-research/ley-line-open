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
MUTATION_SCOPE_PACKAGES='fs=ll-open/fs runtime=ll-open/runtime cli=ll-open/cli-lib,ll-open/cli schema-bridge=ll-open/schema-bridge'

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
    generic_features=''
    add_generic_features() {
        package_changed_in_diff "$1" || return 0
        generic_features="${generic_features:+$generic_features,}$2"
    }
    while read -r pkg feat; do
        [ -n "$pkg" ] || continue
        add_generic_features "$pkg" "$feat"
    done <<LEDGER_ROWS
$enable_rows
LEDGER_ROWS

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

    # $exclude_file_args expands unquoted by design: it is a flag list, and
    # ledger paths carry no whitespace.
    # shellcheck disable=SC2086
    if [ -n "$generic_features" ]; then
        echo "generic slice features (from packages in the diff): $generic_features"
        run_slice lib \
            --features "$generic_features" \
            $exclude_file_args \
            --exclude 'll-open/fs/**' \
            --exclude 'll-open/runtime/**' \
            --exclude 'll-open/cli-lib/**' \
            --exclude 'll-open/cli/**' \
            --exclude 'll-open/schema-bridge/**'
    else
        # shellcheck disable=SC2086
        run_slice lib \
            $exclude_file_args \
            --exclude 'll-open/fs/**' \
            --exclude 'll-open/runtime/**' \
            --exclude 'll-open/cli-lib/**' \
            --exclude 'll-open/cli/**' \
            --exclude 'll-open/schema-bridge/**'
    fi
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

if [ "$SCOPE" = all ] || [ "$SCOPE" = runtime ]; then
  if package_changed_in_diff ll-open/runtime; then
    claim_package ll-open/runtime
    run_slice integration --package leyline-runtime --test-workspace=false
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
    # pointer_store_dual_write_test: kills the blob-serializer stubs
    # (`serialize_ast_node_list_record -> Ok(())` survived on the Phase A
    # locator-eviction PR — the F1 gate that observes blob content lived
    # only in this file, which the slice did not run).
    # injection_extraction_test: the only tests that observe an INJECTED
    # subtree's nid. `inj_injected_nid_scheme_pinned` asserts the injected nid
    # lands in its host file's range, which is what kills `base | ordinal`
    # mutated to `base & ordinal` — `base`'s low 24 bits are zero, so `&`
    # collapses every injected nid to 0 and the fact rows leave the host's
    # range entirely. Verified before routing: the mutation leaves the lib
    # suite green and fails five tests in this binary.
    run_slice integration \
        --features vec \
        -C --lib \
        -C --test -C execution_client \
        -C --test -C execution_transport \
        -C --test -C cdc_activation_consumer_test \
        -C --test -C cdc_command_test \
        -C --test -C lsp_enrich_pipeline \
        -C --test -C pointer_store_dual_write_test \
        -C --test -C injection_extraction_test \
        --package leyline-cli-lib --test-workspace=false
  fi
fi

if { [ "$SCOPE" = all ] || [ "$SCOPE" = fs ]; } && package_changed_in_diff ll-open/fs; then
    claim_package ll-open/fs
    # This is the ONLY slice that passes `--no-default-features`, which makes it
    # the only place tools/feature-ledger.txt's standing exemption for
    # default-enabled features ("cargo resolves them, mutants compiles them") is
    # false. `fuse` and `nfs` ARE in leyline-fs's default set, so the ledger is
    # right not to carry them — but these flags compile `fuse.rs`, `nfs.rs` and
    # `verified.rs` out, while cargo-mutants parses with syn, never evaluates
    # cfg, and enumerates every mutant inside them anyway. Each then builds in
    # 0s, every test trivially passes, and it is reported MISSED: a phantom
    # survivor on healthy code, indistinguishable in the log from a real missing
    # assertion (bead `ley-line-open-b23c41`).
    #
    # A slice that opts out of default features owes an exclusion for what that
    # choice removes — the exclusions below are not a policy about mount, they
    # are this invocation being consistent with itself. Enabling the features
    # instead would be worse: `ley-line-open-aed167` records that mount ships
    # with no tests at any level, so the phantoms would become REAL survivors
    # and redden every mount PR over a coverage debt this gate did not incur.
    # `verified` is the same shape (`ley-line-open-b6a4dd`).
    #
    # Kept in step by tools/check_mutants_cfg_coverage.sh, which fails if a
    # cfg-gated module is neither enabled by its slice nor excluded here. That
    # guard is load-bearing: enumerating this list BY HAND missed `verified.rs`.
    run_slice lib --package leyline-fs --test-workspace=false \
        --no-default-features --features cdc,splice,validate \
        --exclude 'll-open/fs/src/fuse.rs' \
        --exclude 'll-open/fs/src/nfs.rs' \
        --exclude 'll-open/fs/src/verified.rs'
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
