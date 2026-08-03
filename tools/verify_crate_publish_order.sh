#!/bin/sh
set -eu

repo_root=${REPO_ROOT:-$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)}
order_file=${CRATE_PUBLISH_ORDER_FILE:-$repo_root/tools/crate-publish-order.txt}
release_manifests=${RELEASE_VERSION_MANIFESTS_FILE:-$repo_root/tools/release-version-manifests.txt}
cargo_bin=${CARGO_BIN:-cargo}

cd "$repo_root"

test -f "$order_file" || {
    echo "crate publish order is missing: $order_file" >&2
    exit 1
}

schema_manifest=rs/ll-core/schema-spec/Cargo.toml
public_manifest=rs/ll-core/public-schema/Cargo.toml
schema_seen=0
public_seen=0

while IFS= read -r manifest; do
    case "$manifest" in
        ''|'#'*) continue ;;
    esac

    test -f "$manifest" || {
        echo "publish-order manifest is missing: $manifest" >&2
        exit 1
    }
    grep -Fqx "$manifest" "$release_manifests" || {
        echo "$manifest is publishable but absent from release-version-manifests.txt" >&2
        exit 1
    }

    case "$manifest" in
        "$schema_manifest") schema_seen=1 ;;
        "$public_manifest")
            test "$schema_seen" = 1 || {
                echo "$public_manifest must follow $schema_manifest" >&2
                exit 1
            }
            public_seen=1
            ;;
    esac
done < "$order_file"

test "$schema_seen" = 1 || {
    echo "$schema_manifest is absent from the crate publish order" >&2
    exit 1
}
test "$public_seen" = 1 || {
    echo "$public_manifest is absent from the crate publish order" >&2
    exit 1
}

schema_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' \
    "$schema_manifest" | head -n 1)
public_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' \
    "$public_manifest" | head -n 1)

test -n "$schema_version"
test "$schema_version" = "$public_version" || {
    echo "schema crate versions differ: schema-spec=$schema_version public-schema=$public_version" >&2
    exit 1
}

expected_dependency="leyline-schema-spec = { path = \"../schema-spec\", version = \"=$schema_version\" }"
grep -Fqx "$expected_dependency" "$public_manifest" || {
    echo "public-schema must exact-pin schema-spec $schema_version while retaining its local path" >&2
    exit 1
}

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-crate-order.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

# schema-spec is the first-publish bootstrap. Build its real .crate archive;
# this catches omitted package data before a registry credential is involved.
"$cargo_bin" package --manifest-path "$schema_manifest" \
    --allow-dirty --no-verify --target-dir "$tmp_dir/target"
"$cargo_bin" package --manifest-path "$schema_manifest" \
    --allow-dirty --list > "$tmp_dir/schema-files"

# The vector file is as load-bearing as the IDL: a consumer with no LLO
# checkout (cloister CI) pins conformance against these bytes, and shipping
# VECTORS.sha256 without the vector it pins is a dangling digest.
for required in \
    _traits.capnp \
    execution/v1/execution.capnp \
    execution/v1/VECTORS.sha256 \
    execution/v1/test-vectors/canonical-run.json
do
    grep -Fqx "$required" "$tmp_dir/schema-files" || {
        echo "schema-spec package omits canonical IDL asset: $required" >&2
        exit 1
    }
done

# Before schema-spec's first registry publication Cargo cannot resolve the
# consumer archive from crates.io. `--list` still exercises Cargo's packaged
# source selection; full public-schema packaging becomes the post-publication
# check, in this declared order.
"$cargo_bin" package --manifest-path "$public_manifest" \
    --allow-dirty --list > "$tmp_dir/public-files"
grep -Fqx build.rs "$tmp_dir/public-files"
grep -Fqx src/lib.rs "$tmp_dir/public-files"

echo "crate publish order verified: schema-spec $schema_version before public-schema $public_version"
