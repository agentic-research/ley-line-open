#!/bin/sh

release_validate_version() {
    candidate=$1
    if ! printf '%s\n' "$candidate" |
        awk '/^[0-9]+\.[0-9]+\.[0-9]+$/ { found = 1 } END { exit !found }'
    then
        echo "invalid release version: $candidate" >&2
        return 1
    fi
}

release_remote_tag_commit() {
    git_command=$1
    remote=$2
    tag_ref=$3
    tag_lines=$("$git_command" ls-remote --tags "$remote" \
        "$tag_ref" "$tag_ref^{}")
    tag_commit=$(printf '%s\n' "$tag_lines" |
        awk -v ref="$tag_ref^{}" '$2 == ref { print $1; exit }')
    if [ -z "$tag_commit" ]; then
        tag_commit=$(printf '%s\n' "$tag_lines" |
            awk -v ref="$tag_ref" '$2 == ref { print $1; exit }')
    fi
    printf '%s\n' "$tag_commit"
}
