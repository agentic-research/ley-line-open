#!/bin/sh
# Proves the workflow-parity gate fails on each drift it exists to catch
# (bead ley-line-open-2bea72). The acceptance criteria are the mutations the
# v0.12.x releases actually suffered: a dep install deleted from one file of
# the release pair, and a setup pin changed in one file but not the other.
# "Prove by making each edit, not by reading the gate" — this makes each edit
# on a copy of the REAL workflows, every run.
set -eu

repo_root=$(CDPATH='' cd -P -- "$(dirname "$0")/.." && pwd -P)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-workflow-parity.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

lint="$repo_root/tools/lint_workflow_parity.sh"

reset_fixture() {
    rm -rf "$tmp_dir/workflows"
    cp -R "$repo_root/.github/workflows" "$tmp_dir/workflows"
}

expect_pass() {
    if ! WORKFLOWS_DIR="$tmp_dir/workflows" "$lint" > /dev/null; then
        echo "workflow-parity gate failed on the unmutated workflows: $1" >&2
        exit 1
    fi
}

expect_fail() {
    if WORKFLOWS_DIR="$tmp_dir/workflows" "$lint" > /dev/null 2>&1; then
        echo "workflow-parity gate PASSED a mutation it exists to catch: $1" >&2
        exit 1
    fi
}

# The gate must pass the real, unmutated workflows — otherwise every assertion
# below proves nothing.
reset_fixture
expect_pass "baseline"

# Acceptance 1: deleting the capnp deps call from EITHER file fails.
reset_fixture
grep -v 'task deps:capnp' "$tmp_dir/workflows/release.yml" > "$tmp_dir/m" \
    && mv "$tmp_dir/m" "$tmp_dir/workflows/release.yml"
expect_fail "deps:capnp deleted from release.yml"

reset_fixture
grep -v 'task deps:capnp' "$tmp_dir/workflows/release-dryrun.yml" > "$tmp_dir/m" \
    && mv "$tmp_dir/m" "$tmp_dir/workflows/release-dryrun.yml"
expect_fail "deps:capnp deleted from release-dryrun.yml"

# Acceptance 2: changing the setup-task pin in one file and not the other fails.
reset_fixture
sed 's/version: 3\.[0-9.]*/version: 3.999.0/' "$tmp_dir/workflows/release.yml" > "$tmp_dir/m" \
    && mv "$tmp_dir/m" "$tmp_dir/workflows/release.yml"
expect_fail "setup-task pin diverged in release.yml only"

# A floating pin is drift waiting to happen even when both files carry it.
reset_fixture
for wf in "$tmp_dir/workflows"/*.yml; do
    sed 's/version: 3\.[0-9.]*/version: 3.x/' "$wf" > "$tmp_dir/m" && mv "$tmp_dir/m" "$wf"
done
expect_fail "setup-task pin floated to 3.x everywhere"

# The zig toolchain is the other install the image jobs cannot run without.
reset_fixture
grep -v 'task deps:zig' "$tmp_dir/workflows/release.yml" > "$tmp_dir/m" \
    && mv "$tmp_dir/m" "$tmp_dir/workflows/release.yml"
expect_fail "deps:zig deleted from release.yml image job"

# Reintroducing an inline install — the drift vector the deps:* move closed —
# must fail no matter which workflow it lands in.
reset_fixture
printf '      - name: sneak an inline install back in\n        run: sudo apt-get install -y capnproto\n' \
    >> "$tmp_dir/workflows/ci.yml"
expect_fail "inline apt-get install reintroduced in ci.yml"

reset_fixture
printf '      - name: sneak a cargo install back in\n        run: cargo install cargo-zigbuild --locked\n' \
    >> "$tmp_dir/workflows/release-dryrun.yml"
expect_fail "inline cargo install reintroduced in release-dryrun.yml"

echo "workflow-parity fixture proved the gate fails on every drift it guards against"
