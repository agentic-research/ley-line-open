#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/llo-architecture-vocabulary.XXXXXX")
trap 'rm -rf "$fixture"' EXIT HUP INT TERM
mkdir -p "$fixture/docs"
: > "$fixture/README.md"
: > "$fixture/docs/TABLE_CONTRACT.md"

printf '%s\n' \
  'The .db file is the contract' \
  'The Σ substrate — runtime model' \
  'core tables are the canonical substrate' > "$fixture/docs/ARCHITECTURE.md"

if sh "$repo/tools/check_architecture_vocabulary.sh" "$fixture" \
  >"$fixture/broken.out" 2>&1
then
  echo "broken architecture fixture unexpectedly passed" >&2
  exit 1
fi

for violation in \
  'The .db file is the contract' \
  'The Σ substrate — runtime model' \
  'core tables are the canonical substrate'
do
  grep -F "$violation" "$fixture/broken.out" >/dev/null || {
    printf 'linter did not name forbidden assertion: %s\n' "$violation" >&2
    exit 1
  }
done

printf '%s\n' \
  "Cap'n Proto segment root" \
  'SQLite arena snapshot root' \
  'blob hash' \
  'SQL projection ABI' > "$fixture/docs/ARCHITECTURE.md"

sh "$repo/tools/check_architecture_vocabulary.sh" "$fixture"
echo "architecture vocabulary fixtures passed"
