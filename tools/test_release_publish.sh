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
cat > "$tmp_dir/gh-ok" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$GH_LOG"
case "$1 $2" in
    "run list") printf '%s\n' 4242 ;;
    "run watch") test "$3" = "4242" ;;
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
TAG_RELEASE_BIN="$tmp_dir/tag-ok" GH_BIN="$tmp_dir/gh-ok" \
    "$repo_root/tools/release_publish.sh" 0.10.4
grep -q '^run list .*--commit 0123456789abcdef0123456789abcdef01234567' \
    "$GH_LOG"
grep -qx 'run watch 4242 --exit-status' "$GH_LOG"

rm "$GH_LOG"
if TAG_RELEASE_BIN="$tmp_dir/tag-fail" GH_BIN="$tmp_dir/gh-ok" \
    "$repo_root/tools/release_publish.sh" 0.10.4
then
    echo "failed tag gate unexpectedly reached success" >&2
    exit 1
fi
test ! -e "$GH_LOG"

if TAG_RELEASE_BIN="$tmp_dir/tag-ok" GH_BIN="$tmp_dir/gh-fail-list" \
    "$repo_root/tools/release_publish.sh" 0.10.4
then
    echo "failed run lookup unexpectedly reached success" >&2
    exit 1
fi
test "$(wc -l < "$GH_LOG" | tr -d ' ')" -eq 1

echo "release publish fixture proved exact-run watching and failure stops"
