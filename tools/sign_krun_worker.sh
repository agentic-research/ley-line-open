#!/bin/sh
# Ad-hoc sign leyline-krun-worker with the repository's entitlements.plist,
# then prove the signature carries com.apple.security.hypervisor.
#
#   sign_krun_worker.sh <binary>          sign, then check
#   sign_krun_worker.sh --check <binary>  check only
#
# The one place the worker is signed (bead ley-line-open-984acf): the
# krun-worker Taskfile build and install tasks, release staging, and every
# test that boots a real guest call this script, so the entitlement and the
# codesign flags have a single source. Without the entitlement,
# Hypervisor.framework refuses the worker and libkrun fails VM creation with
# EINVAL inside krun_start_enter.
#
# The signature is ad hoc (`--sign -`). Developer ID signing and notarization
# are separate work (bead ley-line-open-984acf's follow-up).
#
# Off macOS the hypervisor needs no entitlement (Linux uses /dev/kvm), so
# signing is a no-op there. A check cannot run there, and says so: an
# entitlement nobody could inspect is not a passed check.
set -eu

usage() {
    echo "usage: $0 [--check] <path-to-leyline-krun-worker>" >&2
    exit 2
}

mode=sign
if [ "${1:-}" = "--check" ]; then
    mode=check
    shift
fi
[ "$#" -eq 1 ] || usage
binary=$1
test -f "$binary" || {
    echo "sign_krun_worker: no such file: $binary" >&2
    exit 1
}

if [ "$(uname -s)" != Darwin ]; then
    if [ "$mode" = check ]; then
        echo "sign_krun_worker: cannot inspect a macOS entitlement on $(uname -s)" >&2
        exit 1
    fi
    exit 0
fi

if [ "$mode" = sign ]; then
    repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
    /usr/bin/codesign --force --sign - \
        --entitlements "$repo_root/entitlements.plist" "$binary"
fi

# Assert, do not assume: a signing step that silently stops granting the
# entitlement must fail here, not inside the VMM at the first run.
if ! /usr/bin/codesign -d --entitlements - --xml "$binary" 2>/dev/null |
    tr -d ' \t\r\n' |
    grep -q '<key>com.apple.security.hypervisor</key><true/>'
then
    echo "sign_krun_worker: $binary lacks com.apple.security.hypervisor" >&2
    exit 1
fi
