#!/usr/bin/env bash
#
# Prove every included Taskfile's `dir:` resolves to the cargo workspace from
# BOTH entry points: the repo root, and `rs/`.
#
# ## Why this exists
#
# `task` picks its root Taskfile by walking up from the working directory, so
# running it inside `rs/` discovers `rs/Taskfile.yml` and `ROOT_DIR` becomes
# `<repo>/rs` rather than `<repo>`. Every `dir: '{{.ROOT_DIR}}/rs'` then
# resolves to `<repo>/rs/rs` — and Task CREATES that directory instead of
# failing, so the task runs to completion in the wrong place.
#
# Two measured rules, both of which this script defends:
#
#   1. Non-root Taskfiles must not use `ROOT_DIR`, because it follows the
#      discovered Taskfile rather than the repository.
#   2. `dir:` must reference a special variable DIRECTLY. Routing the identical
#      absolute path through a `vars:` entry makes Task treat it as relative
#      and join it onto the base directory, yielding a doubled path like
#      `<repo>/Users/.../rs`. This is not documented; it was measured, and it
#      is why the first attempt at rule 1 broke every package task.
#
# Both rules produce WRONG DIRECTORIES THAT GET CREATED, not errors — which is
# why a static grep is not enough on its own and this script also runs the
# tasks.
#
# Exit codes:
#   0  every checked task resolves to <repo>/rs from both entry points
#   1  a violation (static or behavioral)
#   2  internal error

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
rs="$repo/rs"
status=0

if [ ! -d "$rs" ]; then
    echo "expected a cargo workspace at $rs" >&2
    exit 2
fi

# --- Static half: the two rules, as text ------------------------------------

taskfiles=$(find "$rs" -name Taskfile.yml -not -path '*/target/*' | sort)
[ -n "$taskfiles" ] || { echo "found no non-root Taskfiles under $rs" >&2; exit 2; }

while IFS= read -r taskfile; do
    rel=${taskfile#"$repo/"}
    # Comments are exempt so the rules can be explained in prose.
    body=$(sed 's/#.*//' "$taskfile")

    if printf '%s\n' "$body" | grep -q 'ROOT_DIR'; then
        echo "$rel: uses ROOT_DIR — it follows the DISCOVERED Taskfile, so it" >&2
        echo "    is <repo>/rs when task runs from rs/. Use TASKFILE_DIR." >&2
        status=1
    fi

    # `dir:` referencing anything that is not a special variable.
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        case "$line" in
            # Correct anchors, and ROOT_DIR which rule 1 above already owns —
            # reporting it twice, with an explanation that does not apply to a
            # special variable, would obscure the real fix.
            *'{{.TASKFILE_DIR'*|*'{{.USER_WORKING_DIR'*|*'{{.ROOT_DIR'*) ;;
            *'{{'*)
                echo "$rel: dir: indirects through a variable —$line" >&2
                echo "    Task joins the result onto the base dir instead of" >&2
                echo "    treating it as absolute. Reference TASKFILE_DIR directly." >&2
                status=1
                ;;
        esac
    done <<EOF
$(printf '%s\n' "$body" | grep -E '^\s*dir:' || true)
EOF
done <<EOF
$taskfiles
EOF

# --- Behavioral half: running tasks must not CREATE a directory -------------
#
# Both failure modes share one observable: Task resolves `dir:` to a path that
# does not exist and creates it rather than failing. So the behavioral check is
# not "is the resolved path right?" — `task --dry` prints commands, not the
# directory it resolved, so there is nothing to assert on there — but "did
# running these tasks bring a new directory into existence?" That catches the
# two known modes and any future one with the same signature.
#
# `--dry` still resolves `dir:` and still creates it, so this needs no build.

snapshot_dirs() {
    find "$repo" -maxdepth 4 -type d \
        -not -path '*/target*' -not -path '*/.git*' -not -path '*/node_modules*' \
        2>/dev/null | sort
}

before=$(snapshot_dirs)

probe_from() {
    local from=$1 label=$2 out
    shift 2
    if ! out=$(cd "$from" && task --dry "$@" 2>&1); then
        echo "task --dry $* failed from $label ($from):" >&2
        printf '%s\n' "$out" >&2
        status=1
    fi
}

# Each entry point probes what it can actually name there.
#
# From the repo root, package tasks are reachable through the root Taskfile's
# includes — and from here a ROOT_DIR anchor still resolves CORRECTLY, so this
# pass alone proves nothing about the bug. From rs/, `task` discovers
# rs/Taskfile.yml, which owns only the mutation tasks.
#
# The third is the one that bites: `task` run from inside a package directory
# discovers THAT package's Taskfile, so ROOT_DIR becomes the package dir and
# `{{.ROOT_DIR}}/rs` resolves under it. runtime/ stands for all of them — they
# share one shape, and the static rule above covers the rest by inspection.
probe_from "$repo" root runtime:test cli-lib:test fs:test:cdc sign:host:build
probe_from "$rs" rs mutants:pr
probe_from "$rs/ll-open/runtime" runtime-package test

after=$(snapshot_dirs)
created=$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") || true)

if [ -n "$created" ]; then
    echo "running tasks created directories — a wrong dir: was resolved and used:" >&2
    printf '%s\n' "$created" | sed 's/^/    /' >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    echo "taskfile anchors: dir: resolves to $rs from the repo root, rs/, and a package dir"
fi
exit "$status"
