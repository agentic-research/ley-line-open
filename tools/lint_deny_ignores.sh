#!/bin/sh
# deny.toml ignore lint — bead ley-line-open-3adcf7.
#
# usage: tools/lint_deny_ignores.sh <deny.toml> <rs-dir>
#
# An advisory ignore is a promise that someone will come back. This lint
# makes the promise checkable. Every entry of `[advisories] ignore` must:
#
#   1. be one line of the form
#        { id = "RUSTSEC-YYYY-NNNN", reason = "..." }
#      (bare strings and crate-keyed yanked ignores are rejected);
#   2. name a bead in its reason: `bead ley-line-open-xxxxxx`;
#   3. carry an expiry in its reason: `expires YYYY-MM-DD`, not yet past
#      (today, UTC, may be overridden with LLO_LINT_TODAY for the fixture);
#   4. name an advisory that exists in the local advisory database, and
#   5. name an advisory whose crate is NOT in the shipped closure:
#      leyline-cli's normal-edge tree on every release target, plus
#      leyline-krun-worker's on aarch64-apple-darwin, release (default)
#      features. A shipped advisory is fixed, never ignored.
#
# The advisory database is the one cargo-deny fetches, under
# $CARGO_HOME/advisory-dbs (LLO_ADVISORY_DB overrides the search root).
# LLO_SHIPPED_CRATES, when set, names a file listing the shipped crate names
# one per line instead of asking cargo; the fixture uses it so its cases do
# not depend on the live dependency graph.
set -eu

deny=${1:?usage: lint_deny_ignores.sh <deny.toml> <rs-dir>}
rs=${2:?usage: lint_deny_ignores.sh <deny.toml> <rs-dir>}
test -f "$deny" || { printf 'deny ignores: no config at %s\n' "$deny" >&2; exit 2; }

today=${LLO_LINT_TODAY:-$(date -u +%Y-%m-%d)}
db_root=${LLO_ADVISORY_DB:-${CARGO_HOME:-$HOME/.cargo}/advisory-dbs}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/llo-deny-ignores.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM

# The ignore array's lines, comments and blank lines removed.
awk '
  /^\[/ { in_adv = ($0 == "[advisories]") ; in_ign = 0; next }
  in_adv && /^[[:space:]]*ignore[[:space:]]*=[[:space:]]*\[/ { in_ign = 1; next }
  in_ign && /^[[:space:]]*\][[:space:]]*$/ { in_ign = 0; next }
  in_ign {
    line = $0
    sub(/^[[:space:]]+/, "", line)
    if (line == "" || substr(line, 1, 1) == "#") next
    print line
  }
' "$deny" > "$scratch/entries"

# An inline `ignore = [ ... ]` on one line would bypass the parse above.
if grep -Eq '^[[:space:]]*ignore[[:space:]]*=[[:space:]]*\[.*[^[:space:]]' "$deny"; then
    printf 'deny ignores: write `ignore = [` with one entry per line, not inline\n' >&2
    exit 1
fi

if [ ! -s "$scratch/entries" ]; then
    printf 'deny ignores: none\n'
    exit 0
fi

shipped="$scratch/shipped"
if [ -n "${LLO_SHIPPED_CRATES:-}" ]; then
    cp "$LLO_SHIPPED_CRATES" "$shipped"
else
    : > "$scratch/tree"
    for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
        x86_64-apple-darwin aarch64-apple-darwin \
        x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
        cargo tree --quiet --locked --manifest-path "$rs/Cargo.toml" -e normal \
            -p leyline-cli --target "$target" --prefix none --format '{p}' \
            >> "$scratch/tree"
    done
    cargo tree --quiet --locked --manifest-path "$rs/Cargo.toml" -e normal \
        -p leyline-krun-worker --target aarch64-apple-darwin --prefix none \
        --format '{p}' >> "$scratch/tree"
    awk '{ print $1 }' "$scratch/tree" | sort -u > "$shipped"
    # An empty or truncated closure would pass every ignore. Both roots must
    # be in it, or the closure is not the one this lint means.
    for root_crate in leyline-cli leyline-krun-worker tokio; do
        grep -qx "$root_crate" "$shipped" || {
            printf 'deny ignores: shipped closure lacks %s; refusing to judge ignores\n' \
                "$root_crate" >&2
            exit 2
        }
    done
fi

entry_re='^\{ id = "(RUSTSEC-[0-9]{4}-[0-9]{4})", reason = "([^"]+)" \},?$'
failures=0
fail() { printf 'deny ignores: %s\n' "$1" >&2; failures=$((failures + 1)); }

while IFS= read -r entry; do
    id=$(printf '%s\n' "$entry" | sed -nE "s/$entry_re/\\1/p")
    reason=$(printf '%s\n' "$entry" | sed -nE "s/$entry_re/\\2/p")
    if [ -z "$id" ]; then
        fail "not a one-line { id = \"RUSTSEC-…\", reason = \"…\" } table: $entry"
        continue
    fi
    printf '%s\n' "$reason" | grep -Eq '(^|[^a-z0-9-])bead ley-line-open-[0-9a-f]{6}([^0-9a-f]|$)' \
        || fail "$id: reason names no bead (bead ley-line-open-xxxxxx)"
    expiry=$(printf '%s\n' "$reason" | sed -nE 's/.*expires ([0-9]{4}-[0-9]{2}-[0-9]{2}).*/\1/p')
    if [ -z "$expiry" ]; then
        fail "$id: reason has no expiry (expires YYYY-MM-DD)"
    elif [ "$expiry" \< "$today" ]; then
        fail "$id: ignore expired on $expiry (today $today): fix the advisory or re-date it with a reason"
    fi
    file=$(find "$db_root" -path "*/crates/*/$id.md" 2>/dev/null | head -n 1)
    if [ -z "$file" ]; then
        fail "$id: not in the advisory database under $db_root (typo, or run \`cargo deny fetch db\`)"
        continue
    fi
    crate=$(basename "$(dirname "$file")")
    if grep -qx "$crate" "$shipped"; then
        fail "$id: $crate is in the shipped closure (leyline-cli / leyline-krun-worker); fix it, do not ignore it"
    fi
done < "$scratch/entries"

if [ "$failures" -ne 0 ]; then
    printf 'deny ignores: %d problem(s) in %s\n' "$failures" "$deny" >&2
    exit 1
fi
printf 'deny ignores: %d ignore(s), each bead-linked, unexpired, and off the shipped closure\n' \
    "$(wc -l < "$scratch/entries" | tr -d ' ')"
