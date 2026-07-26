#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <version-without-v>" >&2
    exit 2
fi

version=$1
repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
gh_bin=${GH_BIN:-gh}
tag_release_bin=${TAG_RELEASE_BIN:-"$repo_root/tools/tag_release.sh"}

head_commit=$("$tag_release_bin" "$version")
root_tag="v$version"

attempt=0
run_id=
while [ "$attempt" -lt 30 ]; do
    run_list=$("$gh_bin" run list \
        --workflow release.yml \
        --event push \
        --commit "$head_commit" \
        --limit 20 \
        --json databaseId,headBranch,headSha \
        --jq ".[] | select(.headBranch == \"$root_tag\" and .headSha == \"$head_commit\") | .databaseId")
    run_id=$(printf '%s\n' "$run_list" | sed -n '1p')
    test -z "$run_id" || break
    attempt=$((attempt + 1))
    sleep 2
done

case "$run_id" in
    '' | *[!0-9]*)
        echo "could not find release workflow for $root_tag at $head_commit" >&2
        exit 1
        ;;
esac

"$gh_bin" run watch "$run_id" --exit-status

echo "release v$version published and publicly verified"
