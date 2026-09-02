#!/bin/sh
# Prove the diff mutation gate selects tests that can actually observe each
# changed surface. Runtime and CLI-library behavior lives in integration tests;
# the generic workspace slice remains lib-only for repo-relative fixtures.
set -eu

repo_root=$(CDPATH='' cd -P -- "$(dirname "$0")/.." && pwd -P)
fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-mutants-diff.XXXXXX")
trap 'rm -rf "$fixture_dir"' 0 1 2 15

mkdir -p "$fixture_dir/bin"
log="$fixture_dir/cargo.log"

cat > "$fixture_dir/bin/cargo" <<'SH'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$MUTANTS_FIXTURE_LOG"
case " $* " in
  *" --list "*)
    case "$*" in
      *cli-mutants-only.diff*)
        printf '%s\n' 'll-open/cli-lib/src/daemon/client.rs:65:9: fixture mutant'
        ;;
      *)
        printf '%s\n' 'll-open/runtime/src/authorization.rs:65:9: fixture mutant'
        ;;
    esac
    ;;
  *mixed-result.diff*)
    mkdir -p mutants.out
    printf '%s\n' 'll-open/runtime/src/service.rs:1:1: surviving fixture mutant' \
      > mutants.out/missed.txt
    exit 3
    ;;
esac
SH
chmod +x "$fixture_dir/bin/cargo"

cat > "$fixture_dir/pr.diff" <<'DIFF'
diff --git a/ll-open/runtime/src/authorization.rs b/ll-open/runtime/src/authorization.rs
--- a/ll-open/runtime/src/authorization.rs
+++ b/ll-open/runtime/src/authorization.rs
@@ -1 +1 @@
-old
+new
diff --git a/ll-open/cli-lib/src/daemon/client.rs b/ll-open/cli-lib/src/daemon/client.rs
--- a/ll-open/cli-lib/src/daemon/client.rs
+++ b/ll-open/cli-lib/src/daemon/client.rs
@@ -1 +1 @@
-old
+new
DIFF

PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/pr.diff"

assert_call() {
    if ! grep -E "$1" "$log" >/dev/null; then
        echo "missing cargo-mutants invocation: $2" >&2
        sed -n '1,120p' "$log" >&2
        exit 1
    fi
}

assert_call 'mutants .*--package leyline-runtime .*--test-workspace=false' \
    'runtime integration-test slice'
assert_call 'mutants .*--package leyline-cli-lib .*--test-workspace=false' \
    'CLI library integration-test slice'

if grep -E -- '--package leyline-runtime.* -C --lib' "$log" >/dev/null; then
    echo 'runtime mutation slice incorrectly excluded integration tests' >&2
    exit 1
fi

assert_call 'mutants .* -C --lib -C --test -C execution_client -C --test -C execution_transport .*--package leyline-cli-lib' \
    'CLI slice selects its lib and execution contract tests without repo-relative fixtures'

assert_call 'mutants .* -C --lib .*--exclude ll-open/runtime/\*\* .*--exclude ll-open/cli-lib/\*\* .*--exclude ll-open/cli/\*\*' \
    'generic lib-only slice excludes integration-tested packages'

: > "$log"
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/pr.diff" runtime
assert_call 'mutants .*--package leyline-runtime .*--test-workspace=false' \
    'runtime-only mutation scope'
if grep -E -- '--package leyline-cli-lib| -C --lib ' "$log" >/dev/null; then
    echo 'runtime-only scope invoked an unrelated mutation slice' >&2
    sed -n '1,120p' "$log" >&2
    exit 1
fi

cat > "$fixture_dir/cli-only.diff" <<'DIFF'
diff --git a/ll-open/cli-lib/src/daemon/client.rs b/ll-open/cli-lib/src/daemon/client.rs
--- a/ll-open/cli-lib/src/daemon/client.rs
+++ b/ll-open/cli-lib/src/daemon/client.rs
@@ -1 +1 @@
-old
+new
DIFF

if PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/cli-only.diff" runtime; then
    echo 'runtime-only scope falsely passed a CLI-only diff without running mutants' >&2
    exit 1
fi

cp "$fixture_dir/pr.diff" "$fixture_dir/cli-mutants-only.diff"
if PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/cli-mutants-only.diff" runtime; then
    echo 'runtime-only scope falsely passed when only the changed CLI package had mutants' >&2
    exit 1
