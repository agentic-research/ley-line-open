#!/bin/sh
# Proves tools/check_mutants_cfg_coverage.sh fails on each drift it exists to
# catch (bead ley-line-open-b23c41, clause 2). The guard's first version was a
# silent no-op — it reported success with an exclusion deliberately removed —
# which is exactly why a gate is not trusted until it has been watched go red.
#
# Every assertion runs the REAL lint against a copy of the REAL inputs it
# reads (the gate script, the fs crate root), with one edit applied. The
# lint derives its repo root from its own location, so the copy is a repo
# root; nothing here is a hand-built stand-in for the files under test.
set -eu

repo_root=$(CDPATH='' cd -P -- "$(dirname "$0")/.." && pwd -P)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-mutants-cfg.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

fixture="$tmp_dir/repo"
lint="$fixture/tools/check_mutants_cfg_coverage.sh"
gate="$fixture/tools/mutants_diff.sh"
fs_lib="$fixture/rs/ll-open/fs/src/lib.rs"

reset_fixture() {
    rm -rf "$fixture"
    mkdir -p "$fixture/tools" "$fixture/rs/ll-open/fs/src"
    cp "$repo_root/tools/check_mutants_cfg_coverage.sh" "$lint"
    cp "$repo_root/tools/mutants_diff.sh" "$gate"
    cp "$repo_root/rs/ll-open/fs/Cargo.toml" "$fixture/rs/ll-open/fs/Cargo.toml"
    cp "$repo_root/rs/ll-open/fs/src/lib.rs" "$fs_lib"
}

expect_pass() {
    if ! sh "$lint" > /dev/null; then
        echo "cfg-coverage gate failed on the unmutated inputs: $1" >&2
        exit 1
    fi
}

expect_fail() {
    if sh "$lint" > /dev/null 2>&1; then
        echo "cfg-coverage gate PASSED a mutation it exists to catch: $1" >&2
        exit 1
    fi
}

# The gate must pass the real, unmutated inputs — otherwise every assertion
# below proves nothing.
reset_fixture
expect_pass "baseline"

# The bead's own falsifier: a cfg-gated module appears in the crate root and
# the slice neither enables its feature nor excludes the file.
reset_fixture
printf '#[cfg(feature = "ghost")]\npub mod ghost;\n' >> "$fs_lib"
expect_fail "cfg(feature = \"ghost\") module added to fs without an exclude"

# The same module, honestly ENABLED by the slice: not a phantom, must pass.
reset_fixture
printf '#[cfg(feature = "ghost")]\npub mod ghost;\n' >> "$fs_lib"
sed 's/--features cdc,splice,validate/--features cdc,splice,validate,ghost/' "$gate" > "$tmp_dir/m" \
    && mv "$tmp_dir/m" "$gate"
grep -q 'validate,ghost' "$gate" || { echo "fixture: the enable mutation did not apply" >&2; exit 1; }
expect_pass "ghost module enabled by the slice's --features"

# The same module, honestly EXCLUDED by the slice: a recorded debt, must pass.
reset_fixture
printf '#[cfg(feature = "ghost")]\npub mod ghost;\n' >> "$fs_lib"
sed "s|--exclude 'll-open/fs/src/verified.rs'|--exclude 'll-open/fs/src/verified.rs' --exclude 'll-open/fs/src/ghost.rs'|" "$gate" > "$tmp_dir/m" \
    && mv "$tmp_dir/m" "$gate"
grep -q 'ghost.rs' "$gate" || { echo "fixture: the exclude mutation did not apply" >&2; exit 1; }
expect_pass "ghost module excluded by the slice"

# The drift the guard was written for: an existing exclusion deleted from the
# slice while the module stays gated off. The first version of the guard
# passed this silently.
reset_fixture
grep -v -- "--exclude 'll-open/fs/src/verified.rs'" "$gate" > "$tmp_dir/m" \
    && mv "$tmp_dir/m" "$gate"
grep -q -- "--exclude 'll-open/fs/src/verified.rs'" "$gate" \
    && { echo "fixture: the exclude removal did not apply" >&2; exit 1; }
expect_fail "--exclude of verified.rs deleted from the fs slice"

# A `--no-default-features` slice whose invocation the lint cannot find is a
# broken parse, not a clean bill: the lint must fail closed, not report
# "nothing to check". Rename the package on the invocation line so the
# manifest lookup misses.
reset_fixture
sed 's/--package leyline-fs --test-workspace=false/--package leyline-fs-renamed --test-workspace=false/' "$gate" > "$tmp_dir/m" \
    && mv "$tmp_dir/m" "$gate"
grep -q 'leyline-fs-renamed' "$gate" || { echo "fixture: the rename mutation did not apply" >&2; exit 1; }
expect_fail "the --no-default-features slice names a package with no manifest"

echo "mutants cfg-coverage fixture proved the gate fails on every drift it guards against"
