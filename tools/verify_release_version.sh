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
grep -q "ley-line-open:$version" server.json
grep -q "^## \\[$version\\]" CHANGELOG.md
grep -q "ley-line-open:$version" README.md
grep -q "clients/go/leyline-schema/v$version" README.md
grep -Fq "| LLO version | v$version |" docs/ARCHITECTURE.md
grep -q 'daemon/wire' clients/go/leyline-schema/README.md
grep -q "Apache License" clients/go/leyline-schema/LICENSE

echo "binary, schema, metadata, docs, and license agree on v$version"
