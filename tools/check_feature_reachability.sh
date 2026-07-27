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
resolve() {
    # $1: extra cargo args describing one shipping config
    # shellcheck disable=SC2086
    cargo tree -p leyline-cli $1 -f '{p} FEATURES={f}' 2>/dev/null \
        | sed -n 's/.*(\(.*\)) FEATURES=\(.*\)/\1 \2/p'
}

reachable=$(
    {
        resolve ""
        resolve "--features mount"
        resolve "--no-default-features --features all"
        resolve "--no-default-features --features full"
    } | awk '{ dir=$1; n=split($2, f, ","); for (i=1;i<=n;i++) if (f[i] != "") print dir "/" f[i] }' \
      | sort -u
)

if [ -z "$reachable" ]; then
    echo "feature-reachability: cargo resolved nothing — refusing to pass vacuously" >&2
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
printf 'feature reachability verified against the shipping configurations (cargo-resolved)\n'
