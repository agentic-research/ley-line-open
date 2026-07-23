#!/bin/sh
set -eu

verify_sha256_manifest() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$1"
    else
        shasum -a 256 -c "$1"
    fi
}

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <artifact-root>" >&2
    exit 2
fi

artifact_root=$1
test -d "$artifact_root"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-release-verify.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15
all_names="$tmp_dir/all-names"
: > "$all_names"

directory_count=0
for artifact_dir in "$artifact_root"/*; do
    test -d "$artifact_dir" || continue
    directory_count=$((directory_count + 1))
    manifest="$artifact_dir/SHA256SUMS"
    test -s "$manifest"

    expected_names="$tmp_dir/expected-$directory_count"
    actual_names="$tmp_dir/actual-$directory_count"

    awk '
      NF != 2 || length($1) != 64 || $1 !~ /^[0-9a-f]+$/ ||
        $2 !~ /^[A-Za-z0-9._-]+$/ { exit 1 }
      { print $2 }
    ' "$manifest" | LC_ALL=C sort > "$expected_names"

    if [ -n "$(uniq -d "$expected_names")" ]; then
        echo "duplicate filename in $manifest" >&2
        exit 1
    fi

    find "$artifact_dir" -maxdepth 1 -type f ! -name SHA256SUMS \
        -exec basename {} \; | LC_ALL=C sort > "$actual_names"
    test -s "$actual_names"
    diff -u "$expected_names" "$actual_names"

    (
        cd "$artifact_dir"
        verify_sha256_manifest SHA256SUMS
    )
    cat "$actual_names" >> "$all_names"
done

if [ "$directory_count" -eq 0 ]; then
    echo "no downloaded release artifact directories under $artifact_root" >&2
    exit 1
fi

duplicate=$(LC_ALL=C sort "$all_names" | uniq -d | head -n 1)
if [ -n "$duplicate" ]; then
    echo "duplicate release asset across build artifacts: $duplicate" >&2
    exit 1
fi

echo "verified $directory_count release artifact set(s)"
