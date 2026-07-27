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

release_retry() {
    release_retry_max_attempts=$1
    release_retry_delay_seconds=$2
    release_retry_description=$3
    shift 3

    case "$release_retry_max_attempts" in
        '' | *[!0-9]* | 0)
            echo "invalid retry attempt count: $release_retry_max_attempts" >&2
            return 2
            ;;
    esac
    case "$release_retry_delay_seconds" in
        '' | *[!0-9]*)
            echo "invalid retry delay: $release_retry_delay_seconds" >&2
            return 2
            ;;
    esac

    release_retry_attempt=1
    while :; do
        if "$@"; then
            return 0
        else
            release_retry_status=$?
        fi
        if [ "$release_retry_attempt" -ge "$release_retry_max_attempts" ]; then
            echo "$release_retry_description failed after $release_retry_attempt attempt(s)" >&2
            return "$release_retry_status"
        fi
        echo "$release_retry_description failed on attempt $release_retry_attempt; retrying" >&2
        sleep "$release_retry_delay_seconds"
        release_retry_attempt=$((release_retry_attempt + 1))
    done
}
