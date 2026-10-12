#!/bin/sh
# Prove tools/clone_index.sh groups exact copies, leaves a renamed copy out,
# and does not count a function nested inside a reported copy a second time.
# Bead ley-line-open-6e9f79.
set -eu

repo_root=$(CDPATH='' cd -P -- "$(dirname "$0")/.." && pwd -P)
fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-clone-index-test.XXXXXX")
trap 'rm -rf "$fixture_dir"' 0 1 2 15

if [ -z "${LEYLINE:-}" ]; then
  (cd "$repo_root/rs" && cargo build -q -p leyline-cli)
  LEYLINE="$repo_root/rs/target/debug/leyline"
fi
export LEYLINE

src="$fixture_dir/src"
mkdir -p "$src"

# a.rs and b.rs both hold add_all and wrap token for token. b.rs also holds
# sum_every, which is add_all with every identifier renamed. wrap contains
# helper, so helper's two copies sit inside wrap's two copies.
common='fn add_all(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs {
        total += x;
    }
    total
}

fn wrap(n: i32) -> i32 {
    fn helper(n: i32) -> i32 {
        let mut k = n;
        while k > 10 {
            k -= 10;
        }
        k
    }
    helper(n) + 1
}'
printf '%s\n' "$common" > "$src/a.rs"
printf '%s\n\n%s\n' "$common" 'fn sum_every(ys: &[i32]) -> i32 {
    let mut acc = 0;
    for y in ys {
        acc += y;
    }
    acc
}' > "$src/b.rs"

report="$fixture_dir/report.md"
sh "$repo_root/tools/clone_index.sh" --out "$report" "$src"

fail() {
  printf 'clone-index fixture FAILED: %s\n' "$1" >&2
  cat "$report" >&2
  exit 1
}

grep -q '^2 groups of 6+ lines' "$report" || fail 'expected exactly two groups (add_all, wrap)'
grep -q "^- $src/a.rs:1\$" "$report" || fail 'a.rs add_all copy missing'
grep -q "^- $src/b.rs:1\$" "$report" || fail 'b.rs add_all copy missing'
grep -q "^- $src/a.rs:9\$" "$report" || fail 'a.rs wrap copy missing'
grep -q "^- $src/b.rs:9\$" "$report" || fail 'b.rs wrap copy missing'
grep -q 'b.rs:20' "$report" && fail 'renamed copy sum_every was grouped as an exact clone'
grep -q 'a.rs:10' "$report" && fail 'helper, nested inside both wrap copies, was reported twice'
grep -q '`fn wrap(n: i32) -> i32` (10 lines x 2 copies)' "$report" \
  || fail 'wrap should rank first with its signature, lines and copies'

printf 'clone index grouped exact copies and left the renamed and nested copies out\n'
