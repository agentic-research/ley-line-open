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
  *" metadata "*)
    # What drop_path_package_fingerprints reads: one path package, one
    # registry package.
    printf '%s\n' '{"packages":[{"name":"leyline-runtime","source":null},{"name":"serde","source":"registry+https://github.com/rust-lang/crates.io-index"}]}'
    ;;
  *" --list --json "*)
    # The unsharded listing a recording run picks its probes from.
    case "$*" in
      *baseline-none.diff*)
        printf '%s\n' '[]'
        ;;
      *baseline-generic.diff*)
        cat <<'JSON'
[
  {"name": "ll-open/cache/src/lib.rs:7:5: replace a", "package": "leyline-cache",
   "file": "ll-open/cache/src/lib.rs", "span": {"start": {"line": 7, "column": 5}}},
  {"name": "ll-open/cache/src/lib.rs:9:1: replace b", "package": "leyline-cache",
   "file": "ll-open/cache/src/lib.rs", "span": {"start": {"line": 9, "column": 1}}},
  {"name": "ll-open/vcs/src/a+b.rs:3:11: replace c", "package": "leyline-vcs",
   "file": "ll-open/vcs/src/a+b.rs", "span": {"start": {"line": 3, "column": 11}}}
]
JSON
        ;;
      *)
        cat <<'JSON'
[
  {"name": "ll-open/runtime/src/authorization.rs:65:9: fixture mutant", "package": "leyline-runtime",
   "file": "ll-open/runtime/src/authorization.rs", "span": {"start": {"line": 65, "column": 9}}},
  {"name": "ll-open/runtime/src/service.rs:1:1: fixture mutant", "package": "leyline-runtime",
   "file": "ll-open/runtime/src/service.rs", "span": {"start": {"line": 1, "column": 1}}}
]
JSON
        ;;
    esac
    ;;
  *" --re "*)
    # A recording run: the unmutated baseline plus its probes. It writes the
    # outcomes cargo-mutants 27.1.0 writes, and with --leak-dirs leaves its
    # build directory behind, a path package's and a registry package's
    # fingerprints in its target.
    mkdir -p mutants.out
    case " $* " in
      *" --leak-dirs "*)
        leaked="${TMPDIR:-/tmp}/cargo-mutants-fixture-leak.tmp"
        mkdir -p "$leaked/target/debug/.fingerprint/leyline-runtime-0123456789abcdef" \
                 "$leaked/target/debug/.fingerprint/serde-fedcba9876543210"
        ;;
    esac
    case "$*" in
      *baseline-broken.diff*)
        cat > mutants.out/outcomes.json <<'JSON'
{"outcomes": [{"scenario": "Baseline", "summary": "Failure",
  "phase_results": [{"phase": "Build", "duration": 61.5, "process_status": "Success"},
                    {"phase": "Test", "duration": 12.25, "process_status": {"Failure": 101}}]}]}
JSON
        exit 4
        ;;
    esac
    cat > mutants.out/outcomes.json <<JSON
{"outcomes": [
  {"scenario": "Baseline", "summary": "Success",
   "phase_results": [{"phase": "Build", "duration": 111.2, "process_status": "Success"},
                     {"phase": "Test", "duration": ${MUTANTS_FIXTURE_BASELINE_TEST:-37.1}, "process_status": "Success"}]},
  {"scenario": {"Mutant": {"name": "a probe"}}, "summary": "MissedMutant",
   "phase_results": [{"phase": "Build", "duration": 9.0, "process_status": "Success"},
                     {"phase": "Test", "duration": 999.0, "process_status": "Success"}]}
]}
JSON
    # A probe survived. That is the shards' finding to report, not this run's.
    exit 2
    ;;
  *" --list "*)
    case "$*" in
      *cli-mutants-only.diff*|*runtime-tests-only.diff*)
        printf '%s\n' 'll-open/cli-lib/src/daemon/client.rs:65:9: fixture mutant'
        ;;
      *plan-cli-binary.diff*)
        # What the real listing shows for the clap binary: `main` only.
        printf '%s\n' 'll-open/cli/src/main.rs:12:5: replace main with ()'
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

# MUTANTS_SHARD splits a slice across CI runners: it reaches cargo-mutants as
# --shard, and a malformed one is refused before anything runs.
: > "$log"
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" MUTANTS_SHARD=1/2 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/pr.diff" runtime
assert_call 'mutants .*--package leyline-runtime .*--shard 1/2' \
    'a sharded slice passes its shard to cargo-mutants'
