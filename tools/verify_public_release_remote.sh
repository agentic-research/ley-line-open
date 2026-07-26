#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <version-without-v>" >&2
    exit 2
fi

version=$1
repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
module=github.com/agentic-research/ley-line-open/clients/go/leyline-schema
# shellcheck source=tools/release_common.sh
. "$repo_root/tools/release_common.sh"

release_validate_version "$version"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-public-postflight.XXXXXX")
cleanup() {
    chmod -R u+w "$tmp_dir" 2>/dev/null || true
    rm -rf "$tmp_dir"
}
trap cleanup 0 1 2 15
assets_dir="$tmp_dir/assets"
consumer_dir="$tmp_dir/consumer"
mkdir -p "$assets_dir" "$consumer_dir"

(
    cat "$repo_root/tools/release-assets.txt"
    echo SHA256SUMS
) > "$tmp_dir/download-assets"
while IFS= read -r asset; do
    curl --fail --location --retry 3 --silent --show-error \
        --output "$assets_dir/$asset" \
        "https://github.com/agentic-research/ley-line-open/releases/download/v$version/$asset"
done < "$tmp_dir/download-assets"

"$repo_root/tools/verify_public_release.sh" \
    "$assets_dir" "$repo_root/tools/release-assets.txt"

(
    cd "$consumer_dir"
    export GOCACHE="$tmp_dir/gocache"
    export GOPATH="$tmp_dir/gopath"
    export GOMODCACHE="$tmp_dir/gopath/pkg/mod"
    export GOTELEMETRY=off
    export GOPROXY=https://proxy.golang.org,direct
    go mod init example.test/public-leyline-schema-consumer >/dev/null
    go get "$module/daemon/wire@v$version"
    cp "$repo_root/tools/fixtures/schema-consumer/consumer_test.go" .
    go test ./...
    module_dir=$(go list -m -f '{{.Dir}}' "$module")
    test -f "$module_dir/LICENSE"
    grep -q "Apache License" "$module_dir/LICENSE"
)

echo "verified public assets and Apache schema module v$version"
