#!/bin/sh
set -eu

verify_sha256_manifest() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$1"
    else
        shasum -a 256 -c "$1"
    fi
}

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <publication-dir> <expected-assets-file>" >&2
    exit 2
fi

publication_dir=$1
expected_assets_file=$2
test -d "$publication_dir"
test -s "$expected_assets_file"
test -s "$publication_dir/SHA256SUMS"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-public-verify.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

expected_assets="$tmp_dir/expected-assets"
expected_assets_raw="$tmp_dir/expected-assets-raw"
manifest_assets="$tmp_dir/manifest-assets"
manifest_assets_raw="$tmp_dir/manifest-assets-raw"
actual_assets="$tmp_dir/actual-assets"
actual_assets_raw="$tmp_dir/actual-assets-raw"

awk '
  NF != 1 || $1 !~ /^[A-Za-z0-9._-]+$/ || $1 == "SHA256SUMS" { exit 1 }
  { print $1 }
' "$expected_assets_file" > "$expected_assets_raw"
LC_ALL=C sort "$expected_assets_raw" > "$expected_assets"
test -s "$expected_assets"
if [ -n "$(uniq -d "$expected_assets")" ]; then
    echo "duplicate filename in expected release asset contract" >&2
    exit 1
fi

awk '
  NF != 2 || length($1) != 64 || $1 !~ /^[0-9a-f]+$/ ||
    $2 !~ /^[A-Za-z0-9._-]+$/ || $2 == "SHA256SUMS" { exit 1 }
  { print $2 }
' "$publication_dir/SHA256SUMS" > "$manifest_assets_raw"
LC_ALL=C sort "$manifest_assets_raw" > "$manifest_assets"
test -s "$manifest_assets"
if [ -n "$(uniq -d "$manifest_assets")" ]; then
    echo "duplicate filename in public SHA256SUMS" >&2
    exit 1
fi

find "$publication_dir" -maxdepth 1 -type f ! -name SHA256SUMS \
    -exec basename {} \; > "$actual_assets_raw"
LC_ALL=C sort "$actual_assets_raw" > "$actual_assets"
test -s "$actual_assets"

diff -u "$expected_assets" "$manifest_assets"
diff -u "$expected_assets" "$actual_assets"

(
    cd "$publication_dir"
    verify_sha256_manifest SHA256SUMS
)

echo "verified public release asset set"