for bad in 2/2 x/2 1/ /2 12; do
    if PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" MUTANTS_SHARD=$bad \
        "$repo_root/tools/mutants_diff.sh" "$fixture_dir/pr.diff" runtime 2>/dev/null; then
        echo "MUTANTS_SHARD=$bad was accepted" >&2
        exit 1
    fi
done

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

# A scoped leg that enumerates nothing from its own package has two causes
# that need opposite responses, exactly as the global check does: the paths do
# not resolve (the diff was generated from the wrong directory — MISCONFIGURED,
# whatever another package enumerated), or they resolve and the changed lines
# hold nothing cargo-mutants can mutate. Run from the repo root, where
# `ll-open/runtime/...` resolves to nothing, this is the first case.
cp "$fixture_dir/pr.diff" "$fixture_dir/cli-mutants-only.diff"
if PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/cli-mutants-only.diff" runtime; then
    echo 'runtime-only scope falsely passed when only the changed CLI package had mutants' >&2
    exit 1
fi

# The second case is what the first integration -> main promotion through the
# matrix hit (PR #386, ley-line-open-908b68): the diff changed ll-open/runtime
# only under tests/, the planner emitted the runtime scope because a Rust file
# in the package changed, and the leg reported MISCONFIGURED because
# cargo-mutants never enumerates integration tests. Run from a directory where
# the package's changed file resolves, the leg must report that nothing was
# mutable and exit 0 — nothing ran, and the message must not call it a pass.
cat > "$fixture_dir/runtime-tests-only.diff" <<'DIFF'
diff --git a/ll-open/runtime/tests/common/mod.rs b/ll-open/runtime/tests/common/mod.rs
--- a/ll-open/runtime/tests/common/mod.rs
+++ b/ll-open/runtime/tests/common/mod.rs
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
mkdir -p "$fixture_dir/ll-open/runtime/tests/common"
: > "$fixture_dir/ll-open/runtime/tests/common/mod.rs"
: > "$log"
out=$(
    cd "$fixture_dir"
    PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
      "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-tests-only.diff" runtime
) || {
    echo 'runtime scope failed a diff whose runtime changes are integration tests only' >&2
    exit 1
}
case "$out" in
    *"NO MUTABLE LINES"*"runtime"*) ;;
    *)
        echo 'runtime scope exited 0 on a tests-only diff without saying nothing was mutable' >&2
        printf '%s\n' "$out" >&2
        exit 1
        ;;
esac
if grep -E -- '--package leyline-runtime' "$log" >/dev/null; then
    echo 'runtime scope ran a mutation slice on a diff with nothing to mutate' >&2
    exit 1
fi

# Same diff, but the runtime path does not resolve from here: the tests-only
# outcome must not have loosened the path check.
if PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-tests-only.diff" runtime 2>/dev/null; then
    echo 'runtime scope passed a diff whose runtime path does not resolve' >&2
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

# The fs slice is the ONLY one that passes `--no-default-features`, so it must
# enable every gated module it mutates: cargo-mutants parses with syn, never
# evaluates cfg, and would report each mutant inside a compiled-out
# `verified.rs` as a phantom MISSED.
: > "$log"
cat > "$fixture_dir/fs-verify.diff" <<'DIFF'
diff --git a/ll-open/fs/src/verified.rs b/ll-open/fs/src/verified.rs
--- a/ll-open/fs/src/verified.rs
+++ b/ll-open/fs/src/verified.rs
@@ -1 +1 @@
-old
+new
DIFF
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/fs-verify.diff"
assert_call 'mutants .*--package leyline-fs .*--features cdc,validate,verify' \
    'fs slice enables the verify module it mutates'

# storefs's host mount is gated on its `mount` feature; storefs's own slice
# names it, so its adapter mutants compile and their lib tests run, and the
# generic slice leaves storefs alone.
: > "$log"
cat > "$fixture_dir/storefs-host.diff" <<'DIFF'
diff --git a/ll-open/storefs/src/host/nfs.rs b/ll-open/storefs/src/host/nfs.rs
--- a/ll-open/storefs/src/host/nfs.rs
+++ b/ll-open/storefs/src/host/nfs.rs
@@ -1 +1 @@
-old
+new
DIFF
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/storefs-host.diff"
assert_call 'mutants .*-C --lib .*--package leyline-storefs .*--features fuse' \
    'a storefs diff runs the storefs slice with the fuse feature (NFS and FUSE adapters), lib tests only'
