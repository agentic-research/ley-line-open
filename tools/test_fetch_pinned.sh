#!/bin/sh
# Prove tools/fetch_pinned.sh takes the pinned bytes from the first source
# that has them, falls back past a missing or wrong source, and fails naming
# the pin when no source has them (bead ley-line-open-ae6b99). Sources are
# file:// URLs, so this needs no network. Also proves every package
# tools/guest_tools.sh takes a file from is one tools/guest_tools_pins.sh
# pins, so a re-pin cannot change one line and not the other.
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd -P)
fetch="$repo_root/tools/fetch_pinned.sh"
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-fetch-pinned.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

fail() {
    echo "test_fetch_pinned: $*" >&2
    exit 1
}

printf 'the pinned bytes\n' > "$tmp_dir/pinned"
sum=$(shasum -a 256 "$tmp_dir/pinned" | cut -d' ' -f1)
mkdir "$tmp_dir/mirror" "$tmp_dir/primary"
cp "$tmp_dir/pinned" "$tmp_dir/mirror/$sum"
printf 'a rebuild under the same name\n' > "$tmp_dir/primary/pkg.apk"
gone="file://$tmp_dir/primary/deleted.apk"
mirror="file://$tmp_dir/mirror/$sum"

# The primary is gone (Alpine deleted the superseded build): the mirror serves.
"$fetch" "$tmp_dir/a.apk" "$sum" "$gone" "$mirror" 2>/dev/null ||
    fail "a missing primary did not fall back to the mirror"
cmp -s "$tmp_dir/a.apk" "$tmp_dir/pinned" || fail "the fallback wrote other bytes"

# The primary serves other bytes under the pinned name: skipped, not trusted.
"$fetch" "$tmp_dir/b.apk" "$sum" "file://$tmp_dir/primary/pkg.apk" "$mirror" 2>/dev/null ||
    fail "a primary serving other bytes did not fall back to the mirror"
cmp -s "$tmp_dir/b.apk" "$tmp_dir/pinned" || fail "the wrong primary's bytes were kept"

# The primary has the pin: it is used and the mirror is never needed.
cp "$tmp_dir/pinned" "$tmp_dir/primary/good.apk"
"$fetch" "$tmp_dir/c.apk" "$sum" "file://$tmp_dir/primary/good.apk" "$gone" 2>/dev/null ||
    fail "a primary with the pinned bytes was not used"

# No source has the pin: exit 1, nothing at the destination, and a message
# naming the pin and what each source answered.
if "$fetch" "$tmp_dir/d.apk" "$sum" "$gone" "file://$tmp_dir/primary/pkg.apk" 2> "$tmp_dir/err"; then
    fail "a pin no source has was reported fetched"
fi
[ ! -e "$tmp_dir/d.apk" ] || fail "a failed fetch left a file at the destination"
[ ! -e "$tmp_dir/d.apk.part" ] || fail "a failed fetch left its partial download"
grep -q "pin d.apk (sha256 $sum) is missing from every source" "$tmp_dir/err" ||
    fail "the failure does not name the missing pin: $(cat "$tmp_dir/err")"
grep -q "deleted.apk: no answer" "$tmp_dir/err" ||
    fail "the failure does not say the first source had nothing: $(cat "$tmp_dir/err")"
grep -q "pkg.apk: served sha256 " "$tmp_dir/err" ||
    fail "the failure does not say the second source served other bytes: $(cat "$tmp_dir/err")"

# Every `take <package> ...` in guest_tools.sh names a pinned package.
. "$repo_root/tools/guest_tools_pins.sh"
pinned=$(echo "$PINS" | while read -r path _; do
    name=$(basename "$path")
    name=${name%.apk}
    echo "${name%.tar.gz}"
done)
taken=$(sed -n 's/^take \([^ ]*\) .*/\1/p' "$repo_root/tools/guest_tools.sh" | sort -u)
[ -n "$taken" ] || fail "found no take lines in tools/guest_tools.sh"
for package in $taken; do
    echo "$pinned" | grep -qx "$package" ||
        fail "tools/guest_tools.sh takes from $package, which tools/guest_tools_pins.sh does not pin"
done

echo "test_fetch_pinned: ok"
