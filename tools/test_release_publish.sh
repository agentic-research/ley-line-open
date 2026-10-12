#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-release-publish.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

cat > "$tmp_dir/tag-ok" <<'EOF'
#!/bin/sh
set -eu
test "$1" = "0.10.4"
printf '%s\n' 0123456789abcdef0123456789abcdef01234567
EOF
cat > "$tmp_dir/tag-fail" <<'EOF'
#!/bin/sh
exit 1
EOF
# `run view` answers with the next line of $GH_VIEW_STATES; a line reading
# FAIL makes that query fail, as a rate-limited or dropped API call does.
cat > "$tmp_dir/gh-ok" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$GH_LOG"
case "$1 $2" in
    "run list") printf '%s\n' 4242 ;;
    "run view")
        test "$3" = "4242"
        n=0
        test ! -f "$GH_VIEW_COUNT" || n=$(cat "$GH_VIEW_COUNT")
        n=$((n + 1))
        printf '%s\n' "$n" > "$GH_VIEW_COUNT"
        state=$(sed -n "${n}p" "$GH_VIEW_STATES")
        test "$state" != FAIL || exit 1
        printf '%s\n' "$state"
        ;;
    *) exit 1 ;;
esac
EOF
cat > "$tmp_dir/gh-fail-list" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$GH_LOG"
exit 1
EOF
chmod +x \
    "$tmp_dir/tag-ok" \
    "$tmp_dir/tag-fail" \
    "$tmp_dir/gh-ok" \
    "$tmp_dir/gh-fail-list"

export GH_LOG="$tmp_dir/gh.log"
export GH_VIEW_COUNT="$tmp_dir/view-count"
export GH_VIEW_STATES="$tmp_dir/view-states"
export RELEASE_POLL_SECONDS=0
export RELEASE_RETRY_DELAY_SECONDS=0

reset() {
    rm -f "$GH_LOG" "$GH_VIEW_COUNT"
    printf '%s\n' "$@" > "$GH_VIEW_STATES"
}

# A run that queues, runs, and succeeds is waited out with one cheap status
# query per poll, and a failed query along the way is retried, not fatal:
# `gh run watch` re-fetched every job each 3 s and exhausted the API quota
# while v0.20.3's release run was still building.
reset 'queued ' 'in_progress ' FAIL 'completed success'
TAG_RELEASE_BIN="$tmp_dir/tag-ok" GH_BIN="$tmp_dir/gh-ok" \
    "$repo_root/tools/release_publish.sh" 0.10.4
grep -q '^run list .*--commit 0123456789abcdef0123456789abcdef01234567' \
    "$GH_LOG"
test "$(grep -c '^run view 4242 ' "$GH_LOG")" -eq 4
if grep -q '^run watch' "$GH_LOG"; then
    echo "publish watched the run with gh run watch" >&2
    exit 1
fi

# Any conclusion but success fails the publish.
reset 'in_progress ' 'completed failure'
if TAG_RELEASE_BIN="$tmp_dir/tag-ok" GH_BIN="$tmp_dir/gh-ok" \
    "$repo_root/tools/release_publish.sh" 0.10.4
then
    echo "failed release run unexpectedly reached success" >&2
    exit 1
fi

# A status query that keeps failing gives up rather than spinning forever.
reset FAIL FAIL FAIL FAIL FAIL FAIL FAIL FAIL FAIL FAIL
if TAG_RELEASE_BIN="$tmp_dir/tag-ok" GH_BIN="$tmp_dir/gh-ok" \
    "$repo_root/tools/release_publish.sh" 0.10.4
then
    echo "unanswerable status query unexpectedly reached success" >&2
    exit 1
fi

# A run that never completes times out.
reset 'in_progress ' 'in_progress ' 'in_progress ' 'in_progress '
if RELEASE_MAX_POLLS=3 TAG_RELEASE_BIN="$tmp_dir/tag-ok" \
    GH_BIN="$tmp_dir/gh-ok" "$repo_root/tools/release_publish.sh" 0.10.4
then
    echo "unfinished release run unexpectedly reached success" >&2
    exit 1
fi
test "$(grep -c '^run view 4242 ' "$GH_LOG")" -eq 3

reset 'completed success'
if TAG_RELEASE_BIN="$tmp_dir/tag-fail" GH_BIN="$tmp_dir/gh-ok" \
    "$repo_root/tools/release_publish.sh" 0.10.4
then
    echo "failed tag gate unexpectedly reached success" >&2
    exit 1
fi
test ! -e "$GH_LOG"

reset 'completed success'
if TAG_RELEASE_BIN="$tmp_dir/tag-ok" GH_BIN="$tmp_dir/gh-fail-list" \
    "$repo_root/tools/release_publish.sh" 0.10.4
then
    echo "failed run lookup unexpectedly reached success" >&2
    exit 1
fi
test "$(wc -l < "$GH_LOG" | tr -d ' ')" -eq 1

echo "release publish fixture proved exact-run polling, retried status queries, and failure stops"
