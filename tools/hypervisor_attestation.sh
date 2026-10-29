#!/bin/sh
# The hypervisor-suite release gate (bead ley-line-open-16a994).
#
#   hypervisor_attestation.sh record        run the suite on this tree; on a
#                                           pass, record a receipt for it
#   hypervisor_attestation.sh check [TREE]  succeed only if a passing receipt
#                                           exists for TREE (default: this
#                                           worktree's tree)
#
# The suite boots real libkrun guests, agent_in_guest included, and no hosted
# CI runner can: GitHub's macOS runners lack nested Hypervisor.framework. So
# the proof is a manual run on an Apple-silicon (or KVM) host, and this
# receipt is how the release path knows that run happened on the exact tree
# it is about to tag. tools/tag_release.sh refuses to tag without one.
#
# `record` runs `task runtime:test` with LEYLINE_REQUIRE_HYPERVISOR=1, which
# turns every "no hypervisor here" skip into a failure, so a receipt cannot
# come from a run that booted nothing. It needs LEYLINE_LIBKRUNFW_PATH (the
# guest kernel) and LEYLINE_GUEST_TOOLS (the rootfs tools/guest_tools.sh
# writes), as rs/ll-open/runtime/Taskfile.yml documents.
#
# Trees are named exactly as tools/ci_attestation.sh names them (`snapshot`):
# the working tree including untracked files, so a clean checkout's tree is
# HEAD^{tree}. Receipts are keyed by tree, not commit, and live in the shared
# git directory, so a run in any worktree of the repository covers a
# promotion merge whose tree is the same bytes.
#
# Like the task ci receipt, this is a local record that the run happened, not
# an unforgeable attestation: anyone with write access to .git can write one.
# `task release:hypervisor:record` first runs `task krun-worker:install` and
# sets LEYLINE_KRUN_WORKER to the installed worker, so the guest tests run that
# artifact as installed and the receipt records its sha256.
#
# HYPERVISOR_SUITE overrides the suite command for tools/test_*.sh fixtures,
# and the receipt names the command that actually ran.
set -eu

script_root=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
receipt_dir="$(git rev-parse --path-format=absolute --git-common-dir)/leyline-hypervisor-receipts"
canonical_suite='task runtime:test'
suite=${HYPERVISOR_SUITE:-$canonical_suite}

record_command='LEYLINE_LIBKRUNFW_PATH=<libkrunfw> LEYLINE_GUEST_TOOLS=<guest rootfs> task release:hypervisor:record'

usage() {
    echo "usage: $0 record | check [TREE]" >&2
    exit 2
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

host_description() {
    if command -v sw_vers >/dev/null 2>&1; then
        printf '%s macOS %s (%s)\n' "$(uname -sm)" \
            "$(sw_vers -productVersion)" "$(sw_vers -buildVersion)"
    else
        uname -srm
    fi
}

snapshot() {
    "$script_root/ci_attestation.sh" snapshot
}

command=${1:-}
case "$command" in
    record)
        [ "$#" -eq 1 ] || usage
        libkrunfw=${LEYLINE_LIBKRUNFW_PATH:-}
        guest_tools=${LEYLINE_GUEST_TOOLS:-}
        if [ -z "$libkrunfw" ] || [ ! -f "$libkrunfw" ]; then
            echo "LEYLINE_LIBKRUNFW_PATH must name libkrunfw (the guest kernel)" >&2
            echo "usage: $record_command" >&2
            exit 1
        fi
        if [ -z "$guest_tools" ] || [ ! -d "$guest_tools" ]; then
            echo "LEYLINE_GUEST_TOOLS must name the guest rootfs tools/guest_tools.sh writes" >&2
            echo "usage: $record_command" >&2
            exit 1
        fi
        # LEYLINE_KRUN_WORKER: the INSTALLED worker the guest tests run
        # (`task release:hypervisor:record` sets it to what
        # `task krun-worker:install` produced, bead ley-line-open-984acf).
        # Its entitlement is checked here, before the run, and its digest
        # goes in the receipt.
        worker=${LEYLINE_KRUN_WORKER:-}
        if [ -n "$worker" ]; then
            if [ ! -f "$worker" ]; then
                echo "LEYLINE_KRUN_WORKER names no file: $worker" >&2
                exit 1
            fi
            if [ "$(uname -s)" = Darwin ]; then
                "$script_root/sign_krun_worker.sh" --check "$worker"
            fi
            worker_line="worker $worker sha256 $(sha256_of "$worker")"
        else
            worker_line="worker cargo-built, signed per test by tools/sign_krun_worker.sh"
        fi
        before=$(snapshot)
        echo "running the hypervisor suite on tree $before: LEYLINE_REQUIRE_HYPERVISOR=1 $suite" >&2
        # Unquoted on purpose: the suite is a command line ("task runtime:test").
        # shellcheck disable=SC2086
        if ! env LEYLINE_REQUIRE_HYPERVISOR=1 \
            LEYLINE_LIBKRUNFW_PATH="$libkrunfw" \
            LEYLINE_GUEST_TOOLS="$guest_tools" \
            ${worker:+LEYLINE_KRUN_WORKER="$worker"} \
            $suite
        then
            echo "hypervisor suite failed on tree $before; no receipt recorded" >&2
            exit 1
        fi
        after=$(snapshot)
        if [ "$before" != "$after" ]; then
            echo "the tree changed while the hypervisor suite ran ($before -> $after);" >&2
            echo "refusing to record a receipt for a tree the run did not test" >&2
            exit 1
        fi
        mkdir -p "$receipt_dir"
        receipt="$receipt_dir/$after"
        temporary=$(mktemp "$receipt.tmp.XXXXXX")
        {
            printf '%s\n' leyline-hypervisor-receipt-v1
            printf 'tree %s\n' "$after"
            printf 'suite LEYLINE_REQUIRE_HYPERVISOR=1 %s\n' "$suite"
            printf 'libkrunfw %s sha256 %s\n' "$libkrunfw" "$(sha256_of "$libkrunfw")"
            printf 'guest-tools %s\n' "$guest_tools"
            printf '%s\n' "$worker_line"
            printf 'host %s\n' "$(host_description)"
            printf 'rustc %s\n' "$(rustc --version 2>/dev/null || echo unavailable)"
            printf 'recorded %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        } > "$temporary"
        mv -f "$temporary" "$receipt"
        echo "recorded hypervisor receipt for tree $after: $receipt"
        ;;
    check)
        [ "$#" -le 2 ] || usage
        tree=${2:-$(snapshot)}
        receipt="$receipt_dir/$tree"
        if [ -r "$receipt" ] &&
            [ "$(sed -n '1p' "$receipt")" = leyline-hypervisor-receipt-v1 ] &&
            [ "$(sed -n '2p' "$receipt")" = "tree $tree" ]
        then
            echo "hypervisor receipt matches tree $tree:" >&2
            sed -n '3,$p' "$receipt" | sed 's/^/  /' >&2
            exit 0
        fi
        echo "no passing hypervisor-suite receipt for tree $tree." >&2
        echo "The release gate needs the real-guest suite (agent_in_guest included)" >&2
        echo "to have passed on exactly this tree. On an Apple-silicon host, from a" >&2
        echo "checkout of this tree, run:" >&2
        echo "  $record_command" >&2
        exit 1
        ;;
    *)
        usage
        ;;
esac
