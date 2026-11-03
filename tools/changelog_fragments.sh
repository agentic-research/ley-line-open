#!/bin/sh
# CHANGELOG fragments (bead ley-line-open-465ce2).
#
# A change never edits CHANGELOG.md. It adds one file under changes/:
#
#   changes/<bead-id>.md  or  changes/<bead-id>-<slug>.md
#
# whose first line names the CHANGELOG section and whose remaining lines are
# the entry, in the existing CHANGELOG style:
#
#   Section: Fixed
#
#   - **What changed, in one bold clause** (bead `ley-line-open-xxxxxx`).
#     Why it matters and what a user sees.
#
# Two PRs that each add a fragment touch different paths, so they merge in
# either order without a conflict. Before fragments, every merge made each
# other open PR conflict on the [Unreleased] block and forced a rebase and a
# full CI rerun.
#
#   lint      validate every fragment and that CHANGELOG.md's [Unreleased]
#             section holds no entries (run by `task ci`).
#   assemble  <version> <date>: write the fragments into a new
#             `## [<version>] — <date>` section of CHANGELOG.md, sections in
#             canonical order (Security first), then delete them. Called by
#             tools/bump_release_version.sh.
#
# CHANGES_DIR and CHANGELOG override the paths (the fixture test uses them).
set -eu

repo_root=${REPO_ROOT:-$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)}
changes_dir=${CHANGES_DIR:-$repo_root/changes}
changelog=${CHANGELOG:-$repo_root/CHANGELOG.md}

# Canonical order: what a reader must act on first comes first.
sections='Security Breaking Added Changed Fixed Removed'
name_pattern='^ley-line-open-[0-9a-f]{6}(-[a-z0-9][a-z0-9-]*)?\.md$'

usage() {
    echo "usage: $0 lint | assemble <version> <date>" >&2
    exit 2
}

# The fragment files, sorted, one per line. README.md documents the
# directory and is not a fragment.
fragments() {
    [ -d "$changes_dir" ] || return 0
    for path in "$changes_dir"/*; do
        [ -f "$path" ] || continue
        [ "$(basename "$path")" = README.md ] && continue
        printf '%s\n' "$path"
    done | LC_ALL=C sort
}

section_of() {
    sed -n '1s/^Section:[[:space:]]*\([A-Za-z]*\)[[:space:]]*$/\1/p' "$1"
}

# The entry: everything after the Section line, without leading or trailing
# blank lines.
body_of() {
    sed '1d' "$1" | awk '
        NF { started = 1 }
        started { lines[++n] = $0; if (NF) last = n }
        END { for (i = 1; i <= last; i++) print lines[i] }
    '
}

lint() {
    failed=0
    for path in $(fragments); do
        name=$(basename "$path")
        if ! printf '%s\n' "$name" | grep -Eq "$name_pattern"; then
            echo "$path: name must be <bead-id>.md or <bead-id>-<slug>.md (ley-line-open-<6 hex>[-slug].md)" >&2
            failed=1
        fi
        section=$(section_of "$path")
        case " $sections " in
            *" $section "*) [ -n "$section" ] || section=missing ;;
            *) section=missing ;;
        esac
        if [ "$section" = missing ]; then
            echo "$path: first line must be 'Section: <one of: $sections>'" >&2
            failed=1
        fi
        if [ -z "$(body_of "$path")" ]; then
            echo "$path: entry text is empty" >&2
            failed=1
        fi
    done
    # The [Unreleased] block is a pointer to changes/, never a place for
    # entries: an entry there is the conflict fragments exist to prevent.
    if [ -f "$changelog" ]; then
        stray=$(awk '
            /^## \[Unreleased\]/ { inside = 1; next }
            inside && /^## \[/ { exit }
            inside && (/^### / || /^- /) { print NR": "$0 }
        ' "$changelog")
        if [ -n "$stray" ]; then
            echo "$changelog: [Unreleased] holds entries; move them to changes/<bead-id>.md:" >&2
            printf '%s\n' "$stray" >&2
            failed=1
        fi
    fi
    [ "$failed" -eq 0 ] || exit 1
    echo "changelog fragments: $(fragments | grep -c . || true) valid"
}

assemble() {
    version=$1
    date=$2
    lint >/dev/null
    list=$(fragments)
    if [ -z "$list" ]; then
        echo "no fragments in $changes_dir: a release with nothing to record has no CHANGELOG section to write" >&2
        exit 1
    fi
    if grep -q "^## \\[$version\\]" "$changelog"; then
        echo "$changelog already has a [$version] section" >&2
        exit 1
    fi
    block=$(mktemp "${TMPDIR:-/tmp}/llo-changelog-block.XXXXXX")
    output=$(mktemp "$changelog.tmp.XXXXXX")
    trap 'rm -f "$block" "$output"' EXIT HUP INT TERM
    printf '## [%s] — %s\n' "$version" "$date" > "$block"
    for section in $sections; do
        heading_written=0
        for path in $list; do
            [ "$(section_of "$path")" = "$section" ] || continue
            if [ "$heading_written" -eq 0 ]; then
                printf '\n### %s\n' "$section" >> "$block"
                heading_written=1
            fi
            printf '\n' >> "$block"
            body_of "$path" >> "$block"
        done
    done
    printf '\n' >> "$block"
    # Insert before the first released section: [Unreleased] and its pointer
    # stay on top, empty.
    awk -v block="$block" '
        !done && /^## \[[0-9]/ {
            while ((getline line < block) > 0) print line
            done = 1
        }
        { print }
        END { if (!done) exit 1 }
    ' "$changelog" > "$output" || {
        echo "$changelog: no released section to insert before" >&2
        exit 1
    }
    mv -f "$output" "$changelog"
    for path in $list; do
        rm -f "$path"
    done
    echo "assembled $(printf '%s\n' "$list" | grep -c .) fragment(s) into $changelog [$version]" >&2
}

case "${1:-}" in
    lint)
        [ "$#" -eq 1 ] || usage
        lint
        ;;
    assemble)
        [ "$#" -eq 3 ] || usage
        assemble "$2" "$3"
        ;;
    *)
        usage
        ;;
esac
