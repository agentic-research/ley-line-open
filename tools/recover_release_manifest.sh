#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <version-without-v> <expected-commit>" >&2
    exit 2
fi

version=$1
expected_commit=$2
repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
gh_bin=${GH_BIN:-gh}
verify_tags_bin=${VERIFY_TAGS_BIN:-"$repo_root/tools/verify_release_tag_pair.sh"}
# shellcheck source=tools/release_common.sh
. "$repo_root/tools/release_common.sh"

release_validate_version "$version"
"$verify_tags_bin" "$version" "$version" "$expected_commit"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-release-recovery.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15
first_download="$tmp_dir/first-download"
second_download="$tmp_dir/second-download"
release_json="$tmp_dir/release.json"
api_entries="$tmp_dir/api-entries"
manifest_unsorted="$tmp_dir/SHA256SUMS.unsorted"
expected_assets="$tmp_dir/expected-assets"
api_assets="$tmp_dir/api-assets"
api_assets_unsorted="$tmp_dir/api-assets-unsorted"
mkdir -p "$first_download"

"$gh_bin" api \
    "repos/agentic-research/ley-line-open/releases/tags/v$version" \
    > "$release_json"

jq -r '
  .assets[]
  | select(.name != "SHA256SUMS")
  | [(.digest // ""), .name]
  | @tsv
' "$release_json" > "$api_entries"

awk '
  NF != 2 || $1 !~ /^sha256:[0-9a-f]+$/ ||
    length($1) != 71 || $2 !~ /^[A-Za-z0-9._-]+$/ { exit 1 }
  { print substr($1, 8) "  " $2 }
' "$api_entries" > "$manifest_unsorted"
LC_ALL=C sort -k 2 "$manifest_unsorted" > "$tmp_dir/SHA256SUMS"

awk '{ print $2 }' "$tmp_dir/SHA256SUMS" > "$api_assets_unsorted"
LC_ALL=C sort "$api_assets_unsorted" > "$api_assets"
LC_ALL=C sort "$repo_root/tools/release-assets.txt" > "$expected_assets"
diff -u "$expected_assets" "$api_assets"

if jq -e '.assets[] | select(.name == "SHA256SUMS")' \
    "$release_json" >/dev/null
then
    mkdir -p "$second_download"
    "$gh_bin" release download "v$version" --dir "$second_download"
    "$repo_root/tools/verify_public_release.sh" \
        "$second_download" "$repo_root/tools/release-assets.txt"
    "$verify_tags_bin" "$version" "$version" "$expected_commit"
    echo "release v$version already has a verified public SHA256SUMS"
    exit 0
fi

"$gh_bin" release download "v$version" --dir "$first_download"
cp "$tmp_dir/SHA256SUMS" "$first_download/SHA256SUMS"
"$repo_root/tools/verify_public_release.sh" \
    "$first_download" "$repo_root/tools/release-assets.txt"

# Upload only the missing manifest. No --clobber: an unexpected concurrent
# publisher must fail rather than overwrite public evidence.
"$gh_bin" release upload "v$version" "$first_download/SHA256SUMS"

mkdir -p "$second_download"
"$gh_bin" release download "v$version" --dir "$second_download"
"$repo_root/tools/verify_public_release.sh" \
    "$second_download" "$repo_root/tools/release-assets.txt"
"$verify_tags_bin" "$version" "$version" "$expected_commit"

echo "recovered and publicly verified SHA256SUMS for v$version"
