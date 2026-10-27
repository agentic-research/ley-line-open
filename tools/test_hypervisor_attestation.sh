#!/bin/sh
# Fixture for the hypervisor-suite release gate (bead ley-line-open-16a994):
# a receipt is recorded only for a passing suite that ran with
# LEYLINE_REQUIRE_HYPERVISOR=1 on an unchanged tree, and `check` accepts only
# a receipt for the exact tree. tools/test_release_tags.sh proves tagging
# refuses without one.
set -eu

# The fixture's git runs no developer hooks (bead ley-line-open-d1697b).
GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/dev/null
export GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
attest="$repo_root/tools/hypervisor_attestation.sh"
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-hypervisor-attestation.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

fail() {
    echo "$*" >&2
    exit 1
}

work="$tmp_dir/work"
git init -q -b main "$work"
git -C "$work" config user.name fixture
git -C "$work" config user.email fixture@example.test
git -C "$work" config commit.gpgsign false
printf '%s\n' seed > "$work/file"
git -C "$work" add file
git -C "$work" commit -q -m seed
receipts="$work/.git/leyline-hypervisor-receipts"

printf 'kernel' > "$tmp_dir/libkrunfw.dylib"
mkdir -p "$tmp_dir/rootfs"
LEYLINE_LIBKRUNFW_PATH="$tmp_dir/libkrunfw.dylib"
LEYLINE_GUEST_TOOLS="$tmp_dir/rootfs"
export LEYLINE_LIBKRUNFW_PATH LEYLINE_GUEST_TOOLS

# The suite must see LEYLINE_REQUIRE_HYPERVISOR=1, so a host without a
# hypervisor fails rather than skips.
cat > "$tmp_dir/suite-pass" <<'EOF'
#!/bin/sh
test "${LEYLINE_REQUIRE_HYPERVISOR:-}" = 1
test -n "${LEYLINE_LIBKRUNFW_PATH:-}"
test -n "${LEYLINE_GUEST_TOOLS:-}"
EOF
printf '#!/bin/sh\nexit 1\n' > "$tmp_dir/suite-fail"
cat > "$tmp_dir/suite-mutates" <<'EOF'
#!/bin/sh
printf 'changed during the run\n' > untracked-output
EOF
chmod +x "$tmp_dir/suite-pass" "$tmp_dir/suite-fail" "$tmp_dir/suite-mutates"

expect_failure() {
    label=$1
    shift
    if "$@"; then
        fail "$label unexpectedly succeeded"
    fi
}

cd "$work"
tree=$(git rev-parse 'HEAD^{tree}')

# No receipt yet: check refuses and names the command that produces one.
if check_err=$("$attest" check 2>&1); then
    fail "check passed with no receipt"
fi
printf '%s\n' "$check_err" | grep -q 'task release:hypervisor:record' ||
    fail "the refusal does not name the command that records a receipt"

# The suite's inputs are required.
expect_failure missing-libkrunfw \
    env -u LEYLINE_LIBKRUNFW_PATH HYPERVISOR_SUITE="$tmp_dir/suite-pass" "$attest" record
expect_failure missing-guest-tools \
    env -u LEYLINE_GUEST_TOOLS HYPERVISOR_SUITE="$tmp_dir/suite-pass" "$attest" record
test ! -e "$receipts" || fail "a refused record wrote a receipt"

# A failing suite records nothing.
expect_failure failing-suite \
    env HYPERVISOR_SUITE="$tmp_dir/suite-fail" "$attest" record
test ! -e "$receipts/$tree" || fail "a failing suite recorded a receipt"

# A suite that changes the tree it tests records nothing.
expect_failure mutating-suite \
    env HYPERVISOR_SUITE="$tmp_dir/suite-mutates" "$attest" record
test ! -e "$receipts/$tree" || fail "a mutated tree recorded a receipt"
rm -f untracked-output

# A passing suite on an unchanged tree records a receipt naming that tree,
# and check accepts it, implicitly and by explicit tree.
HYPERVISOR_SUITE="$tmp_dir/suite-pass" "$attest" record >/dev/null
test -f "$receipts/$tree" || fail "a passing suite recorded no receipt"
grep -qx "tree $tree" "$receipts/$tree" || fail "the receipt names another tree"
grep -q '^suite LEYLINE_REQUIRE_HYPERVISOR=1 ' "$receipts/$tree" ||
    fail "the receipt does not record that the hypervisor was required"
"$attest" check 2>/dev/null || fail "check refused the matching receipt"
"$attest" check "$tree" 2>/dev/null || fail "check refused the explicit tree"

# An installed worker (LEYLINE_KRUN_WORKER, as `task release:hypervisor:record`
# sets it) reaches the suite, and the receipt names its digest. On macOS it
# must carry the hypervisor entitlement first; elsewhere there is none to hold.
cat > "$tmp_dir/suite-worker" <<'EOF'
#!/bin/sh
test "${LEYLINE_REQUIRE_HYPERVISOR:-}" = 1
test -f "${LEYLINE_KRUN_WORKER:?the suite was not handed the installed worker}"
EOF
chmod +x "$tmp_dir/suite-worker"
if [ "$(uname -s)" = Darwin ]; then
    cp /usr/bin/true "$tmp_dir/installed-worker"
    /usr/bin/codesign --force --sign - "$tmp_dir/installed-worker" 2>/dev/null
    expect_failure unentitled-installed-worker \
        env LEYLINE_KRUN_WORKER="$tmp_dir/installed-worker" \
        HYPERVISOR_SUITE="$tmp_dir/suite-worker" "$attest" record
    "$repo_root/tools/sign_krun_worker.sh" "$tmp_dir/installed-worker" 2>/dev/null
else
    printf 'worker' > "$tmp_dir/installed-worker"
fi
expect_failure missing-installed-worker \
    env LEYLINE_KRUN_WORKER="$tmp_dir/no-such-worker" \
    HYPERVISOR_SUITE="$tmp_dir/suite-worker" "$attest" record
LEYLINE_KRUN_WORKER="$tmp_dir/installed-worker" \
    HYPERVISOR_SUITE="$tmp_dir/suite-worker" "$attest" record >/dev/null
grep -q "^worker $tmp_dir/installed-worker sha256 [0-9a-f]\{64\}\$" "$receipts/$tree" ||
    fail "the receipt does not name the installed worker and its digest"

# Uncommitted edits are a different tree.
printf '%s\n' edited > file
expect_failure dirty-tree "$attest" check
git checkout -q -- file

# A new commit is a different tree: the old receipt does not cover it.
printf '%s\n' newer > file
git commit -q -am newer
expect_failure new-tree "$attest" check
expect_failure new-tree-explicit "$attest" check "$(git rev-parse 'HEAD^{tree}')"

# A receipt whose body names a different tree than its file name is refused.
new_tree=$(git rev-parse 'HEAD^{tree}')
sed "s/^tree .*/tree $tree/" "$receipts/$tree" > "$receipts/$new_tree"
expect_failure mismatched-body "$attest" check "$new_tree"

echo "hypervisor receipt fixture proved pass-only, unchanged-tree, exact-tree rules"
