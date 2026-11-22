#!/bin/sh
set -eu

# Fixture test for tools/land_integration.sh (bead ley-line-open-3a6beb):
# each refusal is named and leaves the remote integration branch where it
# was, and a branch with a receipt for its exact tree, containing
# integration, fast-forwards it.

# The fixture's git runs no developer hooks (bead ley-line-open-d1697b).
GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/dev/null
export GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-land-integration.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

remote="$tmp_dir/remote.git"
fixture="$tmp_dir/repo"
other="$tmp_dir/other"
git init -q --bare "$remote"

mkdir -p "$fixture/tools"
cp "$repo_root/tools/ci_attestation.sh" "$repo_root/tools/land_integration.sh" "$fixture/tools/"
git -C "$fixture" init -q
git -C "$fixture" config user.name "Land Fixture"
git -C "$fixture" config user.email "land@example.test"
git -C "$fixture" config commit.gpgsign false
printf '%s\n' "base" > "$fixture/tracked.txt"
git -C "$fixture" add tracked.txt tools
git -C "$fixture" commit -qm "base"
git -C "$fixture" remote add origin "$remote"
git -C "$fixture" push -q origin HEAD:refs/heads/integration
git -C "$fixture" checkout -q -b feature
printf '%s\n' "feature" > "$fixture/tracked.txt"
git -C "$fixture" commit -qam "feature work"

integration_tip() { git -C "$remote" rev-parse refs/heads/integration; }

# Expects a refusal naming $1; the remote must not move.
expect_refusal() {
    reason=$1
    before=$(integration_tip)
    if (cd "$fixture" && tools/land_integration.sh) 2> "$tmp_dir/err"; then
        echo "land succeeded where $reason was expected" >&2
        exit 1
    fi
    if ! grep -q "land: refused ($reason)" "$tmp_dir/err"; then
        echo "land did not refuse as $reason:" >&2
        cat "$tmp_dir/err" >&2
        exit 1
    fi
    test "$(integration_tip)" = "$before"
}

attest() { (cd "$fixture" && tools/ci_attestation.sh begin && tools/ci_attestation.sh finish) > /dev/null; }

# No receipt at all.
expect_refusal no-receipt

# A receipt for this tree, then an uncommitted edit: dirty-tree comes first.
attest
printf '%s\n' "uncommitted" > "$fixture/tracked.txt"
expect_refusal dirty-tree
git -C "$fixture" restore tracked.txt

# A receipt for an older tree does not cover a newer commit.
printf '%s\n' "feature, revised" > "$fixture/tracked.txt"
git -C "$fixture" commit -qam "revise after task ci"
expect_refusal no-receipt

# Integration moved on and is not in HEAD, even with a matching receipt.
attest
git clone -q -b integration "$remote" "$other"
printf '%s\n' "landed by someone else" > "$other/other.txt"
git -C "$other" add other.txt
git -C "$other" -c user.name=o -c user.email=o@example.test commit -q -m "someone else landed"
git -C "$other" push -q origin integration
expect_refusal integration-not-in-head

# Merge integration in, rerun task ci (the receipt), and land.
git -C "$fixture" fetch -q origin integration
git -C "$fixture" merge -q --no-edit FETCH_HEAD
expect_refusal no-receipt
attest
(cd "$fixture" && tools/land_integration.sh) > /dev/null
test "$(integration_tip)" = "$(git -C "$fixture" rev-parse HEAD)"

# Landing again is a no-op, not an error.
(cd "$fixture" && tools/land_integration.sh) | grep -q "nothing to land"

# A git shim on PATH acts between land's checks and its push, the way another
# writer in the same worktree or on the same remote could.
real_git=$(command -v git)
mkdir -p "$tmp_dir/shim"
cat > "$tmp_dir/shim/git" <<'SHIM'
#!/bin/sh
case "$1" in
fetch)
    if [ -n "${SHIM_WRITE_FILE:-}" ]; then
        printf '%s\n' "$SHIM_WRITE_TEXT" > "$SHIM_WRITE_FILE"
    fi
    ;;
push)
    if [ -n "${SHIM_ADVANCE_FROM:-}" ]; then
        "$SHIM_REAL_GIT" -C "$SHIM_ADVANCE_FROM" push -q origin integration
    fi
    ;;
esac
exec "$SHIM_REAL_GIT" "$@"
SHIM
chmod +x "$tmp_dir/shim/git"

# The remote moves between land's ancestry check and its push: the push is a
# fast-forward only, so it fails and the other landing survives. A forced
# push would overwrite it.
printf '%s\n' "next feature" > "$fixture/tracked.txt"
git -C "$fixture" commit -qam "next feature"
attest
git -C "$other" pull -q --no-rebase origin integration
printf '%s\n' "landed during the push" > "$other/race.txt"
git -C "$other" add race.txt
git -C "$other" -c user.name=o -c user.email=o@example.test commit -q -m "landed concurrently"
concurrent=$(git -C "$other" rev-parse HEAD)
if (cd "$fixture" && PATH="$tmp_dir/shim:$PATH" SHIM_REAL_GIT="$real_git" \
    SHIM_ADVANCE_FROM="$other" tools/land_integration.sh) > /dev/null 2>&1; then
    echo "land succeeded over a concurrent landing" >&2
    exit 1
fi
test "$(integration_tip)" = "$concurrent"

# The receipt binds to the tree of the commit pushed, not to the worktree.
# task ci passed on uncommitted bytes ("fixed") while HEAD holds others
# ("broken"); the file is restored, so the tree is clean when land starts, and
# a writer puts "fixed" back while land fetches. A working-tree check would
# now match the receipt and push "broken".
git -C "$fixture" fetch -q origin integration
git -C "$fixture" merge -q --no-edit FETCH_HEAD
printf '%s\n' "broken" > "$fixture/tracked.txt"
git -C "$fixture" commit -qam "broken"
printf '%s\n' "fixed" > "$fixture/tracked.txt"
attest
git -C "$fixture" restore tracked.txt
before=$(integration_tip)
if (cd "$fixture" && PATH="$tmp_dir/shim:$PATH" SHIM_REAL_GIT="$real_git" \
    SHIM_WRITE_FILE="$fixture/tracked.txt" SHIM_WRITE_TEXT="fixed" \
    tools/land_integration.sh) 2> "$tmp_dir/err" > /dev/null; then
    echo "land pushed a commit whose tree no receipt names" >&2
    exit 1
fi
grep -q "land: refused (no-receipt)" "$tmp_dir/err"
test "$(integration_tip)" = "$before"
git -C "$fixture" restore tracked.txt

echo "land:integration fixture proved the dirty-tree, integration-not-in-head and no-receipt refusals, a receipt-gated fast-forward, a non-forced push that loses to a concurrent landing, and a receipt bound to the pushed commit's tree"
