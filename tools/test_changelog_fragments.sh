#!/bin/sh
# Fixture test for CHANGELOG fragments (bead ley-line-open-465ce2,
# acceptance test 1).
#
# A copy of the files tools/bump_release_version.sh touches becomes a git
# repository. Two branches, standing for two PRs, each add one fragment.
# They must merge into the base in either order with no conflict, and the
# real bump script must then write both entries into the new version's
# section, Security before Fixed, delete the fragments, and leave
# [Unreleased] empty; both merge orders must produce the same CHANGELOG.
# The lint must reject a bad name, an unknown section, an empty entry, and an
# entry written under [Unreleased].
set -eu

repo=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/llo-changelog-fragments.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

git_quiet() {
    git -c user.name=fixture -c user.email=fixture@example.invalid \
        -c core.hooksPath=/dev/null -c init.defaultBranch=main \
        -c commit.gpgsign=false "$@" >/dev/null
}

# --- the base tree: everything the bump script reads or writes ----------
base="$scratch/base"
mkdir -p "$base"
copy() {
    mkdir -p "$base/$(dirname "$1")"
    cp -f "$repo/$1" "$base/$1"
}
for file in CHANGELOG.md README.md compatibility.json docs/ARCHITECTURE.md \
    changes/README.md rs/ll-core/public-schema/Cargo.toml \
    rs/ll-open/cli/Cargo.toml rs/ll-open/cli-lib/src/daemon/version.rs \
    tools/bump_release_version.sh tools/release_common.sh \
    tools/changelog_fragments.sh tools/release-version-manifests.txt; do
    copy "$file"
done
while IFS= read -r manifest; do
    [ -n "$manifest" ] && copy "$manifest"
