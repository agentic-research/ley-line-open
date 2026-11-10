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
# <dir>/rootfs. Build it somewhere durable (~/.cache/leyline/guest-tools):
# macOS empties /private/tmp periodically.
#
# Downloads are untrusted data. Each package is pinned by SHA-256 (the
# values the V-F3 benchmark recorded, docs/benchmarks/2026-10-15-vf3-
# virtiofs.md, except libexpat-2.10.0-r0, pinned on first fetch from Alpine's
# CDN when 2.9.0-r0 was deleted; apk signatures are not verified) and
# checked before it is unpacked; tar only extracts it; the
# binaries run only inside the guest. Nothing here builds or executes a
# downloaded file on the host.
#
# Where the pinned bytes come from (bead ley-line-open-ae6b99). Alpine keeps
# only the current build of each package, so a pin starts to 404 the day
# Alpine ships a newer one (libexpat-2.8.5-r0 did, then 2.9.0-r0). Each pin
# is therefore fetched by tools/fetch_pinned.sh from, in order: this
# machine's content-addressed store (~/.cache/leyline/pins/<sha256>, filled
# with every pin this script has verified), Alpine, and LLO's
# content-addressed mirror (the `guest-tools-pins` GitHub release, whose
# assets are named by their SHA-256 alone; it exists once a maintainer has
# run `tools/guest_tools_mirror.sh upload`). Every source is checked against
# the same pin, so the store and the mirror add availability and no trust. A
# pin missing from all three fails with a message naming it.
#
# Changing a pin: edit PINS in tools/guest_tools_pins.sh (and the matching
# `take` line below), run this script against Alpine, then
# `tools/guest_tools_mirror.sh upload <dir>` puts the new bytes on the mirror
# under their digest; `task guest-tools:mirror:check` confirms every pin is
# there. The mirror never replaces or deletes an asset, so an old pin stays
# fetchable.
set -eu

dir=${1:?usage: $0 <dir>}
case "$(uname -m)" in
    arm64 | aarch64) ;;
    *)
        echo "guest_tools: pinned for aarch64 guests only" >&2
        exit 2
        ;;
esac

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd -P)
. "$repo_root/tools/guest_tools_pins.sh"

mkdir -p "$dir/dl"
echo "$PINS" | while read -r path sum; do
    name=$(basename "$path")
    pkg="$dir/dl/${name%.apk}"
    pkg="${pkg%.tar.gz}"
    if [ ! -f "$pkg/$name" ] ||
        [ "$(shasum -a 256 "$pkg/$name" | cut -d' ' -f1)" != "$sum" ]; then
        rm -rf "$pkg"
        mkdir "$pkg"
        "$repo_root/tools/fetch_pinned.sh" "$pkg/$name" "$sum" \
            "file://$CAS/$sum" "$ALPINE/$path" "$MIRROR/$sum"
    fi
    # Keep the verified bytes under their digest, so this machine never
    # needs a source again for this pin.
    if [ ! -f "$CAS/$sum" ]; then
        mkdir -p "$CAS"
        cp "$pkg/$name" "$CAS/$sum.part"
        mv -f "$CAS/$sum.part" "$CAS/$sum"
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
take libexpat-2.10.0-r0 usr/lib/libexpat.so.1 usr/lib/libexpat.so.1
take libgcc-15.2.0-r5 usr/lib/libgcc_s.so.1 usr/lib/libgcc_s.so.1
take git-2.54.0-r0 usr/bin/git usr/bin/git
take ripgrep-15.1.0-r0 usr/bin/rg usr/bin/rg
chmod 0755 "$rootfs/lib/ld-musl-aarch64.so.1" "$rootfs/lib/libc.musl-aarch64.so.1" \
    "$rootfs/usr/bin/git" "$rootfs/usr/bin/rg"
chmod 0644 "$rootfs"/usr/lib/*
(cd "$rootfs" && find . -type f | sort | xargs shasum -a 256) > "$dir/rootfs.sha256"
echo "guest_tools: $rootfs"
cat "$dir/rootfs.sha256"
