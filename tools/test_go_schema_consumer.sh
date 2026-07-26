#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
schema_dir="$repo_root/clients/go/leyline-schema"
test -f "$schema_dir/LICENSE"
grep -q "Apache License" "$schema_dir/LICENSE"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-schema-consumer.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

(
    cd "$tmp_dir"
    go mod init example.test/leyline-schema-consumer >/dev/null
    go mod edit \
        -require=github.com/agentic-research/ley-line-open/clients/go/leyline-schema@v0.0.0
    go mod edit \
        -replace=github.com/agentic-research/ley-line-open/clients/go/leyline-schema="$schema_dir"

    cp "$repo_root/tools/fixtures/schema-consumer/consumer_test.go" .

    GOCACHE="$tmp_dir/gocache" GOTELEMETRY=off go test ./...
)

echo "external Go consumer compiled canonical daemon/wire API with Apache license"
