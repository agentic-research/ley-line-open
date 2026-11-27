#!/bin/sh
# Smoke of one built or staged `leyline-conn-broker` (bead
# ley-line-open-caa20b): the binary runs, and it refuses a bad argument by
# name before it reads a connections document or binds anything.
#
# The broker's refusal is one `error` event on stdout and exit status 2
# (leyline_runtime::broker::control::run_broker). Each case below names the
# argument the refusal must name. The last case gets past argument parsing:
# descriptor 9 is closed for the child, so the lifeline check itself refuses.
#
# Usage: tools/conn_broker_smoke.sh BIN
set -eu

BIN=${1:?usage: tools/conn_broker_smoke.sh BIN}
case $BIN in /*) ;; *) BIN=$(pwd)/$BIN ;; esac
test -x "$BIN" || { echo "conn broker smoke: $BIN is not an executable" >&2; exit 2; }

fail() {
    echo "conn broker smoke: $*" >&2
    exit 1
}

# expect_refusal DETAIL ARG... : BIN ARG... exits 2 with one error event
# whose text contains DETAIL (as it appears JSON-escaped on the wire).
expect_refusal() {
    detail=$1
    shift
    set +e
    out=$("$BIN" "$@" </dev/null 9<&-)
    status=$?
    set -e
    [ "$status" -eq 2 ] ||
        fail "'$*' exited $status, not 2 (output: $out)"
    printf '%s\n' "$out" | grep -qF -e '"event":"error"' ||
        fail "'$*' printed no error event (output: $out)"
    printf '%s\n' "$out" | grep -qF -e "$detail" ||
        fail "'$*' refusal does not name '$detail' (output: $out)"
    echo "refused by name: ${*:-(no arguments)}"
}

expect_refusal '--lifeline-fd \"not-a-number\" is not a number' --lifeline-fd not-a-number
expect_refusal '--lifeline-fd needs a value' --lifeline-fd
expect_refusal '--lifeline-fd given twice' --lifeline-fd 9 --lifeline-fd 9
expect_refusal '--lifeline-fd is required'
expect_refusal 'unknown argument --not-a-flag' --not-a-flag
expect_refusal '--lifeline-fd 9 is not an open descriptor' --lifeline-fd 9

echo "conn broker smoke passed: $BIN"
