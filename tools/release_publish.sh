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
# shellcheck source=tools/release_common.sh
. "$repo_root/tools/release_common.sh"

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

# One status query per poll, not `gh run watch`: that re-fetches every job
# each 3 s and exhausted the API quota while v0.20.3's release run was still
# building. A failed query is retried, since by then the tag is public and the
# run is going either way.
poll_seconds=${RELEASE_POLL_SECONDS:-30}
max_polls=${RELEASE_MAX_POLLS:-120}
retry_attempts=${RELEASE_RETRY_ATTEMPTS:-5}
retry_delay_seconds=${RELEASE_RETRY_DELAY_SECONDS:-60}

polls=0
while :; do
    state=$(release_retry "$retry_attempts" "$retry_delay_seconds" \
        "status query for release run $run_id" \
        "$gh_bin" run view "$run_id" \
        --json status,conclusion \
        --jq '.status + " " + .conclusion')
    polls=$((polls + 1))
    case "$state" in
        'completed success') break ;;
        completed*)
            echo "release run $run_id finished: $state" >&2
            exit 1
            ;;
    esac
    if [ "$polls" -ge "$max_polls" ]; then
        echo "release run $run_id still '$state' after $polls polls" >&2
        exit 1
    fi
    sleep "$poll_seconds"
done

echo "release v$version published and publicly verified"
