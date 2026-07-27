#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-release-retry.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

# shellcheck source=tools/release_common.sh
. "$repo_root/tools/release_common.sh"

cat > "$tmp_dir/transient" <<'EOF'
#!/bin/sh
set -eu
count=0
test ! -f "$RETRY_COUNT" || count=$(cat "$RETRY_COUNT")
count=$((count + 1))
printf '%s\n' "$count" > "$RETRY_COUNT"
test "$count" -ge 3
EOF

cat > "$tmp_dir/permanent" <<'EOF'
#!/bin/sh
set -eu
count=0
test ! -f "$RETRY_COUNT" || count=$(cat "$RETRY_COUNT")
count=$((count + 1))
printf '%s\n' "$count" > "$RETRY_COUNT"
exit 23
EOF

chmod +x "$tmp_dir/transient" "$tmp_dir/permanent"

export RETRY_COUNT="$tmp_dir/transient-count"
release_retry 4 0 "transient fixture" "$tmp_dir/transient"
test "$(cat "$RETRY_COUNT")" -eq 3

export RETRY_COUNT="$tmp_dir/permanent-count"
status=0
release_retry 3 0 "permanent fixture" "$tmp_dir/permanent" || status=$?
test "$status" -eq 23
test "$(cat "$RETRY_COUNT")" -eq 3

echo "release retry fixture proved transient recovery and bounded failure"
