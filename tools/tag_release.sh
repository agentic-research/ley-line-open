#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <version-without-v>" >&2
    exit 2
fi

version=$1
script_root=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
repo_root=${REPO_ROOT:-$(CDPATH='' cd -- "$script_root/.." && pwd)}
git_bin=${GIT_BIN:-git}
verify_version_bin=${VERIFY_VERSION_BIN:-"$repo_root/tools/verify_release_version.sh"}
# shellcheck source=tools/release_common.sh
. "$script_root/release_common.sh"

release_validate_version "$version"

cd "$repo_root"
"$verify_version_bin" "$version"
"$git_bin" diff --quiet
"$git_bin" diff --cached --quiet
test "$("$git_bin" branch --show-current)" = "main"

"$git_bin" fetch origin main --tags
head_commit=$("$git_bin" rev-parse HEAD)
origin_main=$("$git_bin" rev-parse refs/remotes/origin/main)
if [ "$head_commit" != "$origin_main" ]; then
    echo "HEAD $head_commit is not origin/main $origin_main" >&2
    exit 1
fi

schema_version=$(sed -nE \
    's/^pub const SCHEMA_VERSION: &str = "([^"]+)";/\1/p' \
    rs/ll-open/cli-lib/src/daemon/version.rs)
release_validate_version "$schema_version"
if ! awk -v schema="$schema_version" -v binary="$version" '
    BEGIN {
      split(schema, s, ".")
      split(binary, b, ".")
      for (i = 1; i <= 3; i++) {
        if ((s[i] + 0) < (b[i] + 0)) exit 0
        if ((s[i] + 0) > (b[i] + 0)) exit 1
      }
      exit 0
    }
'
then
    echo "schema v$schema_version is newer than binary v$version" >&2
    exit 1
fi

ensure_local_tag_at_head() {
    tag=$1
    if "$git_bin" rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
        local_commit=$("$git_bin" rev-list -n 1 "$tag")
        if [ "$local_commit" != "$head_commit" ]; then
            echo "local tag $tag points at $local_commit, not $head_commit" >&2
            exit 1
        fi
    else
        "$git_bin" tag -a "$tag" -m "Release $tag" "$head_commit"
    fi
}

root_tag="v$version"
root_ref="refs/tags/$root_tag"
schema_tag="clients/go/leyline-schema/v$schema_version"
schema_ref="refs/tags/$schema_tag"
root_remote_commit=$(release_remote_tag_commit "$git_bin" origin "$root_ref")
schema_remote_commit=$(release_remote_tag_commit "$git_bin" origin "$schema_ref")

if [ -n "$root_remote_commit" ] &&
    [ "$root_remote_commit" != "$head_commit" ]
then
    echo "remote tag $root_tag points at $root_remote_commit, not $head_commit" >&2
    exit 1
fi

if [ "$schema_version" = "$version" ]; then
    if [ -n "$schema_remote_commit" ] &&
        [ "$schema_remote_commit" != "$head_commit" ]
    then
        echo "remote tag $schema_tag points at $schema_remote_commit, not $head_commit" >&2
        exit 1
    fi
    if [ -n "$root_remote_commit" ] && [ -z "$schema_remote_commit" ]; then
        echo "root tag exists without matching schema tag" >&2
        exit 1
    fi
else
    if [ -z "$schema_remote_commit" ]; then
        echo "unchanged schema tag $schema_tag is not published" >&2
        exit 1
    fi
    "$git_bin" diff --quiet "$schema_tag" -- clients/go/leyline-schema
fi

if [ -z "$root_remote_commit" ]; then
    ensure_local_tag_at_head "$root_tag"
    if [ -z "$schema_remote_commit" ]; then
        ensure_local_tag_at_head "$schema_tag"
        "$git_bin" push --atomic origin "$root_ref" "$schema_ref"
    else
        "$git_bin" push origin "$root_ref"
    fi
fi

echo "release tags verified at $head_commit" >&2
printf '%s\n' "$head_commit"
