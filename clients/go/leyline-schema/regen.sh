#!/usr/bin/env bash
# regen.sh — regenerate Go bindings for every public LLO capnp schema.
#
# Source of truth lives in rs/ll-core/{schema-capnp/schemas,public-schema/capnp}/.
# This script invokes `capnp compile -ogo` against each .capnp file and drops
# the generated *.capnp.go into its sibling Go package directory.
#
# CI invariant (.github/workflows/leyline-schema-go.yml):
#   regen → `git diff --exit-code clients/go/leyline-schema/`
#
# So: re-run this whenever a .capnp file changes, then commit the diff.
#
# Tooling required (versions known to work):
#   - capnp (Cap'n Proto) >= 1.3.0
#   - capnpc-go from capnproto.org/go/capnp/v3@v3.1.0-alpha.2
#       go install capnproto.org/go/capnp/v3/capnpc-go@v3.1.0-alpha.2
#   - capnpc-go must be on $PATH (capnp shells out to it for `-ogo`).

set -euo pipefail

# Resolve repo root from this script's location (clients/go/leyline-schema/).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODULE_DIR="$SCRIPT_DIR"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Make sure capnpc-go is reachable. `capnp compile -ogo` shells out to
# `capnpc-go` on $PATH, so a bare `go install` isn't enough — the GOPATH
# bin dir must be on PATH.
if ! command -v capnpc-go >/dev/null 2>&1; then
    GOBIN="$(go env GOBIN)"
    [ -z "$GOBIN" ] && GOBIN="$(go env GOPATH)/bin"
    export PATH="$GOBIN:$PATH"
fi
if ! command -v capnpc-go >/dev/null 2>&1; then
    echo "regen.sh: capnpc-go not found on PATH." >&2
    echo "Install with: go install capnproto.org/go/capnp/v3/capnpc-go@v3.1.0-alpha.2" >&2
    exit 1
fi

# Schemas to regen, as `<absolute schema path>:<package basename>:<include dir>`.
# Include dir is where the vendored go.capnp lives so `import "/go.capnp"`
# resolves; capnpc-rust uses the same dir via build.rs::import_path.
SCHEMAS=(
    "$REPO_ROOT/rs/ll-core/schema-capnp/schemas/common.capnp:common:$REPO_ROOT/rs/ll-core/schema-capnp/schemas"
    "$REPO_ROOT/rs/ll-core/schema-capnp/schemas/ast.capnp:ast:$REPO_ROOT/rs/ll-core/schema-capnp/schemas"
    "$REPO_ROOT/rs/ll-core/schema-capnp/schemas/binding.capnp:binding:$REPO_ROOT/rs/ll-core/schema-capnp/schemas"
    "$REPO_ROOT/rs/ll-core/schema-capnp/schemas/cache.capnp:cache:$REPO_ROOT/rs/ll-core/schema-capnp/schemas"
    "$REPO_ROOT/rs/ll-core/schema-capnp/schemas/head.capnp:head:$REPO_ROOT/rs/ll-core/schema-capnp/schemas"
    "$REPO_ROOT/rs/ll-core/schema-capnp/schemas/source.capnp:source:$REPO_ROOT/rs/ll-core/schema-capnp/schemas"
    "$REPO_ROOT/rs/ll-core/public-schema/capnp/daemon.capnp:daemon:$REPO_ROOT/rs/ll-core/public-schema/capnp"
)

for entry in "${SCHEMAS[@]}"; do
    schema_path="${entry%%:*}"
    rest="${entry#*:}"
    pkg="${rest%%:*}"
    include_dir="${rest#*:}"

    out_dir="$MODULE_DIR/$pkg"
    mkdir -p "$out_dir"

    schema_dir="$(dirname "$schema_path")"
    schema_file="$(basename "$schema_path")"

    echo "regen: $schema_path -> $out_dir/${schema_file}.go"

    # capnp invocation:
    #   -I<include_dir>      : resolve `/go.capnp`
    #   --src-prefix=<dir>   : strip leading dirs so the output filename
    #                          is just `<schema>.capnp.go` (not the
    #                          full repo-rooted path).
    #   -ogo:<out_dir>       : write Go output under <out_dir>; the
    #                          $Go.package annotation pins the package
    #                          name, $Go.import pins the import path
    #                          for cross-schema references.
    (cd "$schema_dir" && capnp compile \
        -I "$include_dir" \
        --src-prefix="$schema_dir" \
        -ogo:"$out_dir" \
        "$schema_file")
done

# `go build` from the module root so cross-schema imports (binding/ast/head/
# source -> common) compile against the freshly-generated files. Catches
# the class of bug where annotations resolved but the generated Go has a
# missing import or stale identifier.
#
# GOWORK=off: this module is intentionally not in any parent `go.work`
# (multi-module monorepo pattern). A workspace file in an ancestor
# directory would otherwise refuse this build.
echo "regen: go build ./..."
(cd "$MODULE_DIR" && GOWORK=off go build ./...)

echo "regen: done."
