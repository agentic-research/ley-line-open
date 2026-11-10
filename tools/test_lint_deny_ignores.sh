#!/bin/sh
# Fixture test for tools/lint_deny_ignores.sh (bead ley-line-open-3adcf7).
#
# Each case is a throwaway deny.toml against a throwaway advisory database and
# a fixed shipped-crate list (LLO_SHIPPED_CRATES), with today pinned
# (LLO_LINT_TODAY), so no case depends on the network, the live dependency
# graph or the calendar. Every red case must fail AND say why; the well-formed
# case must pass. A lint that fails everything would fail the green case, and
# one that passes everything would pass the red ones.
set -eu

repo=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
lint="$repo/tools/lint_deny_ignores.sh"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/llo-deny-ignores-test.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

db="$scratch/db/advisory-db-fixture"
mkdir -p "$db/crates/unshipped-crate" "$db/crates/shipped-crate"
: > "$db/crates/unshipped-crate/RUSTSEC-2099-0001.md"
: > "$db/crates/shipped-crate/RUSTSEC-2099-0002.md"
printf '%s\n' leyline-cli leyline-krun-worker tokio shipped-crate > "$scratch/shipped-crates.txt"

export LLO_ADVISORY_DB="$scratch/db"
export LLO_SHIPPED_CRATES="$scratch/shipped-crates.txt"
export LLO_LINT_TODAY=2030-06-15

good='{ id = "RUSTSEC-2099-0001", reason = "why; bead ley-line-open-abc123; expires 2030-06-15" },'

# write_config <case> <entry lines...>
write_config() {
    case_dir="$scratch/$1"
    shift
    mkdir -p "$case_dir"
    {
        printf '%s\n' '[graph]' 'all-features = true' '' '[advisories]' 'ignore = ['
        for line in "$@"; do printf '    %s\n' "$line"; done
        printf '%s\n' ']'
    } > "$case_dir/deny.toml"
}

pass() {
    if ! out=$(sh "$lint" "$scratch/$1/deny.toml" "$scratch/no-rs" 2>&1); then
        printf 'FAIL %s: expected pass, got:\n%s\n' "$1" "$out" >&2
        exit 1
    fi
    printf 'ok   %s\n' "$1"
}

# fail_with <case> <expected message fragment>
fail_with() {
    if out=$(sh "$lint" "$scratch/$1/deny.toml" "$scratch/no-rs" 2>&1); then
        printf 'FAIL %s: expected failure, lint passed:\n%s\n' "$1" "$out" >&2
        exit 1
    fi
    case $out in
        *"$2"*) printf 'ok   %s (red: %s)\n' "$1" "$2" ;;
        *) printf 'FAIL %s: failed without naming "%s":\n%s\n' "$1" "$2" "$out" >&2; exit 1 ;;
    esac
}

write_config green "# a comment line is skipped" "$good"
pass green

write_config no-ignores
pass no-ignores

write_config no-bead '{ id = "RUSTSEC-2099-0001", reason = "why; expires 2030-06-15" },'
fail_with no-bead 'names no bead'

write_config no-expiry '{ id = "RUSTSEC-2099-0001", reason = "why; bead ley-line-open-abc123" },'
fail_with no-expiry 'has no expiry'

write_config expired '{ id = "RUSTSEC-2099-0001", reason = "why; bead ley-line-open-abc123; expires 2030-06-14" },'
fail_with expired 'expired on 2030-06-14'

write_config bare-string '"RUSTSEC-2099-0001",'
fail_with bare-string 'not a one-line'

write_config unknown-id '{ id = "RUSTSEC-2099-0009", reason = "why; bead ley-line-open-abc123; expires 2030-06-15" },'
fail_with unknown-id 'not in the advisory database'

write_config shipped '{ id = "RUSTSEC-2099-0002", reason = "why; bead ley-line-open-abc123; expires 2030-06-15" },'
fail_with shipped 'shipped-crate is in the shipped closure'

mkdir -p "$scratch/inline"
printf '%s\n' '[advisories]' 'ignore = ["RUSTSEC-2099-0001"]' > "$scratch/inline/deny.toml"
fail_with inline 'one entry per line'

printf 'deny ignore lint: every red case failed for its reason; the green cases passed\n'
