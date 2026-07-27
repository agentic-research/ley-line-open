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

for manifest in $(find . -name Cargo.toml -not -path '*/target/*' -not -path '*worktree*' | sort); do
    dir=$(cd "$(dirname "$manifest")" && pwd)
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
# actually invokes. Counting every `cargo test` line in the file would accept a
# target that exists and never runs — the same unwired failure this gate exists
# to catch, and one I shipped once already today.
ci_tasks=$(sed -n '/^  ci:/,/^  [a-z][a-z0-9:_-]*:$/p' ../Taskfile.yml \
            | grep -E '^[[:space:]]+- task:' | sed 's/.*task: //' | tr '\n' ' ')
if [ -z "$ci_tasks" ]; then
    echo "feature-coverage: could not read the ci chain — refusing to pass vacuously" >&2
    exit 1
fi
tested_explicit=$(awk -v tasks="$ci_tasks" '
    BEGIN { n = split(tasks, t, /[ \n]+/); for (i = 1; i <= n; i++) if (t[i] != "") in_ci[t[i]] = 1 }
    /^  [a-z][a-z0-9:_-]*:/ { cur = $1; sub(/:$/, "", cur) }
    /cargo test/ && (cur in in_ci) { print }
' ../Taskfile.yml \
    | grep -oE '\-\-features [a-z0-9_,-]+' | sed 's/--features //' | tr ',' '\n' | sort -u)

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
