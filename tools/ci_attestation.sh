#!/bin/sh
set -eu

usage() {
    echo "usage: $0 begin|finish|check|verify-head" >&2
    exit 2
}

repo_root=$(git rev-parse --show-toplevel)
receipt_file=$(git rev-parse --git-path leyline-task-ci.receipt)
pending_file=$(git rev-parse --git-path leyline-task-ci.pending)

ci_snapshot() (
    set -eu
    git_dir=$(git rev-parse --absolute-git-dir)
    temporary_index=$(mktemp "$git_dir/leyline-ci-index.XXXXXX")
    trap 'rm -f "$temporary_index"' 0 1 2 15
    rm -f "$temporary_index"

    cd "$repo_root"
    GIT_INDEX_FILE="$temporary_index" git read-tree HEAD
    GIT_INDEX_FILE="$temporary_index" git add -A -- .
    GIT_INDEX_FILE="$temporary_index" git write-tree
)

ci_contract() {
    {
        printf '%s\n' "leyline-task-ci-receipt-v1"
        task --version 2>/dev/null || printf '%s\n' "task unavailable"
        rustc --version --verbose 2>/dev/null || printf '%s\n' "rustc unavailable"
        cargo --version 2>/dev/null || printf '%s\n' "cargo unavailable"
        git --version
        uname -sm
    } | git hash-object --stdin
}

write_record() {
    destination=$1
    snapshot=$2
    contract=$3
    temporary_record=$(mktemp "$destination.tmp.XXXXXX")
    printf 'v1|%s|%s\n' "$snapshot" "$contract" > "$temporary_record"
    chmod 600 "$temporary_record"
    mv "$temporary_record" "$destination"
}

read_record() {
    record=$1
    test -r "$record" || return 1
    IFS='|' read -r record_version record_snapshot record_contract < "$record"
    test "$record_version" = "v1"
    test -n "$record_snapshot"
    test -n "$record_contract"
}

command=${1:-}
case "$command" in
    begin)
        rm -f "$receipt_file" "$pending_file"
        write_record "$pending_file" "$(ci_snapshot)" "$(ci_contract)"
        ;;
    finish)
        rm -f "$receipt_file"
        if ! read_record "$pending_file"; then
            echo "task ci has no matching begin record; refusing attestation" >&2
            exit 1
        fi
        current_snapshot=$(ci_snapshot)
        current_contract=$(ci_contract)
        if [ "$record_snapshot" != "$current_snapshot" ] ||
            [ "$record_contract" != "$current_contract" ]; then
            rm -f "$pending_file"
            echo "repository or CI contract changed during task ci; refusing attestation" >&2
            exit 1
        fi
        write_record "$receipt_file" "$current_snapshot" "$current_contract"
        rm -f "$pending_file"
        echo "recorded task ci receipt for tree $current_snapshot"
        ;;
    check)
        if ! read_record "$receipt_file"; then
            echo "no successful task ci receipt for this worktree" >&2
            exit 1
        fi
        current_snapshot=$(ci_snapshot)
        current_contract=$(ci_contract)
        if [ "$record_snapshot" != "$current_snapshot" ] ||
            [ "$record_contract" != "$current_contract" ]; then
            echo "task ci receipt does not match the current tree and toolchain" >&2
            exit 1
        fi
        echo "task ci receipt matches tree $current_snapshot"
        ;;
    verify-head)
        current_snapshot=$(ci_snapshot)
        head_snapshot=$(git rev-parse 'HEAD^{tree}')
        if [ "$current_snapshot" != "$head_snapshot" ]; then
            echo "working tree differs from HEAD; commit or restore changes before push" >&2
            exit 1
        fi
        ;;
    *)
        usage
        ;;
esac
