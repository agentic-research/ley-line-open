#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$repo_root"

# pre-commit's pre-push hook exports the push target as
# PRE_COMMIT_REMOTE_BRANCH (refs/heads/<name>). Two branches require the
# full local gate:
#
#   main         — the release line; protected, and the runner re-verifies.
#   integration  — the working line. Pushes here trigger NO runner CI at
#                  all (ci.yml fires on PRs and main pushes only), so this
#                  hook IS the gate: the attested local `task ci` receipt
#                  is the trust anchor, and the runner is paid once at the
#                  integration→main merge instead of per push. Requiring
#                  it here is what turns "we trust our local runs" from a
#                  discipline into an invariant.
#
# Every other branch defers to PR CI on the runner. A raw `git push`
# outside the pre-commit framework leaves the variable unset — fail CLOSED
# to the full gate in that case, because a missing signal must never be
# read as "skip".
case "${PRE_COMMIT_REMOTE_BRANCH:-unset}" in
refs/heads/main | refs/heads/integration | unset) ;;
*)
    echo "pre-push: target is not main or integration; full gate deferred to PR CI on the runner"
    exit 0
    ;;
esac

# A push validates committed bytes, not an uncommitted overlay. Refuse the
# ambiguous case before paying for the full gate. The snapshot excludes
# the bead-export file (other repos churn it asynchronously; see the
# exclusion in ci_attestation.sh), so that file being dirty alone does not
# block an integration push — any other uncommitted file does, exactly
# because this hook is the only gate that branch has.
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
