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
not_shipping="
ll-open/fs/splice
ll-open/fs/verify
ll-core/core/interrupt
ll-core/schema-capnp/regen-fixtures
ll-open/sheaf/test-spy
ll-open/sign/host
ll-open/sign/host-extras
ll-open/text-search/engine-witchcraft
ll-open/ts/pyproject
ll-open/vcs/sqlite
"
# Ledger notes — why each of the above does not ship. Kept as prose beside the
# list because a bare allowlist decays into "things we muted".
#
#   ll-open/fs/splice          ley-line-open-918a75 — SHIPPED DEFECT. Enabled
#                              only by test targets; flush_node is a silent
#                              no-op in every build, so mount writes never
#                              reproject. Remove this line when 918a75 decides.
#   ll-open/fs/verify          ley-line-open-b6a4dd — verify-on-fault arena
#                              serving, deliberately default-OFF: additive API
#                              (VerifiedArena + with_verify_on_fault), nothing
#                              degrades when compiled out, unlike splice. The
#                              flag flips into a shipping config in a later
#                              bead; remove this line then.
#   ll-open/ts/pyproject       ley-line-open-988b93 — NOT a grammar: a 349-line
#                              dependency-graph projection (pyproject.toml ->
#                              mountable /deps tree). Unwired too — nothing
#                              calls project_pyproject. Dead in two ways.
#   ll-core/core/interrupt     opt-in signal handling; consumers select it.
#   schema-capnp/regen-fixtures  dev tooling — regenerates test fixtures.
#   ll-open/sheaf/test-spy     declared test-only in its own manifest comment.
#   ll-open/sign/host{,-extras}  ADR-0019 — interactive HOST signing lives
#                              cloister-side; LLO ships verify-only.
#   text-search/engine-witchcraft  engine selection, not built into the CLI.
#   ll-open/vcs/sqlite         leyline-vcs is not a CLI dependency at all.

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
    printf 'config in rs/ll-open/cli{,-lib}/Cargo.toml, or record it in the not-shipping\n' >&2
    printf 'ledger in this script with a bead saying why.\n' >&2
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

    # Coverage ledger — shipping features with no ci-invoked test, each with a
    # bead. Separate from the not-shipping ledger above: these DO ship, they
    # are just unexercised, which is the worse of the two states.
    #
    #   cli-lib/mount   ley-line-open-aed167 — zero test files gated on it at
    #                   any level. Two BUILD checks exist; neither proves a
    #                   mount serves correct bytes. Compounds 918a75, where
    #                   flush_node is a silent no-op so mount writes never
    #                   reproject. Remove when a round-trip test lands.
    case "$feature" in
        mount) continue ;;
    esac

    printf 'feature-coverage FAILED: cli-lib/%s ships but no ci-invoked test enables it\n' \
        "$feature" >&2
    failed=1
done

if [ "$failed" -ne 0 ]; then
    printf '\nA shipping feature no test compiles has never been run in the shape users get.\n' >&2
    printf 'Add a test target that enables it, or stop shipping it.\n' >&2
    exit 1
fi
printf 'feature reachability + coverage verified against the shipping configurations\n'