if grep -q -- "--exclude ll-open/fs/\*\*.*--features" "$log"; then
    echo 'the generic slice was handed storefs features' >&2
    exit 1
fi

# A diff touching several generic-slice packages that carry enable rows —
# a release promotion's diff touches most of them. Each package's features
# must reach only that package's invocation: cargo rejects a `--features`
# name no SELECTED package declares, and a sharded slice selects only the
# packages of the mutants in its shard. One invocation carrying the union of
# every touched package's features (`interrupt,root-signer,hcl,pyproject`)
# failed every generic shard of the v0.21.0 promotion as BASELINE BROKEN
# (bead ley-line-open-c1f8bc).
cat > "$fixture_dir/generic-multi.diff" <<'DIFF'
diff --git a/ll-core/core/src/lib.rs b/ll-core/core/src/lib.rs
--- a/ll-core/core/src/lib.rs
+++ b/ll-core/core/src/lib.rs
diff --git a/ll-open/sign/src/lib.rs b/ll-open/sign/src/lib.rs
--- a/ll-open/sign/src/lib.rs
+++ b/ll-open/sign/src/lib.rs
diff --git a/ll-open/ts/src/lib.rs b/ll-open/ts/src/lib.rs
--- a/ll-open/ts/src/lib.rs
+++ b/ll-open/ts/src/lib.rs
diff --git a/ll-open/cache/src/lib.rs b/ll-open/cache/src/lib.rs
--- a/ll-open/cache/src/lib.rs
+++ b/ll-open/cache/src/lib.rs
DIFF
: > "$log"
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/generic-multi.diff" generic
assert_call "mutants .* -C --lib .*--file ll-core/core/\*\* --features interrupt,sqlite( |\$)" \
    'leyline-core is mutated alone, with both of its features'
assert_call "mutants .* -C --lib .*--file ll-open/sign/\*\* --features root-signer( |\$)" \
    'leyline-sign is mutated alone, with its own feature'
assert_call "mutants .* -C --lib .*--file ll-open/ts/\*\* --features hcl,go,pyproject( |\$)" \
    'leyline-ts is mutated alone, with its three features'
# Every feature-carrying generic invocation is confined to one package, and
# carries only that package's ledger features.
ledger_rows=$(awk '!/^[[:space:]]*(#|$)/ && $4 == "enable" { print $1, $2 }' \
    "$repo_root/tools/feature-ledger.txt")
grep -- ' --features ' "$log" | grep -v -- '--package ' > "$fixture_dir/featured.log" || true
while IFS= read -r call; do
    file_glob=$(printf '%s\n' "$call" | sed -n 's/.* --file \([^ ]*\)\/\*\* .*/\1/p')
    if [ -z "$file_glob" ]; then
        echo "a generic invocation carries features without naming its package: $call" >&2
        exit 1
    fi
    features=$(printf '%s\n' "$call" | sed -n 's/.* --features \([^ ]*\).*/\1/p')
    for feature in $(printf '%s' "$features" | tr ',' ' '); do
        if ! printf '%s\n' "$ledger_rows" | grep -qx "$file_glob $feature"; then
            echo "$file_glob was handed $feature, which is not its feature: $call" >&2
            exit 1
        fi
    done
done < "$fixture_dir/featured.log"
if [ "$(wc -l < "$fixture_dir/featured.log" | tr -d ' ')" -ne 3 ]; then
    echo 'expected one feature-carrying invocation per featured package:' >&2
    cat "$fixture_dir/featured.log" >&2
    exit 1
fi
# The rest of the generic slice (ll-open/cache here) runs once, featureless,
# with the packages mutated above excluded so no mutant is run twice.
assert_call "mutants .* -C --lib .*--exclude ll-open/runtime/\*\* .*--exclude ll-core/core/\*\* --exclude ll-open/sign/\*\* --exclude ll-open/ts/\*\*" \
    'the featureless remainder excludes the packages mutated with their features'
if grep -- '--exclude ll-core/core/\*\*' "$log" | grep -q -- '--features'; then
    echo 'the featureless remainder was handed features' >&2
    exit 1
fi

