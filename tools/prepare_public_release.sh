#!/bin/sh
set -eu

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    else
        shasum -a 256 "$1"
    fi
}

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <artifact-root> <publication-dir> <expected-assets-file>" >&2
    exit 2
fi

artifact_root=$1
publication_dir=$2
expected_assets_file=$3

case "$publication_dir" in
    "" | / | . | ..)
        echo "refusing unsafe publication directory: '$publication_dir'" >&2
        exit 1
        ;;
esac
if [ -e "$publication_dir" ]; then
    echo "publication destination already exists: $publication_dir" >&2
    exit 1
fi

# No destination mutation happens until every per-target digest and the exact
# aggregate inventory have passed.
"$(dirname "$0")/verify_release_artifacts.sh" "$artifact_root"

inventory_tmp=$(mktemp -d "${TMPDIR:-/tmp}/leyline-public-prepare.XXXXXX")
publication_tmp=
cleanup() {
    rm -rf "$inventory_tmp"
    if [ -n "$publication_tmp" ] && [ -d "$publication_tmp" ]; then
        rm -rf "$publication_tmp"
    fi
}
trap cleanup 0 1 2 15

expected_assets="$inventory_tmp/expected-assets"
actual_assets_raw="$inventory_tmp/actual-assets-raw"
actual_assets="$inventory_tmp/actual-assets"
LC_ALL=C sort "$expected_assets_file" > "$expected_assets"
find "$artifact_root" -mindepth 2 -maxdepth 2 \
    -type f ! -name SHA256SUMS -exec basename {} \; > "$actual_assets_raw"
LC_ALL=C sort "$actual_assets_raw" > "$actual_assets"
diff -u "$expected_assets" "$actual_assets"

publication_parent=$(dirname "$publication_dir")
publication_name=$(basename "$publication_dir")
mkdir -p "$publication_parent"
publication_tmp=$(mktemp -d \
    "$publication_parent/.${publication_name}.tmp.XXXXXX")

for artifact_dir in "$artifact_root"/*; do
    test -d "$artifact_dir" || continue
    for source in "$artifact_dir"/*; do
        test -f "$source" || continue
        test "$(basename "$source")" != "SHA256SUMS" || continue
        cp "$source" "$publication_tmp/$(basename "$source")"
    done
done

manifest_tmp="$publication_tmp/.SHA256SUMS.tmp"
manifest_unsorted="$publication_tmp/.SHA256SUMS.unsorted"
(
    cd "$publication_tmp"
    for file in *; do
        test -f "$file"
        sha256_file "$file"
    done
) > "$manifest_unsorted"
LC_ALL=C sort -k 2 "$manifest_unsorted" > "$manifest_tmp"
rm -f "$manifest_unsorted"
mv "$manifest_tmp" "$publication_tmp/SHA256SUMS"

"$(dirname "$0")/verify_public_release.sh" \
    "$publication_tmp" "$expected_assets_file"
mv "$publication_tmp" "$publication_dir"
publication_tmp=

echo "prepared public release in $publication_dir"
