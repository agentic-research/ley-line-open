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

echo "no matching task ci receipt; running the full gate"
"${TASK_BIN:-task}" ci
tools/ci_attestation.sh check
tools/ci_attestation.sh verify-head