# A diff whose generic packages carry no enable rows is unchanged: exactly one
# featureless generic invocation.
cat > "$fixture_dir/generic-plain.diff" <<'DIFF'
diff --git a/ll-open/cache/src/lib.rs b/ll-open/cache/src/lib.rs
--- a/ll-open/cache/src/lib.rs
+++ b/ll-open/cache/src/lib.rs
DIFF
: > "$log"
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/generic-plain.diff" generic
if [ "$(grep -c -- ' -C --lib ' "$log")" -ne 1 ] || grep -q -- '--features\|--file ' "$log"; then
    echo 'a diff with no featured package did not run exactly one featureless generic invocation:' >&2
    cat "$log" >&2
    exit 1
fi

# A generic-slice package with no library target (rs/tools/smell-projector is
# one) cannot run under `-C --lib`: cargo finds no library target, and once
# sharding gives its mutants a shard of their own the slice is BASELINE
# BROKEN. It runs its binary's tests in an invocation of its own, and the
# library remainder excludes it. Run from a directory where both packages'
# manifests resolve, as the real gate runs from rs/.
mkdir -p "$fixture_dir/ws/tools/projector/src" "$fixture_dir/ws/ll-open/cache/src"
printf '[package]\nname = "projector"\n\n[[bin]]\nname = "projector"\npath = "src/main.rs"\n' \
    > "$fixture_dir/ws/tools/projector/Cargo.toml"
: > "$fixture_dir/ws/tools/projector/src/main.rs"
printf '[package]\nname = "cache"\n' > "$fixture_dir/ws/ll-open/cache/Cargo.toml"
: > "$fixture_dir/ws/ll-open/cache/src/lib.rs"
printf '[workspace]\n' > "$fixture_dir/ws/Cargo.toml"
cat > "$fixture_dir/generic-bin-only.diff" <<'DIFF'
diff --git a/tools/projector/src/main.rs b/tools/projector/src/main.rs
--- a/tools/projector/src/main.rs
+++ b/tools/projector/src/main.rs
diff --git a/ll-open/cache/src/lib.rs b/ll-open/cache/src/lib.rs
--- a/ll-open/cache/src/lib.rs
+++ b/ll-open/cache/src/lib.rs
DIFF
: > "$log"
(
    cd "$fixture_dir/ws"
    PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
      "$repo_root/tools/mutants_diff.sh" "$fixture_dir/generic-bin-only.diff" generic
)
assert_call 'mutants .* -C --bins .*--file tools/projector/\*\*' \
    'a binary-only package runs its binary tests in an invocation of its own'
assert_call 'mutants .* -C --lib .*--exclude tools/projector/\*\*' \
    'the library remainder excludes the binary-only package'
if grep -- ' -C --lib ' "$log" | grep -q -- '--file tools/projector/'; then
    echo 'a binary-only package was mutated under -C --lib' >&2
    exit 1
fi
if grep -q -- '--file ll-open/cache/\|--file \./' "$log"; then
    echo 'a library package or the workspace root was routed as binary-only:' >&2
    cat "$log" >&2
    exit 1
fi

# --------------------------------------------------------------------------
# One baseline per (run, scope) (bead ley-line-open-35780f): a recording run
# and the shards that reuse its record.
# --------------------------------------------------------------------------

record="$fixture_dir/baseline/record.txt"
mkdir -p "$fixture_dir/tmp"
# Runs from the fixture directory (cargo-mutants writes ./mutants.out) with a
# private TMPDIR, which is where a recording run looks for its build directory.
baseline_run() {
    (
        cd "$fixture_dir"
        PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" TMPDIR="$fixture_dir/tmp" \
          MUTANTS_BASELINE_RECORD="$record" "$@"
    )
}
# The invocations that ran mutants or a baseline: everything but a listing.
runs_in_log() {
    grep -v -- ' --list' "$log" || true
}

# With nothing set, a slice runs its own baseline and takes its timeout from
# it: no baseline flag, no timeout. That is `task rs:mutants:pr`.
: > "$log"
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
if grep -q -- '--baseline\|--timeout\|--re \|--copy-target\|--leak-dirs\|--jobs' "$log"; then
    echo 'a run with no baseline mode passed a baseline, timeout or probe flag:' >&2
    cat "$log" >&2
    exit 1
fi

# record: the baseline runs once, unsharded, with one probe location per
# mutated package and one build directory. A surviving probe (the fake's
# exit 2) is not the recording run's failure.
: > "$log"
baseline_run env MUTANTS_BASELINE=record \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
assert_call 'mutants .*--package leyline-runtime .*--list --json' \
    'a recording run lists its slice to pick probes'
