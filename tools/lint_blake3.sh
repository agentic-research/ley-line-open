#!/usr/bin/env bash
#
# Σ Phase 4 lint gate (ley-line-open-32201a follow-up).
#
# Forbids direct `blake3::hash(...)` calls in `rs/ll-open/` outside the
# allowlist in `tools/blake3-allowlist.txt`. Auto-allows tests/ and
# benches/ subtrees. Comments mentioning `blake3::hash` are ignored.
#
# Wired into `task ci` so the substrate's documented invariants
# (substrate.rs:28-50 — `BlobStore::put` is the storage primitive) stay
# load-bearing: a new direct-hash call without an allowlist entry fails
# CI rather than silently bypassing the trait.
#
# Exit codes:
#   0  no unsanctioned calls
#   1  unsanctioned call(s) found (output prints them)
#   2  internal error (missing allowlist, rg not on PATH, etc.)

set -euo pipefail

ALLOWLIST="tools/blake3-allowlist.txt"

if [[ ! -f "$ALLOWLIST" ]]; then
    echo "lint_blake3: $ALLOWLIST not found" >&2
    exit 2
fi

# Pull every direct call. Comment lines are filtered downstream.
# Use `grep -rn --include='*.rs'` to avoid taking on a ripgrep dep on
# CI runners (ubuntu-latest doesn't preinstall ripgrep despite the
# runner-image docs claiming so; grep is in every POSIX environment).
matches=$(grep -rn --include='*.rs' 'blake3::hash' rs/ll-open/ 2>/dev/null || true)

# Build the allowlist set: keep only "file:line" prefix (tab-separated columns).
allowed=$(grep -v '^#' "$ALLOWLIST" | grep -v '^$' | awk '{print $1}' | sort -u)

denied=()

# Per-file cache: the line number of the FIRST `#[cfg(test)]` attribute.
# Every `blake3::hash` match in that file at-or-below this line is
# treated as test code (matches the common pattern of `#[cfg(test)] mod
# tests { ... }` at the bottom of a production source file).
declare -A first_cfg_test_line

cfg_cutoff_for_file() {
    local file="$1"
    if [[ -n "${first_cfg_test_line[$file]+set}" ]]; then
        echo "${first_cfg_test_line[$file]}"
        return
    fi
    local cutoff
    cutoff=$(grep -n '^[[:space:]]*#\[cfg(test)\]' "$file" 2>/dev/null | head -1 | cut -d: -f1)
    if [[ -z "$cutoff" ]]; then
        cutoff="999999999"
    fi
    first_cfg_test_line[$file]=$cutoff
    echo "$cutoff"
}

while IFS= read -r match; do
    [[ -z "$match" ]] && continue

    file=$(echo "$match" | cut -d: -f1)
    line=$(echo "$match" | cut -d: -f2)
    content=$(echo "$match" | cut -d: -f3-)
    fileline="${file}:${line}"

    # 1. Auto-allow tests/ and benches/ subtrees.
    if [[ "$file" == */tests/* || "$file" == */benches/* ]]; then
        continue
    fi

    # 2. Auto-allow comment-only lines (// or /* ... */).
    if [[ "$content" =~ ^[[:space:]]*// ]] || [[ "$content" =~ ^[[:space:]]*\* ]]; then
        continue
    fi

    # 3. Auto-allow lines inside an in-file `#[cfg(test)]` block.
    cutoff=$(cfg_cutoff_for_file "$file")
    if (( line >= cutoff )); then
        continue
    fi

    # 4. Check the explicit allowlist.
    if grep -Fxq "$fileline" <<<"$allowed"; then
        continue
    fi

    denied+=("$match")
done <<<"$matches"

if [[ ${#denied[@]} -gt 0 ]]; then
    echo "ERROR: Σ Phase 4 lint — direct \`blake3::hash\` outside the allowlist:" >&2
    printf '  %s\n' "${denied[@]}" >&2
    echo "" >&2
    echo "If this is a legitimate Group B/C/ffi site, append it to" >&2
    echo "$ALLOWLIST with a one-line reason. Otherwise, migrate to" >&2
    echo "\`leyline_core::ContentAddressed\` / \`BlobStore\` (Phase 3 pattern)." >&2
    exit 1
fi

# Sanity: every allowlist entry MUST still exist in the source. A stale
# entry (line moved, file deleted) silently weakens the gate; surface it.
stale=()
while IFS= read -r entry; do
    [[ -z "$entry" ]] && continue
    if ! grep -Fxq "$entry" <<<"$(echo "$matches" | cut -d: -f1-2)"; then
        stale+=("$entry")
    fi
done <<<"$allowed"

if [[ ${#stale[@]} -gt 0 ]]; then
    echo "ERROR: Σ Phase 4 lint — stale allowlist entries (file:line no longer matches a \`blake3::hash\` call):" >&2
    printf '  %s\n' "${stale[@]}" >&2
    echo "" >&2
    echo "Remove the stale entries from $ALLOWLIST. Lines shift when code" >&2
    echo "is edited; the lint catches this so the allowlist doesn't rot." >&2
    exit 1
fi

total=$(echo "$matches" | grep -c '' || true)
allowed_count=$(echo "$allowed" | grep -c '' || true)
echo "blake3 lint: clean — $total total \`blake3::hash\` references; $allowed_count sanctioned production sites; rest tests/benches/comments"
