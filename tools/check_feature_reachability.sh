#!/bin/sh
# Feature reachability — bead ley-line-open-2607d2, instance 3.
#
# The feature lattice is an undeclared decomposition: several configurations
# are individually buildable, nothing states which are real, so a feature can be
# tested-but-never-shipped or shipped-but-never-tested and neither is visible.
#
# `leyline-fs/splice` was the first kind. Enabled only by the Taskfile's test
# targets, reachable from no shipping configuration, therefore compiled out of
# every binary — `flush_node` degraded to `let _ = id; Ok(())`, a silent
# success, so mount writes never reprojected while a test that enabled the
# feature proved they did (ley-line-open-918a75).
#
# The rule, one layer up from F10c's "assert the mechanism fired":
#
#     Every feature a crate declares must be reachable from a declared
#     shipping configuration, or be explicitly recorded as not shipping.
#
# Reachability is CARGO'S ANSWER, not ours. An earlier cut of this script
# grepped manifests for `crate/feature` edges and missed `features = [...]`
# arrays on dependency declarations, giving a 97% false-positive rate. A gate
# that cries wolf gets baselined into silence, which is the failure this whole
# bead is about. `cargo tree -f '{p} FEATURES={f}'` resolves it exactly.
set -eu

root=${1:-.}
# Absolute repo root, captured BEFORE the cd: claim 2 shells out to `task`, whose
# Taskfile lives here, not in rs/.
repo_root=$(cd "$root" && pwd)
cd "$root/rs"
failed=0

# The declared shipping configurations — what `task release`, `release:mount`,
# `release:all`, and `release:full` actually build. Adding one is deliberate.
# A FAILED resolve must abort, never degrade into "nothing is reachable".
#
# The first cut ran `cargo tree ... 2>/dev/null` inside a command substitution
# and kept only stdout. When a resolve failed — lock contention against a
# concurrent `task ci` cargo job is the observed trigger — its rows silently
# vanished and every feature that only that config enables was reported as
# unreachable. One such run produced 34 findings, all false, while the actual
# fault (cargo did not answer) was never printed. That is precisely the
# silent-success class this gate exists to catch, so it may not be tolerated
# HERE of all places.
#
# `exit` cannot live inside the resolve helper: these run inside `$( ... )`, and
# exiting a subshell does not stop the parent. Status is therefore checked in
# the main shell, before any parsing.
raw=""
for cfg in "" "--features mount" \
           "--no-default-features --features all" \
           "--no-default-features --features full"; do
    # `$?` after `if ! cmd` is the NEGATION's status (always 0 in the taken
    # branch), not cargo's — so capture it in the else arm, where it is real.
    # shellcheck disable=SC2086
    if out=$(cargo tree -p leyline-cli $cfg -f '{p} FEATURES={f}' 2>&1); then
        :
    else
        status=$?
        printf 'feature-reachability: `cargo tree -p leyline-cli %s` FAILED (exit %s).\n' \
            "${cfg:-<default>}" "$status" >&2
        printf 'Refusing to report reachability from an incomplete resolve — the\n' >&2
        printf 'findings would be fabricated. cargo said:\n\n' >&2
        printf '%s\n' "$out" | head -5 >&2
        exit 1
    fi
    raw="$raw
$out"
done

reachable=$(
    printf '%s\n' "$raw" \
      | sed -n 's/.*(\(.*\)) FEATURES=\(.*\)/\1 \2/p' \
      | awk '{ dir=$1; n=split($2, f, ","); for (i=1;i<=n;i++) if (f[i] != "") print dir "/" f[i] }' \
      | sort -u
)

if [ -z "$reachable" ]; then
    echo "feature-reachability: cargo resolved nothing — refusing to pass vacuously" >&2
    exit 1
fi

# Positive control. Non-emptiness is too weak: a PARTIAL resolve still clears it
# and still fabricates findings (that is the 34-finding run). `leyline-ts/rust`
# is enabled by the default config — the CLI cannot parse Rust without it — so
# its absence means the resolve is incomplete, not that Rust stopped shipping.
# Same rule the gate imposes on everyone else: assert the mechanism fired.
if ! printf '%s\n' "$reachable" | grep -q '/ll-open/ts/rust$'; then
    echo "feature-reachability: resolve is incomplete — the default config must" >&2
    echo "enable ll-open/ts/rust and did not. Not reporting findings from it." >&2
    exit 1