assert_call 'mutants .*--package leyline-runtime .*--re \^ll-open/runtime/src/authorization\\\.rs:65:9:  --jobs 1$' \
    'a recording run probes one location of the one mutated package, in one build directory'
if [ "$(runs_in_log | grep -c -- ' --re ')" -ne 1 ] || [ "$(runs_in_log | wc -l | tr -d ' ')" -ne 1 ]; then
    echo 'a recording run did not run exactly one baseline invocation:' >&2
    cat "$log" >&2
    exit 1
fi
if runs_in_log | grep -q -- '--baseline\|--timeout\|--shard\|--leak-dirs\|--copy-target'; then
    echo 'a recording run skipped its baseline, sharded, or kept a target unasked:' >&2
    cat "$log" >&2
    exit 1
fi
# The timeout is cargo-mutants' rule on the recorded test time: 37.1 s x 5,
# rounded up, above the 20 s default minimum.
if ! grep -qE '^slice [0-9]+-[0-9]+ ok 186 111\.2 37\.1$' "$record" \
    || [ "$(wc -l < "$record" | tr -d ' ')" -ne 2 ] || ! grep -qE '^diff [0-9]+-[0-9]+$' "$record"; then
    echo 'the baseline record is not one diff line and one passing slice with a 186 s timeout:' >&2
    cat "$record" >&2
    exit 1
fi

# reuse: every shard skips the baseline and carries the recorded timeout.
for shard in 0/4 3/4; do
    : > "$log"
    baseline_run env MUTANTS_BASELINE=reuse MUTANTS_SHARD=$shard \
      "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
    assert_call "mutants .*--package leyline-runtime .*--baseline=skip --timeout 186 --shard $shard\$" \
        "shard $shard reuses the recorded baseline and its timeout"
    if runs_in_log | grep -q -- '--re \|--jobs\|--copy-target'; then
        echo "shard $shard ran probes, changed the job count, or copied a target unasked:" >&2
        cat "$log" >&2
        exit 1
    fi
done

# The minimum is cargo-mutants' too: a short baseline is floored at
# CARGO_MUTANTS_MINIMUM_TEST_TIMEOUT (20 s when unset), and the multiplier
# is .cargo/mutants.toml's `timeout_multiplier` when it sets one.
recorded_timeout() {
    sed -n 's/^slice [^ ]* ok \([0-9]*\) .*/\1/p' "$record"
}
baseline_run env MUTANTS_BASELINE=record MUTANTS_FIXTURE_BASELINE_TEST=2.01 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
[ "$(recorded_timeout)" = 20 ] || { echo "a 2.01 s baseline did not floor at 20 s: $(recorded_timeout)" >&2; exit 1; }
baseline_run env MUTANTS_BASELINE=record MUTANTS_FIXTURE_BASELINE_TEST=2.01 CARGO_MUTANTS_MINIMUM_TEST_TIMEOUT=60 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
[ "$(recorded_timeout)" = 60 ] || { echo "the minimum test timeout was not honoured: $(recorded_timeout)" >&2; exit 1; }
baseline_run env MUTANTS_BASELINE=record MUTANTS_FIXTURE_BASELINE_TEST=20.01 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
[ "$(recorded_timeout)" = 101 ] || { echo "20.01 s x 5 did not round up to 101 s: $(recorded_timeout)" >&2; exit 1; }
mkdir -p "$fixture_dir/.cargo"
printf 'timeout_multiplier = 2.5\n' > "$fixture_dir/.cargo/mutants.toml"
baseline_run env MUTANTS_BASELINE=record MUTANTS_FIXTURE_BASELINE_TEST=40 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
[ "$(recorded_timeout)" = 100 ] || { echo "the configured timeout multiplier was not used: $(recorded_timeout)" >&2; exit 1; }
rm -rf "$fixture_dir/.cargo"

