#!/bin/sh
# LLO's content-addressed mirror of the guest-tools pins (bead
# ley-line-open-ae6b99): the `guest-tools-pins` GitHub release, one asset per
# pin, named by the pin's SHA-256 alone.
#
#   tools/guest_tools_mirror.sh check          every pin is on the mirror,
#                                              with the pinned bytes
#   tools/guest_tools_mirror.sh upload <dir>   upload the pins a
#                                              tools/guest_tools.sh <dir> run
#                                              fetched and the mirror lacks
#
# `upload` needs `gh` with write access. The first upload creates the
# release, as a prerelease that is never marked latest, so nothing that
# resolves /releases/latest sees it. It never replaces an asset: a name is a
# digest, so an asset that exists already holds those bytes, and old pins
# stay fetchable after a re-pin. Until someone runs `upload`, the mirror
# does not exist and `check` fails for every pin.
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd -P)
. "$repo_root/tools/guest_tools_pins.sh"
RELEASE=guest-tools-pins

case "${1:-}" in
    check)
        work=$(mktemp -d "${TMPDIR:-/tmp}/leyline-guest-tools-mirror.XXXXXX")
        trap 'rm -rf "$work"' 0 1 2 15
        missing=0
        for sum in $(echo "$PINS" | cut -d' ' -f2); do
            if "$repo_root/tools/fetch_pinned.sh" "$work/$sum" "$sum" "$MIRROR/$sum"; then
                echo "mirror has $sum"
            else
                missing=1
            fi
        done
        exit "$missing"
        ;;
    upload)
        dir=${2:?usage: $0 upload <dir>}
        work=$(mktemp -d "${TMPDIR:-/tmp}/leyline-guest-tools-mirror.XXXXXX")
        trap 'rm -rf "$work"' 0 1 2 15
        # The first upload creates the release: a prerelease, never latest.
        if ! gh release view "$RELEASE" > /dev/null 2>&1; then
            gh release create "$RELEASE" --prerelease --latest=false \
                --title "guest-tools pins (content-addressed mirror)" \
                --notes "Content-addressed mirror of the tools/guest_tools.sh pins (bead ley-line-open-ae6b99). Each asset is named by its SHA-256; tools/fetch_pinned.sh checks it against the pin. Not a ley-line-open release."
        fi
        present=$(gh release view "$RELEASE" --json assets --jq '.assets[].name')
        echo "$PINS" | while read -r path sum; do
            if echo "$present" | grep -qx "$sum"; then
                echo "mirror has $sum ($(basename "$path"))"
                continue
            fi
            name=$(basename "$path")
            pkg="${name%.apk}"
            pkg="${pkg%.tar.gz}"
            src="$dir/dl/$pkg/$name"
            actual=$(shasum -a 256 "$src" | cut -d' ' -f1)
            if [ "$actual" != "$sum" ]; then
                echo "guest_tools_mirror: $src is $actual, pinned $sum" >&2
                exit 1
            fi
            cp "$src" "$work/$sum"
            gh release upload "$RELEASE" "$work/$sum"
            echo "uploaded $sum ($name)"
        done
        ;;
    *)
        echo "usage: $0 check | upload <dir>" >&2
        exit 2
        ;;
esac