fi

# Features that deliberately do not ship. Each needs a bead. Same pattern as
# docs/smell-baseline.json: known state is recorded so NEW drift fails, not the
# existing backlog.
#
#   <crate-dir>/<feature>   # bead — why
# The rows and their prose live in tools/feature-ledger.txt, which
# tools/mutants_diff.sh reads too — one file, so the two gates cannot drift
# the way their private copies did (bead ley-line-open-cb1e29). A missing or
# unparseable ledger fails closed here: an empty exemption list would fail
# every not-shipping feature loudly, but the earlier we say WHY, the better.
ledger="$repo_root/tools/feature-ledger.txt"
if [ ! -f "$ledger" ]; then
    echo "feature-reachability: $ledger is missing — there is no not-shipping" >&2
    echo "ledger to consult, and every verdict below would be fabricated." >&2
    exit 1
fi
not_shipping=$(awk '!/^[[:space:]]*(#|$)/ && $3 == "not-shipping" { print $1 "/" $2 }' "$ledger")
if [ -z "$not_shipping" ]; then
    echo "feature-reachability: parsed zero not-shipping rows from $ledger —" >&2
    echo "the ledger regressed or the parse broke. Refusing to guess which." >&2
    exit 1
fi
ships_untested=$(awk '!/^[[:space:]]*(#|$)/ && $3 == "ships-untested" { print $1 "/" $2 }' "$ledger")

# Both sides of the grep -qx below must live in the same path universe.
# cargo prints PHYSICAL manifest paths, while a plain `pwd` reports the
# LOGICAL one — under the ~/github → ~/remotes symlink those differ, every
# comparison missed, and all 40+ declared features false-failed as
# unreachable (ley-line-open-db0920). `pwd -P` canonicalizes; the control
# below asserts the universes actually coincide instead of trusting this.
cli_dir=$(cd ll-open/cli && pwd -P)
if ! printf '%s\n' "$reachable" | grep -q "^$cli_dir/"; then
    echo "feature-reachability: the CLI crate dir ($cli_dir) appears nowhere in" >&2
    echo "cargo's reachable set — the path comparison is comparing different" >&2
    echo "path universes and every verdict below would be fabricated." >&2
    exit 1
fi

# A not-shipping row whose feature IS reachable is a stale ledger entry — the
# feature started shipping and the record did not move. Before this check, a
# stale row passed silently: the reachability loop below consults the ledger
# only for features cargo did NOT resolve, so nothing ever read a stale one.
for entry in $not_shipping; do
    dir=${entry%/*}
    feature=${entry##*/}
    [ -d "$dir" ] || continue # claim 3 reports the missing-manifest case
    abs=$(cd "$dir" && pwd -P)
    if printf '%s\n' "$reachable" | grep -qx "$abs/$feature"; then
        printf 'feature-ledger STALE: %s is recorded not-shipping, but a shipping configuration enables it\n' \
            "$entry" >&2
        printf '                      move the row to ships-optin (or drop it) in tools/feature-ledger.txt\n' >&2
        failed=1
    fi
done

for manifest in $(find . -name Cargo.toml -not -path '*/target/*' -not -path '*worktree*' | sort); do
    dir=$(cd "$(dirname "$manifest")" && pwd -P)
    for feature in $(sed -n '/^\[features\]/,/^\[/p' "$manifest" \
                     | grep -E '^[a-z][a-z0-9_-]* *=' | cut -d= -f1 | tr -d ' '); do
        [ "$feature" = "default" ] && continue
        printf '%s\n' "$reachable" | grep -qx "$dir/$feature" && continue

        short=$(printf '%s' "$dir" | sed 's|.*/rs/||')
        printf '%s' "$not_shipping" | grep -qx "$short/$feature" && continue

        printf 'feature-reachability FAILED: %s/%s is declared but no shipping configuration enables it\n' \
            "$short" "$feature" >&2
        failed=1
    done
done

if [ "$failed" -ne 0 ]; then
    printf '\nA feature no build enables is compiled out everywhere. A test that enables it\n' >&2
    printf 'proves a capability the shipped binary does not have. Either wire it into a\n' >&2
    printf 'config in rs/ll-open/cli{,-lib}/Cargo.toml, or record it as not-shipping in\n' >&2
    printf 'tools/feature-ledger.txt with a bead saying why.\n' >&2
    exit 1
