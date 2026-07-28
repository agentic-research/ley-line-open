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
awk '
  $0 != "rs/Cargo.toml" &&
    $0 != "rs/ll-core/schema-spec/Cargo.toml" { print }
' "$tmp_dir/all-manifests" > "$tmp_dir/release-manifests-raw"
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
test "$schema_version" = "$version"

grep -q "\"binary_version\": \"$version\"" compatibility.json
grep -q "\"schema_version\": \"$version\"" compatibility.json
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
grep -q "clients/go/leyline-schema/v$version" README.md
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
