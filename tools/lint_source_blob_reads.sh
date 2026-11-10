#!/bin/sh
# Source-blob read gate — bead ley-line-open-12d305 (Σ axiom CA).
#
# `source_blobs` keys every row on BLAKE3(blob_bytes). A reader that selects
# `blob_bytes` and serves it without re-deriving the key serves whatever the
# row holds: ADR-0047 M0 found five such readers beside the verifying one.
# Every production read of those bytes goes through the one accessor,
# `leyline_core::source_blob` (rs/ll-core/core/src/source_blob.rs), which
# refuses a row whose bytes do not hash to its key.
#
# The rule: outside that module, no production Rust line may read
# `blob_bytes` out of `source_blobs`. A line is a read when it names
# `blob_bytes` (other than inside `length(...)`), a `SELECT` appears on it or
# on one of the two lines before it, and `source_blobs` is its table: on the
# line, or on the two lines after it as the FROM (or as a JOIN when the line
# already has its FROM). Incremental blob handles (`blob_open`) are refused
# outright.
# Writes (INSERT/UPDATE) are not reads.
#
# Test code is exempt: tests/ and benches/ subtrees, `*tests.rs` modules, and
# each `#[cfg(test)]` item, from the attribute to the line its braces
# balance on. (Not tools/lint_blake3.sh's "everything below the first
# `#[cfg(test)]`": checkout.rs carries a `#[cfg(test)]` helper above the raw
# read it hid.) Comment-only lines are ignored.
#
# Usage: tools/lint_source_blob_reads.sh [repo-root]
# Exit codes: 0 clean, 1 unsanctioned read(s) found, 2 internal error.
set -eu

root=${1:-.}
accessor="rs/ll-core/core/src/source_blob.rs"

test -d "$root/rs" || {
    printf 'source-blob reads: no rs/ under %s\n' "$root" >&2
    exit 2
}
test -f "$root/$accessor" || {
    printf 'source-blob reads: the accessor %s is missing\n' "$accessor" >&2
    exit 2
}

files=$(cd "$root" && find rs -name '*.rs' \
    -not -path '*/target/*' -not -path '*/tests/*' -not -path '*/benches/*' \
    -not -name '*tests.rs' | LC_ALL=C sort)

denied=$(cd "$root" && for f in $files; do
    [ "$f" = "$accessor" ] && continue
    awk -v file="$f" '
        { line[NR] = $0 }
        END {
            # Mark each `#[cfg(test)]` item test code: from the attribute
            # to the line its braces balance on (or its `;`, for an item
            # with no body). Braces are counted textually.
            for (i = 1; i <= NR; i++) {
                if (line[i] !~ /^[[:space:]]*#\[cfg\(test\)\]/) continue
                depth = 0; opened = 0
                for (j = i; j <= NR; j++) {
                    test_line[j] = 1
                    t = line[j]
                    o = gsub(/\{/, "{", t); c = gsub(/\}/, "}", t)
                    depth += o - c
                    if (o > 0) opened = 1
                    if (opened && depth <= 0) break
                    if (!opened && j > i && line[j] ~ /;[[:space:]]*$/) break
                }
                i = j
            }
            for (i = 1; i <= NR; i++) {
                if (i in test_line) continue
                l = line[i]
                if (l ~ /^[[:space:]]*(\/\/|\*|\/\*)/) continue
                if (l ~ /blob_open/) { print file ":" i ":" l; continue }
                stripped = l
                gsub(/length\([[:alnum:]_.]*blob_bytes\)/, "", stripped)
                if (stripped !~ /blob_bytes/) continue
                if (stripped ~ /INSERT|UPDATE/) continue
                sel = 0
                for (j = i - 2; j <= i; j++) if (j >= 1 && line[j] ~ /SELECT/) sel = 1
                if (!sel) continue
                # The table: on the line, or on the two lines after it as
                # the FROM (when the line has none) or a JOIN (when it does).
                tbl = (l ~ /source_blobs/)
                after = (l ~ /FROM/) ? "JOIN[[:space:]]+source_blobs" : "source_blobs"
                for (j = i + 1; j <= i + 2 && j <= NR; j++) if (line[j] ~ after) tbl = 1
                if (tbl) print file ":" i ":" l
            }
        }' "$f"
done)

if [ -n "$denied" ]; then
    printf 'ERROR: source_blobs bytes read outside %s (bead ley-line-open-12d305):\n' "$accessor" >&2
    printf '%s\n' "$denied" | sed 's/^/  /' >&2
    printf '\nRead them through leyline_core::source_blob::read / read_at (or\n' >&2
    printf 'leyline_cli_lib::tree::blob_bytes), which refuse bytes that do not\n' >&2
    printf 'hash to their key.\n' >&2
    exit 1
fi

count=$(printf '%s\n' "$files" | grep -c . || true)
printf 'source-blob reads: clean — %s production files; the one reader is %s\n' "$count" "$accessor"
