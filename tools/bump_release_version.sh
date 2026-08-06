#!/bin/sh
set -eu

# Bump every version-carrying file for a new binary release, in place, in the
# current working tree. Does NOT commit, tag, or touch server.json (that is
# generated — regenerate it separately with
# `cargo run -p server-json-gen --quiet > ../server.json` from rs/, after
# this script runs, since BINARY_VERSION is env!("CARGO_PKG_VERSION") at
# compile time and must see the bumped Cargo.toml first).
#
# Grew out of doing this by hand twice (v0.18.0, v0.18.1) with a throwaway
# script each time. Every gap found the hard way at v0.18.0 is fixed here
# permanently instead of relying on remembering it next release:
#
#   - 7 of 27 crate Cargo.tomls carry `version = "X"` twice over: once for
#     their own [package], once per internal path-dependency pin to a
#     sibling crate at the same lockstep version. Both move together.
#   - leyline-public-schema's exact-pin `version = "=X"` on
#     leyline-schema-spec doesn't match the plain pattern above.
#   - version_handshake_blackbox_test.rs pins the release as two literal
#     assertions, outside every version-manifest list — confirmed against
#     the v0.17.0 release PR that this file is touched by hand every
#     release regardless.
#
# SCHEMA_VERSION (ley-line-open-83903c): intentionally
# independent from BINARY_VERSION per the doc comment on the const itself —
# "bump only when the public Cap'n Proto/schema surface changes and a
# matching clients/go/leyline-schema/vX.Y.Z tag is published. Private storage
# changes do not move it." tools/tag_release.sh already has the machinery for
# this (it branches on schema_version == version vs not, and refuses to tag
# if a binary-only release actually touched clients/go/leyline-schema without
# bumping SCHEMA_VERSION to match) — what was missing was a bump step that
# didn't defeat it by force-bumping SCHEMA_VERSION on every release
# regardless. This script checks the same thing tag_release.sh checks at tag
# time — whether clients/go/leyline-schema drifted since the schema's own
# last tag — and only bumps SCHEMA_VERSION when it did. Getting this
# heuristic imperfectly right is safe: tag_release.sh is the real gate and
# will refuse to tag if this script's guess disagrees with what actually
# changed.
if [ "$#" -ne 1 ]; then
    echo "usage: $0 <new-version-without-v>" >&2
    exit 2
fi

new_version=$1
script_root=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
repo_root=${REPO_ROOT:-$(CDPATH='' cd -- "$script_root/.." && pwd)}
git_bin=${GIT_BIN:-git}
# shellcheck source=tools/release_common.sh
. "$script_root/release_common.sh"

release_validate_version "$new_version"
cd "$repo_root"

old_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' rs/ll-open/cli/Cargo.toml | head -n1)
if [ -z "$old_version" ]; then
    echo "could not read current version from rs/ll-open/cli/Cargo.toml" >&2
    exit 1
fi
if [ "$old_version" = "$new_version" ]; then
    echo "already at $new_version" >&2
    exit 1
fi
echo "bumping $old_version -> $new_version" >&2

# Every crate manifest in the canonical list, ALL occurrences per file.
while IFS= read -r manifest; do
    [ -n "$manifest" ] || continue
    if ! grep -q "version = \"$old_version\"" "$manifest"; then
        echo "$manifest: no version = \"$old_version\" found" >&2
        exit 1
    fi
    sed -i.bak "s/version = \"$old_version\"/version = \"$new_version\"/g" "$manifest"
    rm -f "$manifest.bak"
    echo "bumped $manifest" >&2
done <tools/release-version-manifests.txt

# The exact-pin straggler.
public_schema=rs/ll-core/public-schema/Cargo.toml
if grep -q "version = \"=$old_version\"" "$public_schema"; then
    sed -i.bak "s/version = \"=$old_version\"/version = \"=$new_version\"/" "$public_schema"
    rm -f "$public_schema.bak"
    echo "bumped $public_schema (exact-pin straggler)" >&2
fi

# version_handshake_blackbox_test.rs — verified at each of v0.18.0/v0.18.1
# that the current release version appears in this file ONLY as these two
# assertions (no COMPAT_MIN or other version-shaped literal to accidentally
# catch), so a scoped literal replace is safe here.
handshake_test=rs/ll-open/cli-lib/tests/version_handshake_blackbox_test.rs
if grep -q "\"$old_version\"" "$handshake_test"; then
    sed -i.bak "s/\"$old_version\"/\"$new_version\"/g" "$handshake_test"
    rm -f "$handshake_test.bak"
    echo "bumped $handshake_test" >&2
fi

# BINARY_VERSION is env!("CARGO_PKG_VERSION") — nothing to bump here, it
# follows cli-lib's Cargo.toml automatically. SCHEMA_VERSION is the one
# hand-maintained constant in this file, and it moves conditionally.
version_rs=rs/ll-open/cli-lib/src/daemon/version.rs
old_schema_version=$(sed -nE 's/^pub const SCHEMA_VERSION: &str = "([^"]+)";/\1/p' "$version_rs")
if [ -z "$old_schema_version" ]; then
    echo "could not read current SCHEMA_VERSION from $version_rs" >&2
    exit 1
