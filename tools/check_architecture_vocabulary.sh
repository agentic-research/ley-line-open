#!/bin/sh
set -eu

root=${1:-.}
files="$root/README.md $root/docs/ARCHITECTURE.md $root/docs/TABLE_CONTRACT.md"
failed=0

for forbidden in \
  'The .db file is the contract' \
  'The Σ substrate — runtime model' \
  'core tables are the canonical substrate'
do
  if grep -F "$forbidden" $files >/dev/null 2>&1; then
    printf 'forbidden architecture assertion: %s\n' "$forbidden" >&2
    failed=1
  fi
done

for required in \
  "Cap'n Proto segment root" \
  'SQLite arena snapshot root' \
  'blob hash' \
  'SQL projection ABI'
do
  if ! grep -F "$required" $files >/dev/null 2>&1; then
    printf 'missing architecture identity term: %s\n' "$required" >&2
    failed=1
  fi
done

exit "$failed"
