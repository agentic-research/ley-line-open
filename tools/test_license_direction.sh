#!/bin/sh
# Fixture test for tools/check_license_direction.sh (bead ley-line-open-4392a2).
#
# Each case is a throwaway cargo workspace of path-only crates, so cargo
# resolves it offline. Every inverted case must fail AND name the offending
# edge; the allowed shapes and the shipped tree must pass. A check that passes
# everything would pass the shipped tree too, so the red cases are what make
# the green one evidence.
set -eu

repo=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
check="$repo/tools/check_license_direction.sh"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/llo-license-direction.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

# new_workspace <case>: start an empty fixture workspace at $scratch/<case>/rs.
new_workspace() {
    ws="$scratch/$1/rs"
    mkdir -p "$ws"
    printf '%s\n' '[workspace]' 'members = ["*"]' 'exclude = ["target"]' \
        'resolver = "2"' '' '[workspace.package]' 'license = "Apache-2.0"' \
        > "$ws/Cargo.toml"
}

# crate <name> <license-line> [extra manifest lines...]: add a member crate.
crate() {
    name=$1
    license_line=$2
    shift 2
    mkdir -p "$ws/$name/src"
    : > "$ws/$name/src/lib.rs"
    {
        printf '%s\n' '[package]' "name = \"$name\"" 'version = "0.0.0"' \
            'edition = "2021"'
        if [ -n "$license_line" ]; then printf '%s\n' "$license_line"; fi
        for line in "$@"; do printf '%s\n' "$line"; done
    } > "$ws/$name/Cargo.toml"
}

apache='license = "Apache-2.0"'
agpl='license = "AGPL-3.0-or-later"'

lock() {
    cargo generate-lockfile --offline --quiet --manifest-path "$ws/Cargo.toml"
}

# expect_fail <case> <needle>: the check must exit non-zero and print needle.
expect_fail() {
    lock
    if "$check" "$scratch/$1" > "$scratch/$1.out" 2>&1; then
        printf 'license direction: inverted fixture %s unexpectedly passed\n' "$1" >&2
        cat "$scratch/$1.out" >&2
        exit 1
    fi
    grep -F -- "$2" "$scratch/$1.out" > /dev/null || {
        printf 'license direction: fixture %s failed without naming: %s\n' "$1" "$2" >&2
        cat "$scratch/$1.out" >&2
        exit 1
    }
}

expect_pass() {
    lock
    "$check" "$scratch/$1" > "$scratch/$1.out" 2>&1 || {
        printf 'license direction: allowed fixture %s failed\n' "$1" >&2
        cat "$scratch/$1.out" >&2
        exit 1
    }
}

# Allowed: AGPL depends on Apache (the direction the carveout permits), an
# Apache crate dev-depends on AGPL (dev edges never reach a consumer), and a
# member inherits Apache-2.0 through `license.workspace = true`.
new_workspace allowed
crate contract "$apache"
crate inherited 'license.workspace = true'
crate engine "$agpl" '[dependencies]' 'contract = { path = "../contract" }' \
    'inherited = { path = "../inherited" }'
crate tested "$apache" '[dev-dependencies]' 'engine = { path = "../engine" }'
expect_pass allowed

# Inverted, direct: the Apache contract crate depends on the AGPL engine.
new_workspace direct
crate engine "$agpl"
crate contract "$apache" '[dependencies]' 'engine = { path = "../engine" }'
expect_fail direct 'VIOLATION contract (Apache-2.0) reaches engine@0.0.0 (AGPL-3.0-or-later): contract@0.0.0 -> engine@0.0.0'

# Inverted, transitive: Apache -> Apache -> AGPL. The intermediate crate is
# clean on its own declaration; only the closure exposes the edge.
new_workspace transitive
crate engine "$agpl"
crate middle "$apache" '[dependencies]' 'engine = { path = "../engine" }'
crate contract "$apache" '[dependencies]' 'middle = { path = "../middle" }'
expect_fail transitive 'contract@0.0.0 -> middle@0.0.0 -> engine@0.0.0'

# Inverted, build edge: build scripts link into the crate's build, so a build
# dependency counts.
new_workspace build
crate engine "$agpl"
crate contract "$apache" '[build-dependencies]' 'engine = { path = "../engine" }'
expect_fail build 'VIOLATION contract (Apache-2.0) reaches engine@0.0.0'

# Inverted, behind a feature: an optional dependency still ships to anyone who
# enables the feature.
new_workspace optional
crate engine "$agpl"
crate contract "$apache" '[dependencies]' \
    'engine = { path = "../engine", optional = true }' \
    '[features]' 'impl = ["dep:engine"]'
expect_fail optional 'VIOLATION contract (Apache-2.0) reaches engine@0.0.0'

# Undeclared: a member with no license field cannot be placed on either side.
new_workspace undeclared
crate contract "$apache"
crate stray ''
expect_fail undeclared 'UNDECLARED stray'

# Undeclared: a license outside the two the carveout names.
new_workspace foreign
crate stray 'license = "MIT"'
expect_fail foreign 'UNDECLARED stray'

# The shipped tree satisfies the carveout.
"$check" "$repo"
echo "license direction fixtures passed"
