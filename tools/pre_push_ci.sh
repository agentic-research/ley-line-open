#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$repo_root"

# pre-commit's pre-push hook exports the push target as
# PRE_COMMIT_REMOTE_BRANCH (refs/heads/<name>). Only a push that lands on
# main needs the full local gate: every PR now gets `task ci` on a runner
# regardless of base branch (ci.yml's pull_request trigger), so a push to
# any other branch can defer to that and skip the multi-minute wait here.
# A raw `git push` outside the pre-commit framework leaves the variable
# unset — fail CLOSED to the full gate in that case, because a missing
# signal must never be read as "skip".
if [ -n "${PRE_COMMIT_REMOTE_BRANCH:-}" ] && [ "$PRE_COMMIT_REMOTE_BRANCH" != "refs/heads/main" ]; then
    echo "pre-push: target is not main; full gate deferred to PR CI on the runner"
    exit 0
fi

# A push validates committed bytes, not an uncommitted overlay. Refuse the
# ambiguous case before paying for the full gate. Skipped above for
# non-main pushes: a dirty tree there is the normal integration flow (e.g.
# the beads export churning alongside a branch push), not an error. Main
# pushes keep the strict check.
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
