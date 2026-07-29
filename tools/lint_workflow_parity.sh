#!/bin/sh
# Workflow parity gate (bead ley-line-open-2bea72).
#
# release-dryrun.yml exists so the release path gets rehearsed on a PR — which
# only means something if the two workflows run the SAME setup. v0.12.1 proved
# what happens when they drift: the dry-run caught the missing capnp install on
# a branch, the fix went into the dry-run file only, and the tag failed on the
# bug the rehearsal had already found. The signal was right; nothing enforced
# that both copies heard it.
#
# The durable half of the fix is structural: install commands live in Taskfile
# `deps:*` targets, so there is one copy to edit (same rule as everything else
# in this repo — workflows invoke task, they never re-implement commands).
# This gate asserts the facts that remain assertable after that move:
#
#   1. No workflow inlines a package install (apt-get/brew/cargo/pip). One
#      copy in the Taskfile, or it will drift.
#   2. In the release pair, every job that installs Rust also runs
#      `task deps:capnp` — installing a compiler means compiling the
#      workspace, and the workspace's capnp build script needs system deps.
#   3. In the release pair, every job that runs a `task image*` build target
#      also runs `task deps:zig` — the image build cross-compiles with
#      zigbuild.
#   4. Every arduino/setup-task pin is exact and identical across ALL
#      workflows — a floating or divergent go-task version is the same class
#      of drift rust-toolchain.toml exists to close.
#
# WORKFLOWS_DIR is overridable so tools/test_workflow_parity.sh can prove each
# rule fails on the mutation it guards against, not just that this file exists.
set -eu

dir=${WORKFLOWS_DIR:-.github/workflows}
fail=0

# --- Rule 1: no inline installs anywhere -----------------------------------
# Comment lines are stripped first: the history of the krust failure is TOLD
# in comments that legitimately contain the words `cargo install`.
inline=$(
    for wf in "$dir"/*.yml; do
        sed 's/#.*$//' "$wf" |
            grep -nE '(apt-get|brew|pip[0-9.]*|pip3) install|cargo install' |
            sed "s|^|$wf:|" || true
    done
)
if [ -n "$inline" ]; then
    echo "workflow-parity: inline package installs found — move the command into a" >&2
    echo "Taskfile deps:* target and invoke it, so there is one copy to keep true:" >&2
    printf '%s\n' "$inline" >&2
    fail=1
fi

# --- Rules 2+3: per-job structure of the release pair ----------------------
# Job boundaries in this repo's workflows are 2-space-indented keys under
# `jobs:`. The awk tracks the current job and what it contains, then emits
# one line per violation.
for wf in "$dir"/release.yml "$dir"/release-dryrun.yml; do
    test -f "$wf" || { echo "workflow-parity: $wf is missing" >&2; fail=1; continue; }
    violations=$(awk '
        function flush() {
            if (job == "") return
            if (has_rust && !has_capnp)
                print job ": installs a Rust toolchain but never runs task deps:capnp"
            if (has_image && !has_zig)
                print job ": runs a task image build target but never runs task deps:zig"
        }
        /^jobs:/       { injobs = 1; next }
        injobs && /^[a-z]/ { injobs = 0 }   # left the jobs: block entirely
        injobs && /^  [A-Za-z0-9_-]+:[ ]*$/ {
            flush()
            job = $1; sub(":", "", job)
            has_rust = has_capnp = has_image = has_zig = 0
            next
        }
        injobs && /rust-toolchain@/   { has_rust = 1 }
        injobs && /task deps:capnp/   { has_capnp = 1 }
        injobs && /run:.*task image(:publish)? / { has_image = 1 }
        injobs && /task deps:zig/     { has_zig = 1 }
        END { flush() }
    ' "$wf")
    if [ -n "$violations" ]; then
        printf '%s\n' "$violations" | sed "s|^|workflow-parity: $wf: job |" >&2
        fail=1
    fi
done

# --- Rule 4: setup-task pins exact and identical ----------------------------
pins=$(
    for wf in "$dir"/*.yml; do
        awk '
            /arduino\/setup-task@/ { want = 1 }
            want && /version:/ {
                v = $0; sub(/.*version:[ ]*/, "", v); sub(/[ ]*#.*$/, "", v)
                print FILENAME ":" v; want = 0
            }
        ' "$wf"
    done
)
if [ -n "$pins" ]; then
    versions=$(printf '%s\n' "$pins" | cut -d: -f2- | sort -u)
    count=$(printf '%s\n' "$versions" | wc -l | tr -d ' ')
    if [ "$count" -ne 1 ]; then
        echo "workflow-parity: setup-task versions diverge across workflows:" >&2
        printf '%s\n' "$pins" >&2
        fail=1
    fi
    case "$versions" in
        *x*|latest|"")
            echo "workflow-parity: setup-task pin '$versions' is floating — pin an exact version" >&2
            fail=1
            ;;
    esac
fi

if [ "$fail" != 0 ]; then
    echo "--- fix: install commands belong in Taskfile deps:* targets; the release" >&2
    echo "pair must call them from every job that compiles; setup-task pins must" >&2
    echo "be exact and shared. See bead ley-line-open-2bea72." >&2
    exit 1
fi
echo "workflow parity OK — no inline installs, release pair calls deps targets, setup-task pins agree"
