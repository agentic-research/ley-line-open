#!/bin/sh
# Prove the published execution/v1 schema assets are consumable on their own
# (ley-line-open-6d811a).
#
# usage: test_execution_schema_asset.sh <asset-dir> <capnpc-schema-bridge-zod>
#
# <asset-dir> holds `execution.capnp` and `traits.capnp` exactly as a release
# publishes them. This script does what a consumer without an LLO checkout
# does: lay the two files out as the import graph expects, compile with the
# zod plugin, and check that the three execution/v1 structs come out. Only
# the asset directory and the plugin are inputs, so a schema that needs
# anything else to compile fails here instead of in a consumer's CI.
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <asset-dir> <capnpc-schema-bridge-zod>" >&2
    exit 2
fi

asset_dir=$1
zod_plugin=$2

fail() {
    echo "$*" >&2
    exit 1
}

test -f "$asset_dir/execution.capnp" || fail "missing asset: execution.capnp"
test -f "$asset_dir/traits.capnp" || fail "missing asset: traits.capnp"
test -x "$zod_plugin" || fail "zod plugin is not executable: $zod_plugin"
command -v capnp >/dev/null 2>&1 || fail "capnp is not installed (task deps:capnp)"

zod_plugin_abs=$(CDPATH='' cd -- "$(dirname "$zod_plugin")" && pwd)/$(basename "$zod_plugin")
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-execution-schema.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

# The layout execution.capnp's `import "../../_traits.capnp"` resolves against.
mkdir -p "$tmp_dir/schema/execution/v1" "$tmp_dir/out"
cp "$asset_dir/traits.capnp" "$tmp_dir/schema/_traits.capnp"
cp "$asset_dir/execution.capnp" "$tmp_dir/schema/execution/v1/execution.capnp"

(
    cd "$tmp_dir/schema"
    capnp compile --no-standard-import \
        -o"$zod_plugin_abs:$tmp_dir/out" \
        execution/v1/execution.capnp
)

generated="$tmp_dir/out/execution.zod.ts"
test -f "$generated" || fail "zod plugin did not emit execution.zod.ts"

for struct in RunSpec RunGrant RunReceipt; do
    grep -Eq "^export const ${struct}Schema[^A-Za-z0-9_]" "$generated" ||
        fail "generated zod has no ${struct}Schema"
done

echo "execution/v1 schema assets compile standalone to zod RunSpec/RunGrant/RunReceipt"
