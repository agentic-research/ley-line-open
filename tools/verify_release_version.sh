#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$repo_root"
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-release-version.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

if [ "$#" -gt 1 ]; then
    echo "usage: $0 [version-without-v]" >&2
    exit 2
fi

version=${1:-}
if [ -z "$version" ]; then
    version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' \
        rs/ll-open/cli/Cargo.toml | head -n 1)
fi
if ! printf '%s\n' "$version" |
    awk '/^[0-9]+\.[0-9]+\.[0-9]+$/ { found = 1 } END { exit !found }'
then
    echo "invalid release version: $version" >&2
    exit 1
fi

find rs -maxdepth 4 -name Cargo.toml -type f > "$tmp_dir/all-manifests"
awk '$0 != "rs/Cargo.toml" { print }' \
    "$tmp_dir/all-manifests" > "$tmp_dir/release-manifests-raw"
LC_ALL=C sort "$tmp_dir/release-manifests-raw" \
    > "$tmp_dir/release-manifests"
LC_ALL=C sort tools/release-version-manifests.txt \
    > "$tmp_dir/expected-manifests"
diff -u "$tmp_dir/expected-manifests" "$tmp_dir/release-manifests"

while IFS= read -r manifest; do
    package_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' \
        "$manifest" | head -n 1)
    if [ "$package_version" != "$version" ]; then
        echo "$manifest is $package_version; release is $version" >&2
        exit 1
    fi
done < "$tmp_dir/release-manifests"

schema_version=$(sed -nE \
    's/^pub const SCHEMA_VERSION: &str = "([^"]+)";/\1/p' \
    rs/ll-open/cli-lib/src/daemon/version.rs)
# SCHEMA_VERSION is intentionally independent from the binary release
# version (see the doc comment on the const itself) — a binary-only release
# leaves it at whatever it last was. Two valid states, mirroring
# tools/tag_release.sh's own branching at tag time exactly, so this gate
# cannot pass something that script would then refuse to tag:
#   1. schema_version == version: this release also bumps the public schema
#      surface. The new clients/go/leyline-schema/v$version tag is expected
#      to not exist yet — tag_release.sh creates it.
#   2. schema_version != version: binary-only release, reusing an
#      ALREADY-PUBLISHED schema tag. Refuse if that tag was never actually
#      published (a schema_version pointing nowhere is not "unchanged", it's
#      wrong), and refuse if clients/go/leyline-schema drifted from that tag
#      without SCHEMA_VERSION moving to match — the same drift
#      tag_release.sh's own diff --quiet catches at tag time, caught here
#      instead so a release fails fast rather than at the tag step.
if [ "$schema_version" != "$version" ]; then
    old_schema_tag="clients/go/leyline-schema/v$schema_version"
    if ! git rev-parse -q --verify "refs/tags/$old_schema_tag" >/dev/null 2>&1; then
        git fetch -q origin "refs/tags/$old_schema_tag:refs/tags/$old_schema_tag" 2>/dev/null || true
    fi
    if ! git rev-parse -q --verify "refs/tags/$old_schema_tag" >/dev/null 2>&1; then
        echo "SCHEMA_VERSION is $schema_version (binary release is $version), but" \
             "$old_schema_tag is not a published tag — nothing to reuse" >&2
        exit 1
    fi
    if ! git diff --quiet "$old_schema_tag" -- clients/go/leyline-schema; then
        echo "SCHEMA_VERSION is unchanged at $schema_version, but" \
             "clients/go/leyline-schema differs from $old_schema_tag — bump" \
             "SCHEMA_VERSION to $version or revert the schema-client change" >&2
        exit 1
    fi
fi

grep -q "\"binary_version\": \"$version\"" compatibility.json
grep -q "\"schema_version\": \"$schema_version\"" compatibility.json
grep -q "\"version\": \"$version\"" server.json
# The OCI identifier is TAGLESS (cloister ADR-0041, bead `ley-line-open-04300f`).
# This line previously read `grep -q "ley-line-open:$version" server.json`, i.e. it
# proved server.json carried the release version by checking the IMAGE TAG — so the
# ADR-0041 violation was not merely emitted, it was load-bearing for this gate.
# The version itself is already asserted directly, one line above. What is checked
# here is that the identifier is present and correctly tagless.
grep -q '"identifier": "ghcr.io/agentic-research/ley-line-open"' server.json
grep -q "^## \\[$version\\]" CHANGELOG.md
# `v`-prefixed. The image tag must equal server.json's packages[0].version,
# because cloister derives `<identifier>:<version>` from it (ADR-0038) — a
# README documenting an unprefixed pull command would name an image the
# publish job never pushes (`ley-line-open-e44960`).
grep -q "ley-line-open:v$version" README.md
# Tracks schema_version, not the binary release — the README's own text
# names a Go-module tag, and that tag only moves when SCHEMA_VERSION does.
grep -q "clients/go/leyline-schema/v$schema_version" README.md
grep -Fq "| LLO version | v$version |" docs/ARCHITECTURE.md
grep -q 'daemon/wire' clients/go/leyline-schema/README.md
grep -q "Apache License" clients/go/leyline-schema/LICENSE

# The claim below says "docs agree". It did not check that.
#
# This gate greps a handful of exact patterns, so version strings in PROSE went
# unchecked — and at v0.11.1 it printed "docs agree" while README.md still said
# "The current release is `v0.10.4`" and docs/ARCHITECTURE.md still advertised a
# 0.10.4 image and Go-client compatibility point.
#
# The first attempt at a fix banned EVERY earlier version string from the doc
# set. That was wrong and worth recording: docs/ARCHITECTURE.md legitimately
# says "Accepted (shipped v0.5.0)" and "v0.7.2 shipped T1's schema" — true
# statements about the past. A gate that forces the prose it guards to become
# false is worse than the drift it prevents.
#
# So assert the sentences that claim CURRENCY, and leave history alone. Each
# pattern below is a place a reader learns "what is the version NOW".
assert_current() {
    file=$1
    what=$2
    pattern=$3
    if ! grep -Fq "$pattern" "$file"; then
        printf 'stale currency claim: %s should state %s as %s\n' "$file" "$what" "$version" >&2
        printf '  expected to find: %s\n' "$pattern" >&2
        exit 1
    fi
}

assert_current README.md "the current release" "The current release is \`v$version\`"
assert_current docs/ARCHITECTURE.md "the OCI image tag" "produces \`ley-line-open:v$version\`"
assert_current docs/ARCHITECTURE.md "the Go client compatibility point" \
    "v$version is its tested compatibility point"

echo "binary, schema, metadata, docs, and license agree on v$version"