# A multi-package slice probes each package once, so its baseline tests what
# an unsharded run's would; a path is matched literally, not as a regex.
cat > "$fixture_dir/baseline-generic.diff" <<'DIFF'
diff --git a/ll-open/cache/src/lib.rs b/ll-open/cache/src/lib.rs
--- a/ll-open/cache/src/lib.rs
+++ b/ll-open/cache/src/lib.rs
diff --git a/ll-open/vcs/src/a+b.rs b/ll-open/vcs/src/a+b.rs
--- a/ll-open/vcs/src/a+b.rs
+++ b/ll-open/vcs/src/a+b.rs
DIFF
: > "$log"
baseline_run env MUTANTS_BASELINE=record \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/baseline-generic.diff" generic
assert_call 'mutants .* -C --lib .*--re \^ll-open/cache/src/lib\\\.rs:7:5:  --re \^ll-open/vcs/src/a\\\+b\\\.rs:3:11:  --jobs 1$' \
    'a two-package slice probes each package once, with its path escaped'

# A slice with no mutants has no baseline to run, and says so in the record;
# its shards run as they always did.
cat > "$fixture_dir/baseline-none.diff" <<'DIFF'
diff --git a/ll-open/cache/src/lib.rs b/ll-open/cache/src/lib.rs
--- a/ll-open/cache/src/lib.rs
+++ b/ll-open/cache/src/lib.rs
DIFF
: > "$log"
baseline_run env MUTANTS_BASELINE=record \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/baseline-none.diff" generic
if [ -n "$(runs_in_log)" ] || ! grep -qE '^slice [0-9]+-[0-9]+ none$' "$record"; then
    echo 'a slice with no mutants ran a baseline or was not recorded as empty:' >&2
    cat "$log" "$record" >&2
    exit 1
fi
: > "$log"
baseline_run env MUTANTS_BASELINE=reuse MUTANTS_SHARD=0/4 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/baseline-none.diff" generic
if runs_in_log | grep -q -- '--baseline\|--timeout'; then
    echo 'a shard skipped a baseline that was never run:' >&2
    cat "$log" >&2
    exit 1
fi

# A failing baseline fails the recording run AND every shard of the scope,
# with the code a broken baseline has always had, and no shard runs a mutant:
# with the baseline skipped, cargo-mutants would report each one caught.
cp "$fixture_dir/runtime-deletion.diff" "$fixture_dir/baseline-broken.diff"
: > "$log"
rc=0
baseline_run env MUTANTS_BASELINE=record \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/baseline-broken.diff" runtime 2>/dev/null || rc=$?
if [ "$rc" -ne 4 ] || ! grep -qE '^slice [0-9]+-[0-9]+ failed$' "$record"; then
    echo "a failing baseline was not recorded as failed with exit 4 (exit $rc):" >&2
    cat "$record" >&2
    exit 1
fi
for shard in 0/4 1/4 2/4 3/4; do
    : > "$log"
    rc=0
    out=$(baseline_run env MUTANTS_BASELINE=reuse MUTANTS_SHARD=$shard \
      "$repo_root/tools/mutants_diff.sh" "$fixture_dir/baseline-broken.diff" runtime 2>&1) || rc=$?
    if [ "$rc" -ne 4 ] || [ -n "$(runs_in_log)" ]; then
        echo "shard $shard of a scope whose baseline failed exited $rc, or ran mutants:" >&2
        cat "$log" >&2
        exit 1
    fi
    case "$out" in
        *"BASELINE BROKEN"*) ;;
        *) echo "shard $shard did not say the baseline was broken: $out" >&2; exit 1 ;;
    esac
done

# Nor does a shard run on anything else that is not a recorded pass for this
# diff and this slice: no record (the baseline job died), a record of another
# diff, a record without this slice.
expect_baseline_missing() {
    : > "$log"
    rc=0
    out=$(baseline_run env MUTANTS_BASELINE=reuse MUTANTS_SHARD=0/4 \
      "$repo_root/tools/mutants_diff.sh" "$2" runtime 2>&1) || rc=$?
    if [ "$rc" -ne 4 ] || [ -n "$(runs_in_log)" ]; then
        echo "a shard ran with $1 (exit $rc):" >&2
        cat "$log" >&2
        exit 1
    fi
    case "$out" in
        *"BASELINE MISSING"*) ;;
        *) echo "a shard with $1 did not say the baseline was missing: $out" >&2; exit 1 ;;
    esac
}
baseline_run env MUTANTS_BASELINE=record \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
expect_baseline_missing 'the record of another diff' "$fixture_dir/uncovered-package.diff"
grep -v '^slice ' "$record" > "$record.tmp"
mv -f "$record.tmp" "$record"
expect_baseline_missing 'a record that lacks its slice' "$fixture_dir/runtime-deletion.diff"
rm -f "$record"
expect_baseline_missing 'no record' "$fixture_dir/runtime-deletion.diff"

