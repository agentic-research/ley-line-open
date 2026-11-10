#!/bin/sh
# Fixture test for the public schema surface (bead ley-line-open-a4946a).
#
# tools/bump_release_version.sh decides whether SCHEMA_VERSION moves, and
# tools/verify_release_version.sh (and tools/tag_release.sh) refuse a release
# whose SCHEMA_VERSION did not move although the surface changed. Before this
# bead the bump script looked at clients/go/leyline-schema only while the
# verifier also looked at the rs/ll-core/schema-spec IDL and vectors, so a
# release that changed only a schema-spec vector (v0.27.0, v0.28.0) was bumped
# into a state the verifier refused. All three now read one definition,
# release_schema_surface_unchanged in tools/release_common.sh.
#
# A copy of the files the bump and the verifier read becomes a git
# repository whose base commit carries the schema tag for the current
# SCHEMA_VERSION. Each case commits one change, runs the real bump, stands in
# for the server.json regeneration, and runs the real verifier:
#   - a schema-spec vector change raises SCHEMA_VERSION and verifies;
#   - a schema-spec IDL change raises SCHEMA_VERSION and verifies;
#   - a clients/go/leyline-schema change raises SCHEMA_VERSION and verifies;
#   - a change outside the surface leaves SCHEMA_VERSION and verifies;
#   - a surface change with SCHEMA_VERSION held back is refused.
set -eu

GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/dev/null
export GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0

repo=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/llo-schema-surface.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

git_quiet() {
    git -c user.name=fixture -c user.email=fixture@example.invalid \
        -c init.defaultBranch=main -c commit.gpgsign=false \
        -c tag.gpgsign=false "$@" >/dev/null
}

vector=rs/ll-core/schema-spec/sigma/v1/test-vectors/constructions.json
idl=rs/ll-core/schema-spec/execution/v1/execution.capnp
go_file=clients/go/leyline-schema/go.mod
outside=rs/ll-core/schema-spec/README.md
version_rs=rs/ll-open/cli-lib/src/daemon/version.rs

base="$scratch/base"
mkdir -p "$base"
copy() {
    mkdir -p "$base/$(dirname "$1")"
    cp -f "$repo/$1" "$base/$1"
}
for file in CHANGELOG.md README.md compatibility.json server.json \
    docs/ARCHITECTURE.md changes/README.md "$version_rs" \
    clients/go/leyline-schema/README.md clients/go/leyline-schema/LICENSE \
    "$go_file" "$vector" "$idl" "$outside" \
    tools/bump_release_version.sh tools/verify_release_version.sh \
    tools/release_common.sh tools/changelog_fragments.sh \
    tools/release-version-manifests.txt; do
    copy "$file"
done
while IFS= read -r manifest; do
    [ -n "$manifest" ] && copy "$manifest"
done <"$repo/tools/release-version-manifests.txt"
for pending in "$base"/changes/*.md; do
    [ "$(basename "$pending")" = README.md ] || rm -f "$pending"
done
cat >"$base/changes/ley-line-open-ffffff.md" <<'EOF'
Section: Fixed

- **Fixture entry** (bead `ley-line-open-ffffff`).
EOF

schema_of() {
    sed -nE 's/^pub const SCHEMA_VERSION: &str = "([^"]+)";/\1/p' "$1/$version_rs"
}
old_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$base/rs/ll-open/cli/Cargo.toml" | head -n1)
old_schema=$(schema_of "$base")
new_version=$(printf '%s\n' "$old_version" | awk -F. '{ printf "%d.%d.%d", $1, $2 + 1, 0 }')

# The base is a release whose binary and schema both sit at their tags:
# the published schema tag names this exact tree.
git_quiet -C "$base" init
git_quiet -C "$base" add -A
git_quiet -C "$base" commit -m base
git_quiet -C "$base" tag -a "clients/go/leyline-schema/v$old_schema" -m schema

# release <case> <file>: clone the base, change <file>, commit, bump, stand
# in for the server.json regeneration, and leave the clone at $scratch/<case>.
release() {
    clone="$scratch/$1"
    git_quiet clone -q "$base" "$clone"
    printf '\n' >>"$clone/$2"
    git_quiet -C "$clone" commit -am "change $2"
    if ! REPO_ROOT="$clone" GIT_BIN=git RELEASE_DATE=2000-01-02 \
        sh "$clone/tools/bump_release_version.sh" "$new_version" \
        >"$scratch/$1.bump.log" 2>&1
    then
        cat "$scratch/$1.bump.log" >&2
        fail "$1: bump_release_version.sh failed"
    fi
    sed -i.bak \
        -e "s/\"version\": \"$old_version\"/\"version\": \"$new_version\"/" \
        -e "s/\"version\": \"v$old_version\"/\"version\": \"v$new_version\"/" \
        "$clone/server.json"
    rm -f "$clone/server.json.bak"
}

verify() {
    (cd "$scratch/$1" && sh tools/verify_release_version.sh "$new_version") \
        >"$scratch/$1.verify.log" 2>&1
}

expect_schema() {
    actual=$(schema_of "$scratch/$1")
    [ "$actual" = "$2" ] ||
        fail "$1: SCHEMA_VERSION is $actual after the bump, expected $2"
    if ! verify "$1"; then
        cat "$scratch/$1.verify.log" >&2
        fail "$1: verify_release_version.sh refused the bumped tree"
    fi
}

release vector "$vector"
expect_schema vector "$new_version"
release idl "$idl"
expect_schema idl "$new_version"
release go "$go_file"
expect_schema go "$new_version"
release outside "$outside"
expect_schema outside "$old_schema"

# A surface change released under the old SCHEMA_VERSION is refused: undo
# the bump's SCHEMA_VERSION decision on the vector case.
held="$scratch/vector"
sed -i.bak "s/pub const SCHEMA_VERSION: \&str = \"$new_version\";/pub const SCHEMA_VERSION: \&str = \"$old_schema\";/" \
    "$held/$version_rs"
sed -i.bak "s/\"schema_version\": \"$new_version\"/\"schema_version\": \"$old_schema\"/" \
    "$held/compatibility.json"
sed -i.bak "s#clients/go/leyline-schema/v$new_version#clients/go/leyline-schema/v$old_schema#" \
    "$held/README.md"
rm -f "$held/$version_rs.bak" "$held/compatibility.json.bak" "$held/README.md.bak"
if verify vector; then
    fail "verify_release_version.sh accepted a schema-spec vector change under the old SCHEMA_VERSION"
fi
grep -q "public schema surface" "$scratch/vector.verify.log" ||
    fail "the held-back refusal did not name the public schema surface: $(cat "$scratch/vector.verify.log")"

echo "schema surface: a vector, IDL or Go change raises SCHEMA_VERSION and verifies; a change outside leaves it and verifies; a held-back surface change is refused"
