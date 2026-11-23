#!/bin/sh
set -eu

# Land the current branch on `integration` by its local `task ci` receipt
# (bead ley-line-open-3a6beb). A pull request into integration no longer
# reruns the runner: the tree-keyed receipt tools/ci_attestation.sh records
# at the end of a passing `task ci` is the evidence, and this script is the
# only gate between it and the integration branch. It runs on the client by
# explicit invocation, not from a git hook, so no hooks path can skip it.
#
# Refuses, each by name, and changes nothing when it does:
#   dirty-tree               the working tree differs from HEAD, so the
#                            tested bytes are not the bytes that would land;
#   integration-not-in-head  the remote integration tip is not an ancestor
#                            of HEAD, so landing would not be a fast-forward
#                            and the tested tree would not be the landed one
#                            (merge integration into the branch, rerun
#                            `task ci`);
#   no-receipt               no `task ci` receipt names HEAD's tree and the
#                            current toolchain contract.
# Otherwise it pushes HEAD to integration as a fast-forward, never forced.
#
# LAND_REMOTE and LAND_BRANCH override `origin` and `integration` (the
# fixture test lands on a local bare repository).

remote=${LAND_REMOTE:-origin}
branch=${LAND_BRANCH:-integration}

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

refuse() {
    reason=$1
    shift
    echo "land: refused ($reason): $*" >&2
    exit 1
}

# The commit this run lands and its tree, read once. Every later check is
# against these values, never against the worktree or HEAD again, so a
# write to the worktree or a branch switch during the run cannot change what
# lands relative to what was checked.
head=$(git rev-parse --verify 'HEAD^{commit}')
tree=$(git rev-parse --verify "$head^{tree}")

# Early and for the user: a dirty tree usually means the receipt names
# uncommitted bytes. The binding itself is check-tree below.
if ! tools/ci_attestation.sh verify-head >/dev/null 2>&1; then
    refuse dirty-tree "the working tree differs from HEAD; commit or restore changes, then rerun task ci"
fi

if ! git fetch --quiet "$remote" "refs/heads/$branch"; then
    echo "land: could not fetch $remote $branch" >&2
    exit 1
fi
target=$(git rev-parse --verify 'FETCH_HEAD^{commit}')

if ! git merge-base --is-ancestor "$target" "$head"; then
    refuse integration-not-in-head \
        "$remote/$branch ($target) is not an ancestor of $head; merge it into this branch and rerun task ci"
fi

if ! tools/ci_attestation.sh check-tree "$tree" >/dev/null 2>&1; then
    refuse no-receipt \
        "no task ci receipt names $head's tree ($tree) under the current toolchain; run task ci"
fi

if [ "$target" = "$head" ]; then
    echo "land: $remote/$branch is already at HEAD ($head); nothing to land"
    exit 0
fi

git push "$remote" "$head:refs/heads/$branch"
echo "land: $remote/$branch fast-forwarded $target -> $head"
