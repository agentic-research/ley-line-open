#!/bin/sh
# End-to-end smoke for the LLO OCI image (`ley-line-open-49da9a`).
#
# Extracted from Taskfile.yml rather than left inline: it was three near-
# identical deadline loops embedded in YAML, where shellcheck cannot see them,
# nothing can test them, and each block re-implements the same wait. Same
# reasoning as tools/stage_release_artifacts.sh and friends.
#
# What this replaced could not fail, only hang:
#
#   until curl -sf ... -d '{}' || ! docker ps ...; do sleep 0.3; done
#
# The probe sent no token. ADR-0022's gate answers 401, `curl -sf` treats 4xx
# as failure, and the container stays up — so NEITHER loop condition could ever
# become true. No deadline, so it waited forever; observed at 10 minutes with a
# perfectly healthy daemon. In CI that burns the job budget and surfaces as a
# timeout, which points at the wrong thing.
#
# The step after it was independently vacuous: plain `curl -s` (no `-f`), piped
# to `head`, asserting nothing, then an unconditional `echo "smoke passed"`. A
# body of {"error":"unauthorized"} printed and passed.
#
# Env:
#   IMAGE_REF   image to run          (required)
#   PLATFORM    docker platform       (required)
#   HOST_PORT   host port to publish  (default 18384)
#   DEADLINE    seconds per wait      (default 60)
set -eu

: "${IMAGE_REF:?IMAGE_REF is required}"
: "${PLATFORM:?PLATFORM is required}"
HOST_PORT="${HOST_PORT:-18384}"
DEADLINE="${DEADLINE:-60}"
NAME=llo-smoke
TOKEN_PATH=/tmp/.local/share/leyline/daemon.token

cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

# Every failure prints the container's own logs. A smoke failure without them
# tells you something broke but not what, which is most of the cost of a
# failing gate.
die() {
    echo "FAIL: $*" >&2
    docker logs "$NAME" 2>&1 | tail -30 >&2 || true
    exit 1
}

# One wait implementation, used by both phases. Takes the NAME of a predicate
# function rather than a string to eval — shellcheck flagged the string form
# (SC2016) and it was right to: passing code as data here bought nothing and
# cost the ability to lint it.
wait_for() {
    label=$1
    predicate=$2
    deadline=$(( $(date +%s) + DEADLINE ))
    while ! "$predicate" >/dev/null 2>&1; do
        docker ps -q --filter "name=$NAME" | grep -q . ||
            die "container exited while waiting for $label"
        [ "$(date +%s)" -le "$deadline" ] ||
            die "timed out after ${DEADLINE}s waiting for $label"
        sleep 0.3
    done
}

cleanup

# Container-side 0.0.0.0 is the image's own netns; the host publish is pinned to
# loopback so the token-gated listener never reaches the LAN. See the security
# block in image.Dockerfile.
docker run -d --rm --name "$NAME" \
    --platform "$PLATFORM" \
    -p "127.0.0.1:${HOST_PORT}:8384" \
    "$IMAGE_REF" >/dev/null

# `docker cp`, not `docker exec cat`: the image is distroless and has no shell
# or coreutils, which is the point of distroless.
read_token() {
    docker cp "$NAME:$TOKEN_PATH" - 2>/dev/null | tar -xO 2>/dev/null
}

have_token() { [ -n "$(read_token)" ]; }

wait_for "the daemon token" have_token
TOKEN=$(read_token)
[ -n "$TOKEN" ] || die "token vanished between reads"

probe() {
    curl -sf -m 2 -X POST "http://localhost:${HOST_PORT}/mcp" \
        -H 'Content-Type: application/json' \
        -H "x-leyline-token: $TOKEN" \
        -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
}

wait_for "MCP to answer tools/list" probe

BODY=$(probe) || die "tools/list failed after answering once"

# Assert the RESULT, not that something answered. "The endpoint responded" is
# satisfied by an error body; an empty tool array would mean the registry never
# populated, which is the failure worth catching and precisely what the old
# version could not see.
COUNT=$(printf '%s' "$BODY" | jq '(.result.tools // []) | length')
if [ "${COUNT:-0}" -lt 1 ]; then
    printf '%s\n' "$BODY" | head -c 600 >&2
    echo >&2
    die "tools/list returned no tools"
fi

echo "smoke passed — tools/list returned $COUNT tools"