fi

old_schema_tag="clients/go/leyline-schema/v$old_schema_version"
schema_changed=0
if ! "$git_bin" rev-parse -q --verify "refs/tags/$old_schema_tag" >/dev/null 2>&1; then
    "$git_bin" fetch -q origin "refs/tags/$old_schema_tag:refs/tags/$old_schema_tag" 2>/dev/null || true
fi
if "$git_bin" rev-parse -q --verify "refs/tags/$old_schema_tag" >/dev/null 2>&1; then
    if ! "$git_bin" diff --quiet "$old_schema_tag" -- clients/go/leyline-schema; then
        schema_changed=1
    fi
else
    echo "warning: schema tag $old_schema_tag not found even after fetch — bumping SCHEMA_VERSION to be safe" >&2
    schema_changed=1
fi

if [ "$schema_changed" -eq 1 ]; then
    sed -i.bak "s/pub const SCHEMA_VERSION: \&str = \"$old_schema_version\";/pub const SCHEMA_VERSION: \&str = \"$new_version\";/" "$version_rs"
    rm -f "$version_rs.bak"
    echo "bumped SCHEMA_VERSION $old_schema_version -> $new_version (clients/go/leyline-schema changed since $old_schema_tag)" >&2
else
    echo "SCHEMA_VERSION left at $old_schema_version — clients/go/leyline-schema unchanged since $old_schema_tag" >&2
fi

# compatibility.json's two fields track BINARY_VERSION and SCHEMA_VERSION
# independently, matching whatever the two decisions above actually produced
# — NOT both forced to new_version.
new_schema_version=$(sed -nE 's/^pub const SCHEMA_VERSION: &str = "([^"]+)";/\1/p' "$version_rs")
compat=compatibility.json
sed -i.bak \
    -e "s/\"binary_version\": \"$old_version\"/\"binary_version\": \"$new_version\"/" \
    -e "s/\"schema_version\": \"$old_schema_version\"/\"schema_version\": \"$new_schema_version\"/" \
    "$compat"
rm -f "$compat.bak"
echo "bumped $compat" >&2

# README.md — every OTHER version mention in this file has been, at each of
# v0.18.0/v0.18.1, a CURRENT-BINARY reference (download URLs, docker pull
# examples, attestation commands) — no historical "vX shipped Y" prose that a
# blind replace would corrupt. Re-verify that's still true before trusting
# it: fail loudly rather than silently over-replacing if it ever stops being
# true.
readme=README.md
if ! grep -q "v$old_version" "$readme"; then
    echo "$readme: no v$old_version found — README's current-release-only assumption may no longer hold, check by hand" >&2
    exit 1
fi
sed -i.bak "s/v$old_version/v$new_version/g" "$readme"
rm -f "$readme.bak"

# The ONE exception to "every mention tracks binary version": the schema
# client's own tag. Caught by testing this script for real against a
# genuinely decoupled release — the blind replace above happily rewrote this
# to the new BINARY version, which is wrong exactly when SCHEMA_VERSION did
# not move, and produces a README pointing at a Go-module tag that will never
# exist. Corrected unconditionally to whatever SCHEMA_VERSION actually ended
# up as, regardless of what the blind replace above did to this one line.
sed -i.bak \
    "s#\`clients/go/leyline-schema/v[0-9.]*\`#\`clients/go/leyline-schema/v$new_schema_version\`#" \
    "$readme"
rm -f "$readme.bak"
echo "bumped $readme" >&2

# docs/ARCHITECTURE.md — ONLY the three CURRENCY claims verify_release_version.sh
# checks. This file legitimately contains historical version prose ("v0.7.2
# shipped X") that must NOT be touched.
arch=docs/ARCHITECTURE.md
sed -i.bak \
    -e "s/| LLO version | v$old_version |/| LLO version | v$new_version |/" \
    -e "s/produces \`ley-line-open:v$old_version\`/produces \`ley-line-open:v$new_version\`/" \
    -e "s/to \`ghcr.io\/agentic-research\/ley-line-open:v$old_version\` with a/to \`ghcr.io\/agentic-research\/ley-line-open:v$new_version\` with a/" \
    -e "s/v$old_version is its tested compatibility point/v$new_version is its tested compatibility point/" \
    "$arch"
rm -f "$arch.bak"
echo "bumped $arch (3 currency claims)" >&2

cat >&2 <<EOF

Not done by this script — do these next:
  1. Regenerate server.json (from rs/):
       cargo run -p server-json-gen --quiet > ../server.json
  2. Write the CHANGELOG entry: convert ## [Unreleased] to
     ## [$new_version] — <date>, with a real summary of what shipped.
  3. cargo build (or any cargo command) to refresh rs/Cargo.lock.
  4. bash tools/verify_release_version.sh $new_version
EOF