fi

# ── Claim 2: every SHIPPING CLI feature must be exercised by some test ───────
#
# Reachability says a feature lands in a binary. It says nothing about whether
# anything ever ran it. The README documents three install paths — `task
# install`, `install:full`, `install:full+mount` — so every feature they enable
# reaches users, and a feature no test compiles is shipped-but-unexercised.
#
# Scope: the CLI-level feature names, which is what "a feature" means to
# someone installing this. Crate-internal features are covered by claim 1.
cli_manifest="ll-open/cli-lib/Cargo.toml"
cli_defaults=$(sed -n 's/^default = \[\(.*\)\]/\1/p' "$cli_manifest" | tr -d '" ' | tr ',' ' ')
cli_features=$(sed -n '/^\[features\]/,/^\[/p' "$cli_manifest" \
                | grep -E '^[a-z][a-z0-9_-]* *=' | cut -d= -f1 | tr -d ' ' \
                | grep -v '^default$')

# Features named on a `cargo test` line owned by a target the CI CHAIN
# actually invokes. Counting every `cargo test` line would accept a target that
# exists and never runs — the same unwired failure this gate exists to catch.
#
# RESOLVE INCLUDES OURSELVES, WITH NO SUBPROCESS.
#
# Three designs have been tried here. The first grepped the root Taskfile for
# both the chain and the `cargo test` lines; it broke the moment tasks moved to
# crate-scoped Taskfiles, because it asserted a LAYOUT rather than a contract.
# The second shelled out to `task --summary`, which resolves includes properly —
# and passed locally but failed in GitHub CI with an empty chain, for reasons
# the script could not report because it had discarded task's stderr.
#
# The lesson is not "text parsing bad". It is that a gate must not depend on
# behaviour that varies between the developer's machine and CI. `task --summary`
# is a nested invocation of the very runner executing this script; that is a
# runtime dependency with an environment-shaped failure mode.
#
# So: parse, but resolve includes EXACTLY, using the two facts the root Taskfile
# states declaratively.
#   1. The ci CHAIN is still in the root file — only task DEFINITIONS moved.
#   2. `includes:` is an explicit prefix -> taskfile map.
# A chain entry `fs:test:cdc` therefore means "task `test:cdc` in the file
# mapped by prefix `fs`". Deterministic, no subprocess, identical everywhere.

taskfile_root="$repo_root/Taskfile.yml"

