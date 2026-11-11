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

# The public schema surface: everything a SCHEMA_VERSION names, so a change
# to any of it since the schema's tag must move SCHEMA_VERSION. It is the Go
# schema client plus the rs/ll-core/schema-spec IDL (Cap'n Proto and JSON
# Schema) and conformance vectors, which ship as release assets and carry no
# Go type (bead ley-line-open-da36d2). The bump script, the verifier and the
# tag script all decide through this one function (bead ley-line-open-a4946a:
# the bump script once read only the Go half, and produced v0.27.0 and
# v0.28.0 releases the verifier refused).
#
# release_schema_surface_unchanged <git> <tag>: succeeds when the working
# tree's surface equals <tag>'s.
release_schema_surface_unchanged() {
    "$1" diff --quiet "$2" -- \
        clients/go/leyline-schema \
        ':(glob)rs/ll-core/schema-spec/*.capnp' \
        ':(glob)rs/ll-core/schema-spec/**/*.capnp' \
        ':(glob)rs/ll-core/schema-spec/**/*.schema.json' \
        ':(glob)rs/ll-core/schema-spec/**/test-vectors/**'
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
