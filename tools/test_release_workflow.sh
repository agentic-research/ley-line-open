#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
workflow="$repo_root/.github/workflows/release.yml"
postflight="$repo_root/tools/verify_public_release_remote.sh"

test -f "$workflow"
test -f "$postflight"

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

# Binary-only releases keep the public schema module at SCHEMA_VERSION. Keep
# this contract executable so a future refactor cannot silently resolve the
# nested Go module at the binary release version again.
grep -q '^schema_version=' "$postflight"
grep -q 'SCHEMA_VERSION' "$postflight"
grep -q 'go get .*@v\$schema_version' "$postflight"

echo "release workflow has one post-build credentialed publisher"
