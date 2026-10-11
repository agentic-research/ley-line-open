#!/bin/sh
# End-to-end smoke of one built `leyline` binary: the commands a release
# ships, run the way a user runs them, on a two-language fixture tree.
#
# Unit and integration tests drive library entry points; this drives the
# binary. Its first run (before v0.20.2) found two defects no test had: the
# daemon's `query` op answered every non-text value with "" (so
# `SELECT count(*)` returned ""), and SIGTERM killed the daemon outright,
# leaving its socket, arena lock and any mount behind.
#
# Usage: tools/cli_smoke.sh BIN [--mount]
#   BIN      the leyline binary to exercise
#   --mount  also mount the daemon's projection and write through it
#            (BIN must be built with the `mount` feature; needs FUSE/fuse-t)
# Env:
#   EXPECT_VERSION  if set, `BIN --version` must report exactly this
#   DEADLINE        seconds any one wait may take (default 30)
set -eu

BIN=${1:?usage: tools/cli_smoke.sh BIN [--mount]}
MODE=${2:-}
case $BIN in /*) ;; *) BIN=$(pwd)/$BIN ;; esac
test -x "$BIN" || { echo "cli smoke: $BIN is not an executable" >&2; exit 2; }
DEADLINE=${DEADLINE:-30}

# The daemon's socket sits beside its control file, and a unix socket path is
# capped near 104 bytes. $TMPDIR on macOS is ~50 bytes before our names, so
# the work dir is under /tmp to leave the socket room.
W=$(mktemp -d /tmp/llsmoke.XXXXXX)
DPID=

cleanup() {
    if [ -n "$DPID" ]; then
        kill -KILL "$DPID" 2>/dev/null || true
        wait "$DPID" 2>/dev/null || true
    fi
    if [ "$MODE" = --mount ] && mount | grep -qF "$W/mnt"; then
        umount -f "$W/mnt" 2>/dev/null || fusermount3 -uz "$W/mnt" 2>/dev/null ||
            fusermount -uz "$W/mnt" 2>/dev/null || true
    fi
    rm -rf "$W"
}
trap cleanup EXIT
trap 'exit 1' INT TERM

fail() {
    echo "cli smoke: FAIL: $*" >&2
    if [ -f "$W/d/log" ]; then sed 's/^/  daemon: /' "$W/d/log" >&2; fi
    exit 1
}
step() { echo "cli smoke: $*"; }

# sql DB QUERY — one value per line, rows' columns joined by |.
sql() {
    python3 -I -c '
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
for row in c.execute(sys.argv[2]):
    print("|".join("" if v is None else v.decode() if isinstance(v, bytes) else str(v) for v in row))
' "$1" "$2"
}

# op JSON — one request over the daemon socket; prints the response line.
op() {
    python3 -I -c '
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(float(sys.argv[3]))
s.connect(sys.argv[1])
s.sendall(sys.argv[2].encode() + b"\n")
buf = b""
while not buf.endswith(b"\n"):
    chunk = s.recv(65536)
    if not chunk:
        break
    buf += chunk
sys.stdout.write(buf.decode())
' "$W/d/a.sock" "$1" "$DEADLINE"
}

# field JSON PATH... — print one value out of a JSON document on stdin.
field() {
    python3 -I -c '
import json, sys
v = json.load(sys.stdin)
for k in sys.argv[1:]:
    v = v[int(k)] if isinstance(v, list) else v[k]
print("null" if v is None else v)
' "$@"
}

expect_eq() { [ "$1" = "$2" ] || fail "$3: got '$1', want '$2'"; }

# Run a command in the background and fail if it outlives DEADLINE. A FUSE
# call on a wedged mount blocks in the kernel; this keeps the smoke from
# hanging with it.
within_deadline() {
    what=$1
    shift
    "$@" &
    pid=$!
    ticks=0
    while kill -0 "$pid" 2>/dev/null; do
        ticks=$((ticks + 1))
        if [ "$ticks" -gt $((DEADLINE * 10)) ]; then
            kill -KILL "$pid" 2>/dev/null || true
            fail "$what did not finish within ${DEADLINE}s"
        fi
        sleep 0.1
    done
    wait "$pid" || fail "$what failed"
}

wait_for() {
    what=$1
    shift
    ticks=0
    until "$@"; do
        ticks=$((ticks + 1))
        [ "$ticks" -le $((DEADLINE * 10)) ] || fail "timed out waiting for $what"
        if [ -n "$DPID" ] && ! kill -0 "$DPID" 2>/dev/null; then
            fail "the daemon exited while waiting for $what"
        fi
        sleep 0.1
    done
}

# Stop the daemon with SIGTERM and require a graceful exit: status 0 and the
# socket removed.
stop_daemon() {
    kill -TERM "$DPID"
    ticks=0
    while kill -0 "$DPID" 2>/dev/null; do
        ticks=$((ticks + 1))
        [ "$ticks" -le $((DEADLINE * 10)) ] || fail "the daemon did not exit within ${DEADLINE}s of SIGTERM"
        sleep 0.1
    done
    status=0
    wait "$DPID" || status=$?
    DPID=
    expect_eq "$status" 0 "the daemon's exit status on SIGTERM"
    [ ! -e "$W/d/a.sock" ] || fail "SIGTERM left the daemon socket behind"
}

# --- version ---------------------------------------------------------------
version=$("$BIN" --version)
case $version in
leyline\ [0-9]*) ;;
*) fail "--version printed '$version'" ;;
esac
if [ -n "${EXPECT_VERSION:-}" ]; then
    expect_eq "$(echo "$version" | cut -d' ' -f2)" "$EXPECT_VERSION" "--version"
fi
step "$version"

# --- fixture ---------------------------------------------------------------
mkdir "$W/src" "$W/d"
printf 'package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n' >"$W/src/util.go"
printf 'pub fn twice(x: u32) -> u32 {\n    x * 2\n}\n' >"$W/src/lib.rs"
LEAF=util.go/package_clause/package_identifier
DB=$W/out.db

# --- parse -----------------------------------------------------------------
"$BIN" parse "$W/src" -o "$DB" >/dev/null 2>"$W/parse.err" || fail "parse: $(cat "$W/parse.err")"
expect_eq "$(sql "$DB" 'SELECT id FROM _source ORDER BY id' | tr '\n' ' ')" "lib.rs util.go " "parsed files"
expect_eq "$(sql "$DB" 'SELECT count(*) FROM _source s LEFT JOIN source_blobs b ON b.blob_hash = s.content_hash WHERE b.blob_hash IS NULL')" 0 \
    "files whose content_hash names no source blob"
expect_eq "$(sql "$DB" 'SELECT count(*) > 0, sum(node_hash IS NULL) FROM _ast')" "1|0" "_ast rows present, none without node_hash"
step "parse: 2 files, every source in source_blobs, every _ast row hashed"

# --- verify-head -----------------------------------------------------------
head1=$("$BIN" verify-head "$DB") || fail "verify-head after parse: $head1"
case $head1 in valid\ treeRoot=*) ;; *) fail "verify-head printed '$head1'" ;; esac
step "verify-head: $head1"

# --- splice: a good edit ---------------------------------------------------
"$BIN" splice --db "$DB" --node "$LEAF" --text util >/dev/null 2>"$W/splice.err" ||
    fail "splice: $(cat "$W/splice.err")"
expect_eq "$(sql "$DB" "SELECT b.blob_bytes FROM _source s JOIN source_blobs b ON b.blob_hash = s.content_hash WHERE s.id = 'util.go'" | head -1)" \
    "package util" "util.go's stored bytes after the splice"
expect_eq "$(sql "$DB" 'SELECT sum(node_hash IS NULL) FROM _ast')" 0 "_ast rows without node_hash after the splice"
head2=$("$BIN" verify-head "$DB") || fail "verify-head after splice: $head2"
[ "$head2" != "$head1" ] || fail "the splice did not move treeRoot"
step "splice: bytes stored, file re-hashed, head restamped"

# --- splice: a bad edit changes nothing -------------------------------------
before=$(cksum <"$DB")
if "$BIN" splice --db "$DB" --node "$LEAF" --text '((' >/dev/null 2>"$W/bad.err"; then
    fail "a splice that breaks the syntax succeeded"
fi
grep -q 'syntax errors' "$W/bad.err" || fail "the refused splice did not name the syntax error: $(cat "$W/bad.err")"
expect_eq "$(cksum <"$DB")" "$before" "the database after a refused splice"
step "splice: a syntax-breaking edit is refused and the database is unchanged"

# --- daemon: an over-long socket path is an error, not a panic --------------
long=$W/$(printf '%0120d' 0)
mkdir "$long"
if "$BIN" daemon --arena "$long/a.arena" --control "$long/a.ctrl" --timeout 5s >"$W/long.log" 2>&1; then
    fail "the daemon started with a socket path over the limit"
fi
grep -q 'unix socket path limit' "$W/long.log" || fail "over-long socket path: $(cat "$W/long.log")"
if grep -q 'panicked' "$W/long.log"; then fail "over-long socket path panicked: $(cat "$W/long.log")"; fi
step "daemon: an over-long socket path is refused with the limit named"

# --- daemon: headless --------------------------------------------------------
# PATH holds only the system dirs so the daemon does not find and spawn a
# developer's mache; sbin stays on it for mount_nfs (the macOS default
# backend) and fusermount.
start_daemon() {
    env PATH=/usr/bin:/bin:/usr/sbin:/sbin "$BIN" daemon --arena "$W/d/a.arena" --control "$W/d/a.ctrl" \
        --source "$W/src" --timeout 300s "$@" >"$W/d/log" 2>&1 &
    DPID=$!
    wait_for "the daemon socket" test -S "$W/d/a.sock"
    wait_for "the daemon to report ready" ready
}
ready() { [ "$(op '{"op":"status"}' 2>/dev/null | field phase 2>/dev/null)" = ready ]; }

start_daemon
expect_eq "$(op '{"op":"leyline_version"}' | field binary_version)" "$(echo "$version" | cut -d' ' -f2)" "leyline_version op"
expect_eq "$(op '{"op":"query","sql":"SELECT count(*) AS n FROM _source"}' | field rows 0 n)" 2 "query op count(*)"
expect_eq "$(op '{"op":"query","sql":"SELECT NULL AS z, x'"'"'00ff'"'"' AS b"}' | field rows 0 z)" null "query op NULL"
expect_eq "$(op '{"op":"query","sql":"SELECT x'"'"'00ff'"'"' AS b"}' | field rows 0 b)" 00ff "query op blob"
stop_daemon
step "daemon: status, leyline_version and typed query answers; SIGTERM shuts it down cleanly"

# --- daemon: mount -----------------------------------------------------------
if [ "$MODE" = --mount ]; then
    mkdir "$W/mnt"
    start_daemon --mount "$W/mnt"
    wait_for "the mount to serve util.go" test -d "$W/mnt/util.go"
    within_deadline "read through the mount" sh -c "test \"\$(cat '$W/mnt/$LEAF')\" = main"
    # truncate + write + fsync: the fsync is what makes the mount publish.
    within_deadline "write through the mount" python3 -I -c '
import os, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_TRUNC)
os.write(fd, b"util")
os.fsync(fd)
os.close(fd)
' "$W/mnt/$LEAF"
    expect_eq "$(op '{"op":"query","sql":"SELECT b.blob_bytes FROM _source s JOIN source_blobs b ON b.blob_hash = s.content_hash WHERE s.id = '"'"'util.go'"'"'"}' | field rows 0 blob_bytes)" \
        "$(printf 'package util\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n' | od -An -tx1 | tr -d ' \n')" \
        "util.go's stored bytes after the mount write"
    within_deadline "read back through the mount" sh -c "test \"\$(cat '$W/mnt/$LEAF')\" = util"
    stop_daemon
    wait_for "the mount to detach" sh -c "! mount | grep -qF '$W/mnt'"
    step "mount: a write through the mount is stored, served back, and the mount detaches on SIGTERM"
fi

step "PASS"