fi

cat > "$fixture_dir/runtime-deletion.diff" <<'DIFF'
diff --git a/ll-open/runtime/src/obsolete.rs b/ll-open/runtime/src/obsolete.rs
deleted file mode 100644
--- a/ll-open/runtime/src/obsolete.rs
+++ /dev/null
@@ -1 +0,0 @@
-old
DIFF

: > "$log"
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
assert_call 'mutants .*--package leyline-runtime .*--test-workspace=false' \
    'Rust-only deletion routes to its package mutation slice'

cp "$fixture_dir/runtime-deletion.diff" "$fixture_dir/mixed-result.diff"
if (
    cd "$fixture_dir"
    PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
      "$repo_root/tools/mutants_diff.sh" "$fixture_dir/mixed-result.diff" runtime
); then
    echo 'timeout exit code falsely hid a surviving mutant report' >&2
    exit 1
fi

# A package excluded from the generic slice and covered by no slice of its own
# must fail loudly. Without the pairing check it enumerates mutants, runs the
# generic slice that skips it, matches nothing else, and exits 0 having tested
# nothing.
cat > "$fixture_dir/uncovered-package.diff" <<'DIFF'
diff --git a/ll-open/runtime/src/service.rs b/ll-open/runtime/src/service.rs
--- a/ll-open/runtime/src/service.rs
+++ b/ll-open/runtime/src/service.rs
@@ -1 +1 @@
-old
+new
DIFF

: > "$log"
MUTANTS_GENERIC_EXCLUDES='ll-open/nowhere' \
  PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/uncovered-package.diff" \
  || { echo 'pairing check fired for a package the diff does not touch' >&2; exit 1; }

: > "$log"
cat > "$fixture_dir/orphan-package.diff" <<'DIFF'
diff --git a/ll-open/orphan/src/lib.rs b/ll-open/orphan/src/lib.rs
--- a/ll-open/orphan/src/lib.rs
+++ b/ll-open/orphan/src/lib.rs
@@ -1 +1 @@
-old
+new
DIFF
if MUTANTS_GENERIC_EXCLUDES='ll-open/orphan' \
   PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
   "$repo_root/tools/mutants_diff.sh" "$fixture_dir/orphan-package.diff"; then
    echo 'a package excluded from every slice falsely passed the gate' >&2
    exit 1
fi

# The fs slice is the ONLY one that passes `--no-default-features`, which makes
# it the only place the ledger's standing exemption for default-enabled features
# ("cargo resolves them, mutants compiles them") is false. `fuse` and `nfs` ARE
# in leyline-fs's default set, so the ledger correctly declines to carry them —
# but this slice's own flags compile `fuse.rs` and `nfs.rs` out, while
# cargo-mutants (which parses with syn and never evaluates cfg) still enumerates
# every mutant inside them. Each then builds in 0s, every test trivially passes,
# and it reports MISSED: a phantom survivor on healthy code.
#
# A slice that opts out of default features owes an exclusion for what that
# choice removes. Their missing tests are a real gap owned by
# `ley-line-open-aed167`, so exclusion is the honest move here rather than
# enabling the features and reddening the gate on a coverage debt this gate did
# not incur.
: > "$log"
cat > "$fixture_dir/fs-mount.diff" <<'DIFF'
diff --git a/ll-open/fs/src/fuse.rs b/ll-open/fs/src/fuse.rs
--- a/ll-open/fs/src/fuse.rs
+++ b/ll-open/fs/src/fuse.rs
@@ -1 +1 @@
-old
+new
DIFF
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/fs-mount.diff"
assert_call 'mutants .*--package leyline-fs .*--exclude ll-open/fs/src/fuse\.rs' \
    'fs slice excludes the fuse module its --no-default-features compiles out'
assert_call 'mutants .*--package leyline-fs .*--exclude ll-open/fs/src/nfs\.rs' \
    'fs slice excludes the nfs module its --no-default-features compiles out'
assert_call 'mutants .*--package leyline-fs .*--exclude ll-open/fs/src/verified\.rs' \
    'fs slice excludes the verify module its feature set compiles out'

echo 'diff mutation fixture proved package-specific integration-test routing'
echo 'diff mutation fixture proved excluded packages must be claimed by a slice'
echo 'diff mutation fixture proved the fs slice excludes what it compiles out'
