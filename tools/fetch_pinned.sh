#!/bin/sh
# Fetch one file pinned by SHA-256 from the first source that serves the
# pinned bytes (bead ley-line-open-ae6b99).
#
#   tools/fetch_pinned.sh <dest> <sha256> <url>...
#
# Each URL is tried in order. A source that answers with an error (Alpine
# deletes a superseded package build, so its URL starts to 404) or with
# different bytes (a rebuilt package under the same name) is skipped, and the
# next one is tried. The first download whose SHA-256 is the pin is moved to
# <dest>; nothing that failed the check is ever left there. When no source
# serves the pinned bytes, the script exits 1 naming the destination, the pin
# and what every source answered, so a rotted pin is reported as a missing
# pin rather than as a bare curl error.
#
# A URL may be file://, which tools/test_fetch_pinned.sh uses to prove each
# path without the network.
set -eu

dest=${1:?usage: $0 <dest> <sha256> <url>...}
sum=${2:?usage: $0 <dest> <sha256> <url>...}
shift 2
[ "$#" -gt 0 ] || {
    echo "usage: $0 <dest> <sha256> <url>..." >&2
    exit 2
}

name=$(basename "$dest")
part="$dest.part"
trap 'rm -f "$part"' 0 1 2 15
answers=
for url in "$@"; do
    rm -f "$part"
    # %{http_code} is 000 for a file:// URL and for a transport failure; the
    # exit status says which.
    status=0
    code=$(curl -sSL --retry 2 -o "$part" -w '%{http_code}' "$url" 2>/dev/null) || status=$?
    if [ "$status" -ne 0 ]; then
        answers="$answers
  $url: no answer (curl exit $status)"
    elif [ "$code" != 200 ] && [ "$code" != 000 ]; then
        answers="$answers
  $url: HTTP $code"
    else
        actual=$(shasum -a 256 "$part" | cut -d' ' -f1)
        if [ "$actual" = "$sum" ]; then
            mv -f "$part" "$dest"
            exit 0
        fi
        answers="$answers
  $url: served sha256 $actual"
    fi
done
echo "fetch_pinned: pin $name (sha256 $sum) is missing from every source:$answers
Re-pin it (and upload the new bytes to the mirror), or restore the pinned bytes to a source." >&2
exit 1
