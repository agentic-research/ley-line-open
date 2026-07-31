#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
verifier="$repo_root/tools/verify_crate_publish_order.sh"

test -x "$verifier"

"$verifier"

schema_line=$(grep -n '^rs/ll-core/schema-spec/Cargo.toml$' \
    "$repo_root/tools/crate-publish-order.txt" | cut -d: -f1)
public_line=$(grep -n '^rs/ll-core/public-schema/Cargo.toml$' \
    "$repo_root/tools/crate-publish-order.txt" | cut -d: -f1)

test -n "$schema_line"
test -n "$public_line"
test "$schema_line" -lt "$public_line"

grep -qx 'rs/ll-core/schema-spec/Cargo.toml' \
    "$repo_root/tools/release-version-manifests.txt"
grep -Eq '^leyline-schema-spec = \{ path = "\.\./schema-spec", version = "=[0-9]+\.[0-9]+\.[0-9]+" \}$' \
    "$repo_root/rs/ll-core/public-schema/Cargo.toml"

echo "crate publish order fixture passed"