# prefix -> taskfile path, from the `includes:` block.
include_map=$(awk '
    /^includes:/ { inc = 1; next }
    /^[a-z]/     { inc = 0 }
    inc && /^  [a-z][a-z0-9_-]*:/ { p = $1; sub(/:$/, "", p) }
    inc && /taskfile:/            { print p, $2 }
' "$taskfile_root")

ci_tasks=$(sed -n '/^  ci:/,/^  [a-z][a-z0-9:_-]*:$/p' "$taskfile_root" \
            | grep -E '^[[:space:]]+- task:' | sed 's/.*task: //')
if [ -z "$ci_tasks" ]; then
    echo "feature-coverage: could not read the ci chain — refusing to pass vacuously" >&2
    exit 1
fi

# Emit every `cargo test` line belonging to a ci-invoked task, following
# includes. An unmapped prefix (e.g. `lint:doc-claims`, where `lint` is a naming
# convention and not an include) resolves to the root file, which is correct.
tested_explicit=$(
    printf '%s\n' "$ci_tasks" | while IFS= read -r t; do
        [ -n "$t" ] || continue
        prefix=${t%%:*}
        file=$(printf '%s\n' "$include_map" | awk -v p="$prefix" '$1 == p { print $2 }')
        if [ -n "$file" ]; then
            scan="$repo_root/$file"
            local_name=${t#*:}
        else
            scan="$taskfile_root"
            local_name="$t"
        fi
        [ -f "$scan" ] || continue
        # The task's own block: from `  <name>:` to the next two-space key.
        awk -v n="  $local_name:" '
            $0 == n            { inblock = 1; next }
            /^  [a-z][a-z0-9:_-]*:/ { inblock = 0 }
            inblock && /cargo test/ { print }
        ' "$scan"
    done \
    | grep -oE '\-\-features [a-z0-9_,-]+' | sed 's/--features //' | tr ',' '\n' | sort -u
)

# Positive control. An empty result is indistinguishable from "no feature is
# tested", which would silently pass every check below — the vacuous-pass shape
# this whole gate exists to prevent. `cdc` is named on a ci-invoked `cargo test`
# line; its absence means task resolution broke, not that coverage changed.
if ! printf '%s\n' "$tested_explicit" | grep -qx 'cdc'; then
    echo "feature-coverage: task resolution is incomplete — expected the ci chain" >&2
    echo "to name --features cdc and it did not. Not reporting coverage from it." >&2
    exit 1
fi

for feature in $cli_features; do
    # In the CLI default set => compiled by `cargo test -p leyline-cli-lib`.
    printf '%s\n' $cli_defaults | grep -qx "$feature" && continue
    printf '%s\n' "$tested_explicit" | grep -qx "$feature" && continue

    # Coverage exemptions — shipping features with no ci-invoked test, the
    # `ships-untested` rows of tools/feature-ledger.txt, each with a bead and
    # its why. Separate from the not-shipping rows: these DO ship, they are
    # just unexercised, which is the worse of the two states.
    printf '%s\n' "$ships_untested" | grep -qx "ll-open/cli-lib/$feature" && continue

    printf 'feature-coverage FAILED: cli-lib/%s ships but no ci-invoked test enables it\n' \
        "$feature" >&2
    failed=1
done

if [ "$failed" -ne 0 ]; then
    printf '\nA shipping feature no test compiles has never been run in the shape users get.\n' >&2
    printf 'Add a test target that enables it, or stop shipping it.\n' >&2
    exit 1
fi
# ── Claim 3: every NOT-SHIPPING feature must still COMPILE, tests included ──
#
# Claim 1 lets a feature opt out of shipping. Nothing then compiled it, so the
# ledger above quietly became a list of code that no build in this repo touches
# — and unbuilt code does not stay correct just because it stopped being
# interesting.
#
# `engine-witchcraft` spent five weeks proving it. PR #210's
# `std::sync::{Mutex,RwLock}` -> parking_lot refactor rewrote the mutex type and
# left six `.map_err()` calls behind on a guard that no longer returns a
# `Result`. The module — 444 lines carrying five tests — did not compile AT ALL,
# and no gate said so, because every gate skipped it. A workspace-wide refactor
# is exactly the change that reaches this code, and exactly the one nothing
# verified against it.
#
# It also blinded the mutation gate: `tools/mutants_diff.sh` mutates gated
# source whether or not the gate is on, so all 28 of witchcraft's mutants
# reported MISSED while testing nothing — a false survivor that reads as a
# missing test.
#
# Not-shipping describes what USERS get. It is not permission for the tree to
# stop building. `--lib --no-run` compiles the test targets too, which is what
# catches a break confined to `#[cfg(test)]`.
for entry in $not_shipping; do
    dir=${entry%/*}
    feature=${entry##*/}
    manifest="$dir/Cargo.toml"
    if [ ! -f "$manifest" ]; then
        printf 'feature-rot FAILED: %s names %s, which does not exist — stale ledger entry\n' \
            "$entry" "$manifest" >&2
        failed=1
        continue
    fi
    pkg=$(sed -n 's/^name = "\(.*\)"/\1/p' "$manifest" | head -1)
    if [ -z "$pkg" ]; then
        printf 'feature-rot FAILED: could not read a package name from %s\n' "$manifest" >&2
        failed=1
        continue
    fi
    if ! cargo test --package "$pkg" --features "$feature" --lib --no-run >/dev/null 2>&1; then
        printf 'feature-rot FAILED: %s/%s is on the not-shipping ledger and no longer COMPILES\n' \
            "$dir" "$feature" >&2
        printf '                    reproduce: cargo test --package %s --features %s --lib\n' \
            "$pkg" "$feature" >&2
        failed=1
    fi
done

if [ "$failed" -ne 0 ]; then
    printf '\nCode behind a not-shipping feature is still in the tree and still swept up by\n' >&2
    printf 'every workspace-wide refactor. Fix it, or delete the feature and its module —\n' >&2
    printf 'those are the two honest options. Leaving it on the ledger is how it rots.\n' >&2
    exit 1
fi

printf 'feature reachability + coverage + not-shipping build verified against the shipping configurations\n'
