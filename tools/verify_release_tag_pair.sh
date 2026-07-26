#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <binary-version> <schema-version> <expected-commit>" >&2
    exit 2
fi

binary_version=$1
schema_version=$2
expected_commit=$3
repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
git_bin=${GIT_BIN:-git}
# shellcheck source=tools/release_common.sh
. "$repo_root/tools/release_common.sh"

release_validate_version "$binary_version"
release_validate_version "$schema_version"
case "$expected_commit" in
    *[!0-9a-f]* | '')
        echo "invalid expected commit: $expected_commit" >&2
        exit 1
        ;;
esac

root_ref="refs/tags/v$binary_version"
schema_ref="refs/tags/clients/go/leyline-schema/v$schema_version"
root_commit=$(release_remote_tag_commit "$git_bin" origin "$root_ref")
schema_commit=$(release_remote_tag_commit "$git_bin" origin "$schema_ref")

test "$root_commit" = "$expected_commit"
test "$schema_commit" = "$expected_commit"
echo "root and schema tags peel to $expected_commit"
