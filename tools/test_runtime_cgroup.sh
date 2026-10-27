#!/usr/bin/env bash
# Run the native tier's cgroup v2 tests against a real, delegated cgroup
# (bead ley-line-open-da36d2, ADR-0035 "OCI resources on execution/v1").
#
# The tests in rs/ll-open/runtime/tests/native_cgroup.rs need a cgroup the
# test process may create children in and move processes between. The kernel
# allows that migration only to a writer of cgroup.procs in the COMMON
# ANCESTOR of the source and destination cgroups, so this script builds:
#
#   /sys/fs/cgroup/llo-test-<pid>/        owned by the invoking user, delegates
#     leaf/                               memory cpu pids; this shell lives here
#     runs/                               LLO_TEST_CGROUP_ROOT, delegates them too
#
# and runs the tests from inside leaf/, so every migration's common ancestor
# is the user-owned llo-test-<pid>/. That takes root once, for setup.
#
# Off Linux there is no cgroup v2, and the native tier refuses every
# memory/CPU/pids ceiling instead (tests/resource_limits.rs); this script says
# so and exits 0. On Linux without passwordless sudo it skips too, unless CI
# is set: CI has sudo, and a skipped kernel test there would read as passing.
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
    echo "runtime:test:cgroup: skipped — cgroup v2 is Linux-only; on $(uname -s) the native tier refuses memory/cpu/pids ceilings (resource_limits.rs)"
    exit 0
fi
if ! sudo -n true 2>/dev/null; then
    if [ -n "${CI:-}" ]; then
        echo "runtime:test:cgroup: CI is set but sudo is unavailable; cannot delegate a cgroup" >&2
        exit 1
    fi
    echo "runtime:test:cgroup: skipped — needs passwordless sudo to delegate a cgroup"
    exit 0
fi
host=/sys/fs/cgroup
# The filesystem magic, not its name: older coreutils print the name of
# cgroup2 as "UNKNOWN (0x63677270)".
if [ "$(stat -fc %t "$host")" != "63677270" ]; then
    echo "runtime:test:cgroup: $host is not cgroup v2" >&2
    exit 1
fi
for controller in memory cpu pids; do
    if ! tr ' ' '\n' < "$host/cgroup.subtree_control" | grep -qx "$controller"; then
        echo "runtime:test:cgroup: the host hierarchy does not delegate $controller" >&2
        exit 1
    fi
done

base="$host/llo-test-$$"
origin="$host$(sed -n 's/^0:://p' /proc/self/cgroup)"
cleanup() {
    # Return to the cgroup this shell started in (the hierarchy's root may
    # refuse processes), then remove deepest first.
    echo $$ | sudo tee "$origin/cgroup.procs" >/dev/null || true
    for dir in "$base"/runs/* "$base/runs" "$base/leaf" "$base"; do
        [ -d "$dir" ] && sudo rmdir "$dir" 2>/dev/null || true
    done
}
trap cleanup EXIT

sudo mkdir "$base" "$base/leaf" "$base/runs"
echo "+memory +cpu +pids" | sudo tee "$base/cgroup.subtree_control" >/dev/null
echo "+memory +cpu +pids" | sudo tee "$base/runs/cgroup.subtree_control" >/dev/null
sudo chown -R "$(id -u):$(id -g)" "$base"
echo $$ | sudo tee "$base/leaf/cgroup.procs" >/dev/null

cd "$(dirname "$0")/../rs"
LLO_TEST_CGROUP_ROOT="$base/runs" RUSTC_WRAPPER='' \
    cargo test -p leyline-runtime --test native_cgroup -- --nocapture
