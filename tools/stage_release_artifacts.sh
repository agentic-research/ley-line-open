#!/bin/sh
set -eu

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    else
        shasum -a 256 "$1"
    fi
}

: "${OUTPUT_DIR:?OUTPUT_DIR is required}"
: "${CLI_SOURCE:?CLI_SOURCE is required}"
: "${ASSET_NAME:?ASSET_NAME is required}"

case "$OUTPUT_DIR" in
    "" | /)
        echo "refusing unsafe release output directory: '$OUTPUT_DIR'" >&2
        exit 1
        ;;
esac
case "$ASSET_NAME" in
    *[!A-Za-z0-9._-]*)
        echo "unsafe release asset name: $ASSET_NAME" >&2
        exit 1
        ;;
esac

if [ -e "$OUTPUT_DIR" ]; then
    echo "release output already exists: $OUTPUT_DIR" >&2
    exit 1
fi
test -f "$CLI_SOURCE"

if { [ -n "${STATICLIB_SOURCE:-}" ] && [ -z "${LIB_ASSET:-}" ]; } ||
    { [ -z "${STATICLIB_SOURCE:-}" ] && [ -n "${LIB_ASSET:-}" ]; }; then
    echo "STATICLIB_SOURCE and LIB_ASSET must be provided together" >&2
    exit 1
fi

mkdir -p "$OUTPUT_DIR"
cp "$CLI_SOURCE" "$OUTPUT_DIR/$ASSET_NAME"
chmod +x "$OUTPUT_DIR/$ASSET_NAME"

if [ -n "${STATICLIB_SOURCE:-}" ]; then
    case "$LIB_ASSET" in
        *[!A-Za-z0-9._-]*)
            echo "unsafe staticlib asset name: $LIB_ASSET" >&2
            exit 1
            ;;
    esac
    if [ "$LIB_ASSET" = "$ASSET_NAME" ] || [ "$LIB_ASSET" = "leyline_fs.h" ]; then
        echo "release asset names would collide: $LIB_ASSET" >&2
        exit 1
    fi
    test -f "$STATICLIB_SOURCE"
    cp "$STATICLIB_SOURCE" "$OUTPUT_DIR/$LIB_ASSET"
fi

if [ -n "${HEADER_SOURCE:-}" ]; then
    if [ "$ASSET_NAME" = "leyline_fs.h" ]; then
        echo "release asset names would collide: leyline_fs.h" >&2
        exit 1
    fi
    test -f "$HEADER_SOURCE"
    cp "$HEADER_SOURCE" "$OUTPUT_DIR/leyline_fs.h"
fi

# Generator binaries (ley-line-open-e44960 sibling gap).
#
# LLO ships generators that downstream repos RUN — the capnp plugins cloister
# invokes via `task cluster:zod`, the tooldefs plugin rosary uses, and the
# mcp-descriptor emitter. Until now none were published, so consumers had to
# SHA-pin a git dep and `cargo build` a build tool from source. v0.11.3 shipped
# the `8c00c6` zod fix and cloister still could not obtain it from the release:
# "released" and "deliverable" were different statements.
#
# Every named binary MUST exist. A missing one aborts the release rather than
# quietly shipping a smaller asset set — a partial publish is exactly the
# silent-success failure this whole change exists to remove.
if [ -n "${GENERATOR_BINS:-}" ]; then
    : "${GENERATOR_SOURCE_DIR:?GENERATOR_SOURCE_DIR is required with GENERATOR_BINS}"
    : "${GENERATOR_SUFFIX:?GENERATOR_SUFFIX is required with GENERATOR_BINS}"
    for generator_bin in $GENERATOR_BINS; do
        generator_src="$GENERATOR_SOURCE_DIR/$generator_bin"
        if [ ! -f "$generator_src" ]; then
            echo "generator binary not built: $generator_src" >&2
            echo "run 'task release:generators:target' for this BUILD_TARGET" >&2
            exit 1
        fi
        generator_asset="$generator_bin-$GENERATOR_SUFFIX"
        case "$generator_asset" in
            *[!A-Za-z0-9._-]*)
                echo "unsafe generator asset name: $generator_asset" >&2
                exit 1
                ;;
        esac
        if [ -e "$OUTPUT_DIR/$generator_asset" ]; then
            echo "release asset names would collide: $generator_asset" >&2
            exit 1
        fi
        cp "$generator_src" "$OUTPUT_DIR/$generator_asset"
        chmod +x "$OUTPUT_DIR/$generator_asset"
    done
fi

manifest_tmp="$OUTPUT_DIR/.SHA256SUMS.tmp.$$"
manifest_unsorted="$OUTPUT_DIR/.SHA256SUMS.unsorted.$$"
trap 'rm -f "$manifest_tmp" "$manifest_unsorted"' 0 1 2 15
(
    cd "$OUTPUT_DIR"
    for file in *; do
        test -f "$file"
        sha256_file "$file"
    done
) > "$manifest_unsorted"
LC_ALL=C sort -k 2 "$manifest_unsorted" > "$manifest_tmp"
mv "$manifest_tmp" "$OUTPUT_DIR/SHA256SUMS"
rm -f "$manifest_unsorted"
trap - 0 1 2 15

echo "staged release artifacts in $OUTPUT_DIR"
