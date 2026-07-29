#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-ci-attestation.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

fixture="$tmp_dir/repo"
mkdir -p "$fixture/tools"
cp "$repo_root/tools/ci_attestation.sh" "$fixture/tools/"
cp "$repo_root/tools/pre_push_ci.sh" "$fixture/tools/"

git -C "$fixture" init -q
git -C "$fixture" config user.name "CI Attestation Fixture"
git -C "$fixture" config user.email "ci-attestation@example.test"
git -C "$fixture" config commit.gpgsign false
printf '%s\n' "original" > "$fixture/tracked.txt"
git -C "$fixture" add tracked.txt tools
git -C "$fixture" commit -qm "fixture"

if "$fixture/tools/ci_attestation.sh" check; then
    echo "missing receipt unexpectedly verified" >&2
    exit 1
fi

index_before=$(git -C "$fixture" diff --cached --binary)
(
    cd "$fixture"
    tools/ci_attestation.sh begin
    tools/ci_attestation.sh finish
    tools/ci_attestation.sh check
    tools/ci_attestation.sh verify-head
)
test "$(git -C "$fixture" diff --cached --binary)" = "$index_before"

printf '%s\n' "edited after CI" > "$fixture/tracked.txt"
if (cd "$fixture" && tools/ci_attestation.sh check); then
    echo "post-CI edit unexpectedly matched receipt" >&2
    exit 1
fi
if (cd "$fixture" && tools/ci_attestation.sh verify-head); then
    echo "dirty tree unexpectedly matched HEAD" >&2
    exit 1
fi

git -C "$fixture" restore tracked.txt
(
    cd "$fixture"
    tools/ci_attestation.sh begin
    printf '%s\n' "changed during CI" > tracked.txt
    if tools/ci_attestation.sh finish; then
        echo "tree changed during CI but receipt was recorded" >&2
        exit 1
    fi
    if tools/ci_attestation.sh check; then
        echo "failed CI stability check left a usable receipt" >&2
        exit 1
    fi
)

# The normal workflow runs CI before commit. Committing the exact bytes must
# preserve the receipt because Git commit metadata is not a source input.
(
    cd "$fixture"
    tools/ci_attestation.sh begin
    tools/ci_attestation.sh finish
)
git -C "$fixture" add tracked.txt
git -C "$fixture" commit -qm "commit tested bytes"
(
    cd "$fixture"
    tools/ci_attestation.sh check
    tools/ci_attestation.sh verify-head
)

fake_task="$tmp_dir/fake-task"
cat > "$fake_task" <<'EOF'
#!/bin/sh
set -eu
test "$1" = "ci"
count=0
test ! -f "$TASK_COUNT" || count=$(cat "$TASK_COUNT")
printf '%s\n' "$((count + 1))" > "$TASK_COUNT"
tools/ci_attestation.sh begin
tools/ci_attestation.sh finish
EOF
chmod +x "$fake_task"

export TASK_COUNT="$tmp_dir/task-count"
(
    cd "$fixture"
    TASK_BIN="$fake_task" tools/pre_push_ci.sh
)
test ! -e "$TASK_COUNT"

printf '%s\n' "next committed tree" > "$fixture/tracked.txt"
git -C "$fixture" add tracked.txt
git -C "$fixture" commit -qm "invalidate receipt"
(
    cd "$fixture"
    TASK_BIN="$fake_task" tools/pre_push_ci.sh
    TASK_BIN="$fake_task" tools/pre_push_ci.sh
)
test "$(cat "$TASK_COUNT")" -eq 1

# The receipt attests "task ci passed for these INPUTS", and .beads/beads.jsonl
# is not an input: nothing task ci runs reads it (asserted below against the
# real repo). A bead filed while task ci runs must therefore not destroy the
# run (bead ley-line-open-080643 — this cost three full runs in one session).
mkdir -p "$fixture/.beads"
printf '%s\n' '{"id":"fixture-000001"}' > "$fixture/.beads/beads.jsonl"
git -C "$fixture" add .beads/beads.jsonl
git -C "$fixture" commit -qm "track bead export"
(
    cd "$fixture"
    tools/ci_attestation.sh begin
    printf '%s\n' '{"id":"fixture-000002"}' >> .beads/beads.jsonl
    if ! tools/ci_attestation.sh finish; then
        echo "bead export write during task ci destroyed the attestation" >&2
        exit 1
    fi
    tools/ci_attestation.sh check
)

# The exclusion is that one path, nothing wider: a source edit in the same
# window must still invalidate, even when accompanied by bead churn.
(
    cd "$fixture"
    tools/ci_attestation.sh begin
    printf '%s\n' '{"id":"fixture-000003"}' >> .beads/beads.jsonl
    printf '%s\n' "source edited during CI" > tracked.txt
    if tools/ci_attestation.sh finish; then
        echo "source edit hidden by concurrent bead churn was attested" >&2
        exit 1
    fi
)
git -C "$fixture" restore tracked.txt

# A tracked file elsewhere under .beads/ is NOT excluded — only beads.jsonl.
(
    cd "$fixture"
    printf '%s\n' "other" > .beads/other.txt
    git add .beads/other.txt
    git commit -qm "track another .beads file"
    tools/ci_attestation.sh begin
    printf '%s\n' "changed during CI" > .beads/other.txt
    if tools/ci_attestation.sh finish; then
        echo "exclusion is wider than .beads/beads.jsonl" >&2
        exit 1
    fi
)

# The exclusion rests on a fact that must stay true: no file task ci can
# execute reads .beads/. Assert it against the real repo, not the fixture,
# so a future subtask that consumes the export re-fails this gate instead of
# silently trusting an input the receipt no longer covers.
beads_readers=$(
    git -C "$repo_root" ls-files 'Taskfile.yml' '*/Taskfile.yml' 'tools/*.sh' '.github/workflows/*.yml' |
        grep -v -e '^tools/ci_attestation\.sh$' -e '^tools/test_ci_attestation\.sh$' |
        while IFS= read -r f; do
            grep -l '\.beads' "$repo_root/$f" || true
        done
)
if [ -n "$beads_readers" ]; then
    echo "task ci-reachable files reference .beads/ — the attestation exclusion is no longer sound:" >&2
    printf '%s\n' "$beads_readers" >&2
    exit 1
fi

echo "CI attestation fixture proved exact-tree reuse, fail-closed invalidation, and bead-export exclusion"
