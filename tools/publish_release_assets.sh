#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <publication-dir> <tag> <expected-assets-file>" >&2
    exit 2
fi

publication_dir=$1
tag=$2
expected_assets_file=$3
gh_bin=${GH_BIN:-gh}
script_root=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
# shellcheck source=tools/release_common.sh
. "$script_root/release_common.sh"

case "$tag" in
    v*) release_validate_version "${tag#v}" ;;
    *)
        echo "invalid release tag: $tag" >&2
        exit 1
        ;;
esac

# This is deliberately the final check before the first credentialed command.
# If it fails, neither release creation nor asset upload is reachable.
"$script_root/verify_public_release.sh" \
    "$publication_dir" "$expected_assets_file"

if ! "$gh_bin" release view "$tag" >/dev/null 2>&1; then
    "$gh_bin" release create "$tag" \
        --title "$tag" \
        --generate-notes
fi

set -- "$publication_dir"/*
test "$#" -gt 0
"$gh_bin" release upload "$tag" "$@" --clobber

echo "published verified assets for $tag"
