#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <version> <destination> <checksums-file>" >&2
    exit 2
fi

version=$1
destination=$2
checksums_file=$3

case "$(uname -s)" in
    Darwin) platform=apple-darwin ;;
    Linux) platform=unknown-linux-musl ;;
    *)
        echo "unsupported sccache host OS: $(uname -s)" >&2
        exit 1
        ;;
esac

case "$(uname -m)" in
    arm64 | aarch64) architecture=aarch64 ;;
    x86_64 | amd64) architecture=x86_64 ;;
    *)
        echo "unsupported sccache host architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

archive="sccache-v${version}-${architecture}-${platform}.tar.gz"
expected=$(
    awk -v archive="$archive" '$2 == archive { print $1 }' "$checksums_file"
)
if [ -z "$expected" ]; then
    echo "no pinned checksum for $archive" >&2
    exit 1
fi

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-sccache.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

url="https://github.com/mozilla/sccache/releases/download/v${version}/${archive}"
curl --fail --location --silent --show-error "$url" -o "$tmp_dir/$archive"

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp_dir/$archive" | awk '{ print $1 }')
else
    actual=$(shasum -a 256 "$tmp_dir/$archive" | awk '{ print $1 }')
fi
if [ "$actual" != "$expected" ]; then
    echo "checksum mismatch for $archive: expected $expected, got $actual" >&2
    exit 1
fi

mkdir "$tmp_dir/extract"
tar -xzf "$tmp_dir/$archive" -C "$tmp_dir/extract"
binary=$(find "$tmp_dir/extract" -type f -name sccache -print | head -n 1)
if [ -z "$binary" ]; then
    echo "$archive did not contain a sccache binary" >&2
    exit 1
fi

mkdir -p "$(dirname "$destination")"
install -m 0755 "$binary" "${destination}.tmp"
mv "${destination}.tmp" "$destination"
"$destination" --version
