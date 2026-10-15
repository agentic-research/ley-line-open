#!/bin/sh
# Guest tools for the agent-in-guest end-to-end test (ADR-0044 phase C, bead
# ley-line-open-f210c2): git and ripgrep for an aarch64 Linux guest, with the
# musl loader and the libraries they link, laid out as a rootfs fragment.
#
#   tools/guest_tools.sh <dir>
#
# writes <dir>/dl/<package>/ (one new directory per download), and
# <dir>/rootfs/{lib,usr/lib,usr/bin}/... with regular files only (the run's
# rootfs manifest admits no symbolic links). Point LEYLINE_GUEST_TOOLS at
# <dir>/rootfs.
#
# Downloads are untrusted data. Each package is pinned by SHA-256 (the
# values the V-F3 benchmark recorded, docs/benchmarks/2026-10-15-vf3-
# virtiofs.md) and checked before it is unpacked; tar only extracts it; the
# binaries run only inside the guest. Nothing here builds or executes a
# downloaded file on the host.
set -eu

dir=${1:?usage: $0 <dir>}
case "$(uname -m)" in
    arm64 | aarch64) ;;
    *)
        echo "guest_tools: pinned for aarch64 guests only" >&2
        exit 2
        ;;
esac

ALPINE=https://dl-cdn.alpinelinux.org/alpine/v3.24
# path  sha256
PINS='main/aarch64/git-2.54.0-r0.apk ea41e2f837f1f6a8b66e72a5ece0fc75cca44f76f49cf5500d467fc5627d0ab7
community/aarch64/ripgrep-15.1.0-r0.apk 1bf67cfa4464bc91e1dd1a4f537c40664c473ce92bf16ebba64bc7968d7545eb
main/aarch64/pcre2-10.49-r0.apk 03efd0b29f7f38deb10a9ccb54f56e19683eb7ac27eb1f347e6dbd2e59ea000d
main/aarch64/zlib-1.3.2-r1.apk 1f77a542ac6634761b9fe38d8bdca19b154071dcfa25432e73f2bc8a0740a33f
main/aarch64/libexpat-2.8.5-r0.apk 568a12a4889a00711d1c80c197a5540ee1cdec26c1c59da38f8342ad69e7eff9
main/aarch64/libgcc-15.2.0-r5.apk 369aaa6e9d099a737bad6dd3e6c2fe7bb1547ca26d22b94ee0411228f709b403
releases/aarch64/alpine-minirootfs-3.24.2-aarch64.tar.gz 9bf70a7f18ea44094cbb5f70c58f9af129c8214745743db0e68e5502cc2ce773'

mkdir -p "$dir/dl"
echo "$PINS" | while read -r path sum; do
    name=$(basename "$path")
    pkg="$dir/dl/${name%.apk}"
    pkg="${pkg%.tar.gz}"
    if [ ! -f "$pkg/$name" ]; then
        rm -rf "$pkg"
        mkdir "$pkg"
        curl -sSf -o "$pkg/$name" "$ALPINE/$path"
    fi
    actual=$(shasum -a 256 "$pkg/$name" | cut -d' ' -f1)
    if [ "$actual" != "$sum" ]; then
        echo "guest_tools: $name is $actual, pinned $sum" >&2
        exit 1
    fi
    echo "$sum  $name"
    mkdir -p "$pkg/x"
    tar -xzf "$pkg/$name" -C "$pkg/x" 2>/dev/null || true
done > "$dir/sha256.txt"
cat "$dir/sha256.txt"

# Regular files only: each wanted path, symbolic links resolved, from the
# package that ships it.
rootfs="$dir/rootfs"
rm -rf "$rootfs"
mkdir -p "$rootfs/lib" "$rootfs/usr/lib" "$rootfs/usr/bin"
take() { # package path-in-package destination
    cp -L "$dir/dl/$1/x/$2" "$rootfs/$3"
}
take alpine-minirootfs-3.24.2-aarch64 lib/ld-musl-aarch64.so.1 lib/ld-musl-aarch64.so.1
# Alpine binaries name libc by this soname; minirootfs ships it as a link.
take alpine-minirootfs-3.24.2-aarch64 lib/ld-musl-aarch64.so.1 lib/libc.musl-aarch64.so.1
take zlib-1.3.2-r1 usr/lib/libz.so.1 usr/lib/libz.so.1
take pcre2-10.49-r0 usr/lib/libpcre2-8.so.0 usr/lib/libpcre2-8.so.0
take libexpat-2.8.5-r0 usr/lib/libexpat.so.1 usr/lib/libexpat.so.1
take libgcc-15.2.0-r5 usr/lib/libgcc_s.so.1 usr/lib/libgcc_s.so.1
take git-2.54.0-r0 usr/bin/git usr/bin/git
take ripgrep-15.1.0-r0 usr/bin/rg usr/bin/rg
chmod 0755 "$rootfs/lib/ld-musl-aarch64.so.1" "$rootfs/lib/libc.musl-aarch64.so.1" \
    "$rootfs/usr/bin/git" "$rootfs/usr/bin/rg"
chmod 0644 "$rootfs"/usr/lib/*
(cd "$rootfs" && find . -type f | sort | xargs shasum -a 256) > "$dir/rootfs.sha256"
echo "guest_tools: $rootfs"
cat "$dir/rootfs.sha256"