# Misuse is refused before anything runs.
refused() {
    what=$1
    shift
    : > "$log"
    if baseline_run env "$@" \
        "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime 2>/dev/null; then
        echo "$what was accepted" >&2
        exit 1
    fi
    if [ -s "$log" ]; then
        echo "$what was refused only after cargo ran:" >&2
        cat "$log" >&2
        exit 1
    fi
}
refused 'an unknown baseline mode' MUTANTS_BASELINE=skip
refused 'a sharded recording run' MUTANTS_BASELINE=record MUTANTS_SHARD=0/4
refused 'a baseline mode without a record path' MUTANTS_BASELINE=record MUTANTS_BASELINE_RECORD=
refused 'a seeded target outside CI' -u CI MUTANTS_BASELINE=record MUTANTS_SEED_TARGET=1
refused 'a seeded target without a baseline mode' CI=true MUTANTS_SEED_TARGET=1
refused 'a seeded target that is not 1' CI=true MUTANTS_BASELINE=record MUTANTS_SEED_TARGET=yes

# MUTANTS_SEED_TARGET (CI only): the recording run keeps its build directory's
# target as ./target with every path package's fingerprint deleted, so only
# registry dependencies are reusable, and removes the build directory. The
# shards copy ./target into their build directories.
rm -rf "$fixture_dir/target"
: > "$log"
baseline_run env CI=true MUTANTS_BASELINE=record MUTANTS_SEED_TARGET=1 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
assert_call 'mutants .*--package leyline-runtime .*--jobs 1 --leak-dirs$' \
    'a seeding recording run keeps its build directory'
if [ ! -d "$fixture_dir/target/debug/.fingerprint/serde-fedcba9876543210" ]; then
    echo "the baseline's registry dependency build was not kept in ./target" >&2
    exit 1
fi
if [ -e "$fixture_dir/target/debug/.fingerprint/leyline-runtime-0123456789abcdef" ]; then
    echo "a path package's fingerprint survived into the kept target: a probe mutant's build could be reused" >&2
    exit 1
fi
if [ -e "$fixture_dir/tmp/cargo-mutants-fixture-leak.tmp" ]; then
    echo 'the recording run left its build directory behind' >&2
    exit 1
fi
# The next slice of a recording run, and every shard, start from that target.
: > "$log"
baseline_run env CI=true MUTANTS_BASELINE=record MUTANTS_SEED_TARGET=1 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
assert_call 'mutants .*--package leyline-runtime .*--jobs 1 --leak-dirs --copy-target=true$' \
    'a later recording slice builds on the kept target'
: > "$log"
baseline_run env CI=true MUTANTS_BASELINE=reuse MUTANTS_SEED_TARGET=1 MUTANTS_SHARD=2/4 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
assert_call 'mutants .*--package leyline-runtime .*--baseline=skip --timeout 186 --copy-target=true --shard 2/4$' \
    'a seeded shard copies the kept target into its build directories'
rm -rf "$fixture_dir/target"
: > "$log"
baseline_run env CI=true MUTANTS_BASELINE=reuse MUTANTS_SEED_TARGET=1 MUTANTS_SHARD=2/4 \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/runtime-deletion.diff" runtime
if runs_in_log | grep -q -- '--copy-target'; then
    echo 'a shard with no kept target asked cargo-mutants to copy one' >&2
    exit 1
fi

# --------------------------------------------------------------------------
# `list-scopes`: the plan the CI matrix is built from. Runs no cargo.
# --------------------------------------------------------------------------

plan_is() {
    got=$(PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
        "$repo_root/tools/mutants_diff.sh" "$1" list-scopes | tr '\n' ' ' | sed 's/ $//')
    if [ "$got" != "$2" ]; then
        echo "plan for $1: expected '$2', got '$got'" >&2
        exit 1
    fi
}

# A diff touching an owned package and an unowned one plans both legs.
cat > "$fixture_dir/plan-mixed.diff" <<'DIFF'
diff --git a/ll-open/runtime/src/authorization.rs b/ll-open/runtime/src/authorization.rs
--- a/ll-open/runtime/src/authorization.rs
+++ b/ll-open/runtime/src/authorization.rs
diff --git a/ll-core/core/src/partition.rs b/ll-core/core/src/partition.rs
--- a/ll-core/core/src/partition.rs
+++ b/ll-core/core/src/partition.rs
DIFF
plan_is "$fixture_dir/plan-mixed.diff" 'generic runtime'

