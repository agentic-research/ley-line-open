#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$repo_root"

# A push validates committed bytes, not an uncommitted overlay. Refuse the
# ambiguous case before paying for the full gate.
tools/ci_attestation.sh verify-head

if tools/ci_attestation.sh check; then
    echo "task ci already passed for this exact tree; skipping duplicate pre-push run"
    exit 0
fi

# Straight to the terminal, not stdout: pre-commit captures stdout and holds
# it until the hook exits, so a message there arrives after the wait it was
# meant to explain. /dev/tty bypasses that. Guarded because a hook can run
# without a controlling terminal (CI, a GUI client).
{ echo "pre-push: no task ci receipt for this tree — running the full gate (several minutes)" \
    > /dev/tty; } 2>/dev/null || true
echo "no matching task ci receipt; running the full gate"
"${TASK_BIN:-task}" ci
tools/ci_attestation.sh check
tools/ci_attestation.sh verify-head