done <"$repo/tools/release-version-manifests.txt"
# Fragments pending in the working tree are not part of this fixture.
for pending in "$base"/changes/*.md; do
    [ "$(basename "$pending")" = README.md ] || rm -f "$pending"
done

old_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$base/rs/ll-open/cli/Cargo.toml" | head -n1)
new_version=$(printf '%s\n' "$old_version" | awk -F. '{ printf "%d.%d.%d", $1, $2, $3 + 1 }')

git_quiet -C "$base" init
git_quiet -C "$base" add -A
git_quiet -C "$base" commit -m base

# --- two PRs, one fragment each ------------------------------------------
git_quiet -C "$base" checkout -q -b pr-fixed main
cat >"$base/changes/ley-line-open-aaaaaa.md" <<'EOF'
Section: Fixed

- **Fixture fix entry** (bead `ley-line-open-aaaaaa`). First PR's text,
  wrapped onto a second line.
EOF
git_quiet -C "$base" add -A
git_quiet -C "$base" commit -m "pr fixed"

git_quiet -C "$base" checkout -q -b pr-security main
cat >"$base/changes/ley-line-open-bbbbbb-guard.md" <<'EOF'
Section: Security


- **Fixture security entry** (bead `ley-line-open-bbbbbb`). Second PR's text.

EOF
git_quiet -C "$base" add -A
git_quiet -C "$base" commit -m "pr security"

# merge_and_bump <name> <first> <second>: clone the base, merge the two PRs
# in the given order, and run the release bump.
merge_and_bump() {
    clone="$scratch/$1"
    git_quiet clone -q "$base" "$clone"
    git_quiet -C "$clone" checkout -q main
    for branch in "$2" "$3"; do
        if ! git_quiet -C "$clone" merge --no-ff --no-edit "origin/$branch"; then
            fail "merging $branch ($1 order) conflicted"
        fi
    done
    [ -f "$clone/changes/ley-line-open-aaaaaa.md" ] || fail "$1: fixed fragment missing after merge"
    [ -f "$clone/changes/ley-line-open-bbbbbb-guard.md" ] || fail "$1: security fragment missing after merge"
    if ! REPO_ROOT="$clone" GIT_BIN=git RELEASE_DATE=2000-01-02 \
        sh "$clone/tools/bump_release_version.sh" "$new_version" \
        >"$scratch/$1.log" 2>&1
    then
        cat "$scratch/$1.log" >&2
        fail "$1: bump_release_version.sh failed"
    fi
}

merge_and_bump fixed-first pr-fixed pr-security
merge_and_bump security-first pr-security pr-fixed

cmp -s "$scratch/fixed-first/CHANGELOG.md" "$scratch/security-first/CHANGELOG.md" ||
    fail "the two merge orders assembled different CHANGELOGs"

changelog="$scratch/fixed-first/CHANGELOG.md"
expected="$scratch/expected-section"
cat >"$expected" <<EOF
## [$new_version] — 2000-01-02

### Security

- **Fixture security entry** (bead \`ley-line-open-bbbbbb\`). Second PR's text.

### Fixed

- **Fixture fix entry** (bead \`ley-line-open-aaaaaa\`). First PR's text,
  wrapped onto a second line.

## [$old_version]
EOF
# The new section, through the first line of the previous release's heading.
awk -v new="## [$new_version]" -v old="## [$old_version]" '
    index($0, new) == 1 { inside = 1 }
    inside && index($0, old) == 1 { print old; exit }
    inside { print }
' "$changelog" >"$scratch/actual-section"
if ! cmp -s "$expected" "$scratch/actual-section"; then
    diff -u "$expected" "$scratch/actual-section" >&2 || true
    fail "assembled section differs from the expected text"
fi

for name in fixed-first security-first; do
    left=$(find "$scratch/$name/changes" -type f ! -name README.md)
    [ -z "$left" ] || fail "$name: fragments not deleted: $left"
done
unreleased=$(awk '/^## \[Unreleased\]/ { inside = 1; next } inside && /^## \[/ { exit } inside' "$changelog")
printf '%s\n' "$unreleased" | grep -q 'changes/' || fail "[Unreleased] lost its pointer to changes/"
if printf '%s\n' "$unreleased" | grep -Eq '^(### |- )'; then
    fail "[Unreleased] is not empty after the bump"
fi
CHANGES_DIR="$scratch/fixed-first/changes" CHANGELOG="$changelog" \
    sh "$repo/tools/changelog_fragments.sh" lint >/dev/null ||
    fail "the bumped tree does not lint"

# --- the lint refuses malformed fragments --------------------------------
# reject <case> <file name> <content>: the lint must fail on this fragment.
reject() {
    dir="$scratch/reject-$1"
    mkdir -p "$dir"
    printf '%s' "$3" >"$dir/$2"
    if CHANGES_DIR="$dir" CHANGELOG="$scratch/none" \
        sh "$repo/tools/changelog_fragments.sh" lint >/dev/null 2>&1
    then
        fail "lint accepted $1 ($2)"
    fi
}
reject bad-name fix-the-thing.md 'Section: Fixed

- entry
'
reject short-id ley-line-open-abc.md 'Section: Fixed

- entry
'
reject unknown-section ley-line-open-cccccc.md 'Section: Bugfixes

- entry
'
reject no-section ley-line-open-dddddd.md '- entry with no section line
'
reject empty-body ley-line-open-eeeeee.md 'Section: Added


'

stray="$scratch/stray-changelog.md"
printf '%s\n' '# Changelog' '' '## [Unreleased]' '' '### Fixed' '' '- an entry' '' \
    '## [0.1.0] — 2000-01-01' >"$stray"
mkdir -p "$scratch/empty-changes"
if CHANGES_DIR="$scratch/empty-changes" CHANGELOG="$stray" \
    sh "$repo/tools/changelog_fragments.sh" lint >/dev/null 2>&1
then
    fail "lint accepted an entry written under [Unreleased]"
fi

echo "changelog fragments: two PRs merge in either order without conflict; the bump assembles both, Security first; the lint rejects 6 malformed cases"