# A diff confined to owned packages must NOT plan the generic leg — that leg
# would enumerate nothing and the whole point is to stop paying for it.
cat > "$fixture_dir/plan-owned-only.diff" <<'DIFF'
diff --git a/ll-open/fs/src/graph.rs b/ll-open/fs/src/graph.rs
--- a/ll-open/fs/src/graph.rs
+++ b/ll-open/fs/src/graph.rs
DIFF
plan_is "$fixture_dir/plan-owned-only.diff" 'fs'

# ll-open/cli has no slice of its own; it is claimed inside the cli scope, so a
# cli-binary-only diff must still plan `cli` or the pairing assertion below
# would have nothing to satisfy it.
cat > "$fixture_dir/plan-cli-binary.diff" <<'DIFF'
diff --git a/ll-open/cli/src/main.rs b/ll-open/cli/src/main.rs
--- a/ll-open/cli/src/main.rs
+++ b/ll-open/cli/src/main.rs
DIFF
plan_is "$fixture_dir/plan-cli-binary.diff" 'cli'

# ...and the `cli` leg that plan starts must finish. The binary's only mutant
# is excluded by policy, so the leg reports that and exits 0; MISCONFIGURED
# here failed every PR that touched only ll-open/cli (PR #464).
if ! out=$(cd "$repo_root/rs" && PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
    "$repo_root/tools/mutants_diff.sh" "$fixture_dir/plan-cli-binary.diff" cli 2>&1); then
    echo "a cli-binary-only diff failed its planned cli leg:" >&2
    echo "$out" >&2
    exit 1
fi
case "$out" in
    *"ll-open/cli carries only 'replace main'"*) ;;
    *) echo "the cli leg did not say why nothing ran: $out" >&2; exit 1 ;;
esac

# The planner emits a scope for ANY Rust change in its package, tests included.
# It runs no cargo, so it cannot know what will enumerate; the leg asks
# cargo-mutants and reports NO MUTABLE LINES when the answer is nothing. One
# authority on mutability, not a second rule here that could drift from it.
plan_is "$fixture_dir/runtime-tests-only.diff" 'runtime cli'

# THE case a hand-copied package list got wrong: a package the generic slice
# excludes and no scope owns. Serially this is caught by
# assert_excluded_packages_were_covered; split across matrix legs that check is
# vacuous in every leg, so planning must catch it or the gate reports green
# from a matrix of skipped legs.
cat > "$fixture_dir/plan-orphan.diff" <<'DIFF'
diff --git a/ll-open/orphan/src/lib.rs b/ll-open/orphan/src/lib.rs
--- a/ll-open/orphan/src/lib.rs
+++ b/ll-open/orphan/src/lib.rs
DIFF
if MUTANTS_GENERIC_EXCLUDES='ll-open/orphan' \
   PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
   "$repo_root/tools/mutants_diff.sh" "$fixture_dir/plan-orphan.diff" list-scopes; then
    echo 'planner emitted a matrix that would skip an unowned package' >&2
    exit 1
fi

# An unknown scope must be rejected, and the message must name the valid set —
# the set is derived from the table, so a new slice cannot forget to appear.
if PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
   "$repo_root/tools/mutants_diff.sh" "$fixture_dir/plan-mixed.diff" nonsense 2>/dev/null; then
    echo 'an unknown mutation scope was accepted' >&2
    exit 1
fi

# The generic scope runs the lib-only slice and no package slice.
: > "$log"
PATH="$fixture_dir/bin:$PATH" MUTANTS_FIXTURE_LOG="$log" \
  "$repo_root/tools/mutants_diff.sh" "$fixture_dir/pr.diff" generic
assert_call 'mutants .* -C --lib .*--exclude ll-open/runtime/\*\*' \
    'generic scope runs the lib-only slice'
if grep -E -- '--package leyline-runtime|--package leyline-cli-lib' "$log" >/dev/null; then
    echo 'generic scope invoked a package slice' >&2
    exit 1
fi

echo 'diff mutation fixture proved package-specific integration-test routing'
echo 'diff mutation fixture proved excluded packages must be claimed by a slice'
echo 'diff mutation fixture proved the fs and storefs slices enable what they mutate'
echo 'diff mutation fixture proved each generic package is built with its own features'
echo 'diff mutation fixture proved one recorded baseline serves every shard, and a failed or missing one fails them all'
