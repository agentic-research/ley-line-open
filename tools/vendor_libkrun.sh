#!/bin/sh
# Vendored libkrun: the VMM LLO builds and runs (ADR-0044, bead
# ley-line-open-f1c46f).
#
# rs/third_party/libkrun is upstream libkrun at one pinned commit, cut to the
# files cargo builds, with rs/third_party/libkrun-patches/*.patch applied in
# order. This script is the only writer of that tree, and its `check` mode
# proves the committed tree is exactly that: a byte-for-byte diff against a
# fresh upstream checkout with the same patches applied.
#
#   tools/vendor_libkrun.sh sync    rewrite rs/third_party/libkrun from upstream
#   tools/vendor_libkrun.sh check   fail if rs/third_party/libkrun differs
#
# Both modes fetch from GitHub, so neither runs in `task ci`; the build itself
# reads only the committed tree and needs no network. `task vmm:vendor:check`
# runs `check`.
#
# To move to a new upstream release: change TAG and SHA, rebase the patches
# onto it (dropping any upstream has merged), run `sync`, and record the
# change in rs/third_party/README.md and CHANGELOG.md.
set -eu

URL=https://github.com/containers/libkrun.git
TAG=v1.19.4
SHA=728df8125077d0db44265f6e997c72b81b65c015

# What cargo needs, plus the license and the C header the Rust API mirrors.
# Not upstream's root Cargo.toml: it declares only upstream's own [workspace].
# The crates sit under rs/, whose workspace excludes rs/third_party, and a
# second workspace manifest there would also be a release manifest to
# tools/verify_release_version.sh. Cargo.lock stays as the record of the
# dependency versions upstream tested.
KEEP='AUTHORS Cargo.lock LICENSE README.md hvf-entitlements.plist include src'
# The prebuilt EDK2 firmware is read only with the `efi` feature, which LLO
# does not enable. Leaving it out keeps a 2 MiB binary nobody reviews out of
# the repository.
DROP='src/vmm/edk2/KRUN_EFI.silent.fd'

mode=${1:-}
case "$mode" in
    sync | check) ;;
    *)
        echo "usage: $0 sync|check" >&2
        exit 2
        ;;
esac

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd -P)
vendored="$repo_root/rs/third_party/libkrun"
patches="$repo_root/rs/third_party/libkrun-patches"
work=$(mktemp -d "${TMPDIR:-/tmp}/llo-vendor-libkrun.XXXXXX")
trap 'rm -rf "$work"' 0 1 2 15

git -c advice.detachedHead=false clone -q --depth 1 --branch "$TAG" "$URL" "$work/upstream"
actual=$(git -C "$work/upstream" rev-parse HEAD)
if [ "$actual" != "$SHA" ]; then
    echo "vendor-libkrun: $URL tag $TAG resolves to $actual, pinned $SHA." >&2
    echo "The tag moved. Refusing to vendor a commit nobody reviewed." >&2
    exit 1
fi

mkdir "$work/tree"
for path in $KEEP; do
    cp -R "$work/upstream/$path" "$work/tree/$path"
done
for path in $DROP; do
    rm -f "$work/tree/$path"
done
for patch in "$patches"/*.patch; do
    git -C "$work/tree" apply --whitespace=nowarn "$patch"
done

case "$mode" in
    sync)
        rm -rf "$vendored"
        cp -R "$work/tree" "$vendored"
        echo "vendor-libkrun: rs/third_party/libkrun is $TAG ($SHA) + $(ls "$patches"/*.patch | wc -l | tr -d ' ') patch(es)"
        ;;
    check)
        if ! diff -r "$work/tree" "$vendored"; then
            echo "vendor-libkrun: rs/third_party/libkrun is not $TAG ($SHA) + rs/third_party/libkrun-patches." >&2
            echo "Edit a patch and run '$0 sync'; never edit the vendored tree by hand." >&2
            exit 1
        fi
        echo "vendor-libkrun: rs/third_party/libkrun matches $TAG ($SHA) + patches"
        ;;
esac
