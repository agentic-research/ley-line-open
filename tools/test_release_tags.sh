#!/bin/sh
set -eu

# The fixture's git runs no developer hooks: a global `core.hooksPath`
# with a commit-msg policy must not reject `git commit -m seed` (bead
# ley-line-open-d1697b).
GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/dev/null
export GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-release-tags.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

# Tagging needs a passing hypervisor-suite receipt for the tree (bead
# ley-line-open-16a994). Fixtures record one with a stand-in suite through
# the real tools/hypervisor_attestation.sh, so the gate under test is the
# shipped one; the refusal itself is the no-receipt case at the end.
printf '#!/bin/sh\ntest "$LEYLINE_REQUIRE_HYPERVISOR" = 1\n' > "$tmp_dir/suite"
chmod +x "$tmp_dir/suite"
printf 'kernel' > "$tmp_dir/libkrunfw"
mkdir -p "$tmp_dir/guest-tools"
record_receipt() {
    (
        cd "$1"
        HYPERVISOR_SUITE="$tmp_dir/suite" \
            LEYLINE_LIBKRUNFW_PATH="$tmp_dir/libkrunfw" \
            LEYLINE_GUEST_TOOLS="$tmp_dir/guest-tools" \
            "$repo_root/tools/hypervisor_attestation.sh" record >/dev/null 2>&1
    )
}

new_fixture() {
    name=$1
    bare="$tmp_dir/$name.git"
    work="$tmp_dir/$name"
    git init --bare "$bare" >/dev/null
    git init -b main "$work" >/dev/null
    (
        cd "$work"
        git config user.name fixture
        git config user.email fixture@example.test
        git config commit.gpgsign false
        git config tag.gpgsign false
        git remote add origin "$bare"
        mkdir -p rs/ll-open/cli-lib/src/daemon clients/go/leyline-schema
        printf '%s\n' \
            'pub const SCHEMA_VERSION: &str = "0.10.4";' \
            > rs/ll-open/cli-lib/src/daemon/version.rs
        printf '%s\n' schema > clients/go/leyline-schema/api.go
        git add .
        git commit -m seed >/dev/null
        git push -u origin main >/dev/null
    )
    record_receipt "$work"
    printf '%s\n' "$work"
}

expect_failure() {
    label=$1
    shift
    if "$@"; then
        echo "$label unexpectedly succeeded" >&2
        exit 1
    fi
}

work=$(new_fixture atomic)
head_commit=$(git -C "$work" rev-parse HEAD)
REPO_ROOT="$work" VERIFY_VERSION_BIN=true \
    "$repo_root/tools/tag_release.sh" 0.10.4 >/dev/null
test "$(git -C "$work" ls-remote origin refs/tags/v0.10.4^{} |
    cut -f1)" = "$head_commit"
test "$(git -C "$work" ls-remote origin \
    refs/tags/clients/go/leyline-schema/v0.10.4^{} | cut -f1)" = \
    "$head_commit"
cat > "$tmp_dir/noisy-version-verifier" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "binary, schema, metadata, docs, and license agree"
EOF
chmod +x "$tmp_dir/noisy-version-verifier"
tag_output=$(REPO_ROOT="$work" \
    VERIFY_VERSION_BIN="$tmp_dir/noisy-version-verifier" \
    "$repo_root/tools/tag_release.sh" 0.10.4)
test "$tag_output" = "$head_commit"

work=$(new_fixture unchanged)
(
    cd "$work"
    git tag -a clients/go/leyline-schema/v0.10.4 -m schema
    git push origin refs/tags/clients/go/leyline-schema/v0.10.4 >/dev/null
    printf '%s\n' unrelated > release-note
    git add release-note
    git commit -m binary-only >/dev/null
    git push origin main >/dev/null
)
record_receipt "$work"
REPO_ROOT="$work" VERIFY_VERSION_BIN=true \
    "$repo_root/tools/tag_release.sh" 0.10.5 >/dev/null
test -n "$(git -C "$work" ls-remote origin refs/tags/v0.10.5)"

work=$(new_fixture partial)
(
    cd "$work"
    git tag -a v0.10.4 -m root-only
    git push origin refs/tags/v0.10.4 >/dev/null
)
expect_failure partial-tags \
    env REPO_ROOT="$work" VERIFY_VERSION_BIN=true \
    "$repo_root/tools/tag_release.sh" 0.10.4

work=$(new_fixture wrong)
old_commit=$(git -C "$work" rev-parse HEAD)
(
    cd "$work"
    git tag -a v0.10.4 -m wrong "$old_commit"
    git tag -a clients/go/leyline-schema/v0.10.4 -m wrong "$old_commit"
    git push --atomic origin \
        refs/tags/v0.10.4 \
        refs/tags/clients/go/leyline-schema/v0.10.4 >/dev/null
    printf '%s\n' newer > newer
    git add newer
    git commit -m newer >/dev/null
    git push origin main >/dev/null
)
record_receipt "$work"
expect_failure wrong-commit \
    env REPO_ROOT="$work" VERIFY_VERSION_BIN=true \
    "$repo_root/tools/tag_release.sh" 0.10.4

# A detached HEAD at origin/main is refused, with the reason named, and
# pushes no tag.
work=$(new_fixture detached)
git -C "$work" checkout -q --detach
if detached_err=$(REPO_ROOT="$work" VERIFY_VERSION_BIN=true \
    "$repo_root/tools/tag_release.sh" 0.10.4 2>&1 >/dev/null)
then
    echo "detached-head unexpectedly succeeded" >&2
    exit 1
fi
printf '%s\n' "$detached_err" | grep -q "not 'detached HEAD'"
test -z "$(git -C "$work" ls-remote origin refs/tags/v0.10.4)"

# No hypervisor receipt for the tree being released: refused before any tag
# exists, naming the command that produces one (bead ley-line-open-16a994).
work=$(new_fixture no-receipt)
(
    cd "$work"
    printf '%s\n' untested > untested
    git add untested
    git commit -m untested >/dev/null
    git push origin main >/dev/null
)
if receipt_err=$(REPO_ROOT="$work" VERIFY_VERSION_BIN=true \
    "$repo_root/tools/tag_release.sh" 0.10.4 2>&1 >/dev/null)
then
    echo "no-receipt unexpectedly succeeded" >&2
    exit 1
fi
printf '%s\n' "$receipt_err" | grep -q 'no passing hypervisor-suite receipt'
printf '%s\n' "$receipt_err" | grep -q 'task release:hypervisor:record'
test -z "$(git -C "$work" ls-remote origin refs/tags/v0.10.4)"
test -z "$(git -C "$work" tag -l v0.10.4)"
# The same tree, once recorded, tags.
record_receipt "$work"
REPO_ROOT="$work" VERIFY_VERSION_BIN=true \
    "$repo_root/tools/tag_release.sh" 0.10.4 >/dev/null
test -n "$(git -C "$work" ls-remote origin refs/tags/v0.10.4)"

echo "release tag fixture proved atomic, idempotent, receipt-gated, and fail-closed rules"
