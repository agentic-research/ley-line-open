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

(
    cd "$OUTPUT_DIR"
    for file in *; do
        test -f "$file"
        sha256_file "$file"
    done | LC_ALL=C sort -k 2 > SHA256SUMS
)

echo "staged release artifacts in $OUTPUT_DIR"
