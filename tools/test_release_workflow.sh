#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
workflow="$repo_root/.github/workflows/release.yml"

test -f "$workflow"

if grep -q '^  create-release:' "$workflow"; then
    echo "release object must not be created before builds verify" >&2
    exit 1
fi
if grep -q 'needs: create-release' "$workflow"; then
    echo "build still depends on premature release creation" >&2
    exit 1
fi

awk '
  /^  [A-Za-z0-9_-]+:/ {
    job = $1
    sub(/:$/, "", job)
  }
  /contents: write/ {
    writers++
    writer = job
  }
  job == "publish" && /needs: build/ { publish_needs_build = 1 }
  END {
    exit !(writers == 1 && writer == "publish" && publish_needs_build)
  }
' "$workflow"

prepare_line=$(grep -n 'task release:artifacts:prepare' "$workflow" |
    cut -d: -f1)
publish_line=$(grep -n 'task release:artifacts:publish' "$workflow" |
    cut -d: -f1)
postflight_line=$(grep -n 'task release:verify-public' "$workflow" |
    cut -d: -f1)
test -n "$prepare_line"
test -n "$publish_line"
test -n "$postflight_line"
test "$prepare_line" -lt "$publish_line"
test "$publish_line" -lt "$postflight_line"

if grep -q 'gh release' "$workflow"; then
    echo "workflow bypasses the tested Taskfile publication boundary" >&2
    exit 1
fi

echo "release workflow has one post-build credentialed publisher"
