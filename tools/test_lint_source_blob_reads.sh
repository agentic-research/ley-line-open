#!/bin/sh
# Fixture test for tools/lint_source_blob_reads.sh (bead ley-line-open-12d305).
#
# Each case is a throwaway tree holding the accessor and one production file.
# Every raw-read shape must fail AND name its file; the exempt shapes must
# pass. A gate that passes everything would pass the shipped tree too, so the
# red cases are what make the green one evidence.
set -eu

repo=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
lint="$repo/tools/lint_source_blob_reads.sh"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/llo-source-blob-reads.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
failures=0

# tree <case>: a fixture tree with the accessor; the case's file is
# rs/x/src/lib.rs, written from stdin.
tree() {
    t="$scratch/$1"
    mkdir -p "$t/rs/ll-core/core/src" "$t/rs/x/src"
    printf '%s\n' 'const S: &str = "SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1";' \
        > "$t/rs/ll-core/core/src/source_blob.rs"
    cat > "$t/rs/x/src/lib.rs"
}

expect() {
    want=$1 case=$2
    set +e
    sh "$lint" "$scratch/$case" > "$scratch/$case.out" 2>&1
    got=$?
    set -e
    if [ "$got" -ne "$want" ]; then
        printf 'FAIL %s: exit %s, want %s\n' "$case" "$got" "$want" >&2
        sed 's/^/    /' "$scratch/$case.out" >&2
        failures=$((failures + 1))
    elif [ "$want" -eq 1 ] && ! grep -q 'rs/x/src/lib.rs' "$scratch/$case.out"; then
        printf 'FAIL %s: the refusal does not name the file\n' "$case" >&2
        failures=$((failures + 1))
    else
        printf 'ok   %s\n' "$case"
    fi
}

tree raw-single-line <<'EOF'
fn blob(conn: &Connection) {
    conn.query_row("SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1", [], |r| r.get(0));
}
EOF
expect 1 raw-single-line

tree raw-multi-line-join <<'EOF'
fn blob(conn: &Connection) {
    conn.query_row(
        "SELECT b.blob_bytes FROM _source s \
           JOIN source_blobs b ON b.blob_hash = s.content_hash",
        [], |r| r.get(0));
}
EOF
expect 1 raw-multi-line-join

tree raw-select-on-line-above <<'EOF'
fn blob(conn: &Connection) {
    conn.prepare("SELECT
        blob_bytes
        FROM source_blobs");
}
EOF
expect 1 raw-select-on-line-above

# checkout.rs's shape on integration: a `#[cfg(test)]` helper ABOVE the raw
# production read. Everything-below-the-first-cfg(test) would miss it.
tree cfg-test-helper-above-raw-read <<'EOF'
#[cfg(test)]
fn helper() -> u8 {
    0
}

fn blob(conn: &Connection) {
    conn.query_row("SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1", [], |r| r.get(0));
}
EOF
expect 1 cfg-test-helper-above-raw-read

tree blob-open <<'EOF'
fn blob(conn: &Connection) {
    conn.blob_open(DatabaseName::Main, "source_blobs", "blob_bytes", 1, true);
}
EOF
expect 1 blob-open

tree read-inside-cfg-test-mod <<'EOF'
fn real() {}

#[cfg(test)]
mod tests {
    fn raw(conn: &Connection) {
        conn.query_row("SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1", [], |r| r.get(0));
    }
}
EOF
expect 0 read-inside-cfg-test-mod

tree exempt-shapes <<'EOF'
// SELECT blob_bytes FROM source_blobs: a comment is not a read.
fn ok(conn: &Connection) {
    conn.execute("INSERT OR IGNORE INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)", []);
    conn.execute("UPDATE source_blobs SET blob_bytes = ?1 WHERE blob_hash = ?2", []);
    conn.query_row("SELECT t.mode, length(b.blob_bytes) FROM _tree t \
        LEFT JOIN source_blobs b ON b.blob_hash = t.blob_hash", [], |r| r.get(0));
    conn.query_row("SELECT blob_bytes FROM observation_blobs WHERE blob_hash = ?1", [], |r| r.get(0));
    conn.query_row("SELECT blob_hash FROM source_blobs WHERE byte_len >= ?1", [], |r| r.get(0));
}
EOF
expect 0 exempt-shapes

tree test-subtree <<'EOF'
fn real() {}
EOF
mkdir -p "$scratch/test-subtree/rs/x/tests"
printf '%s\n' 'fn t(c: &Connection) { c.query_row("SELECT blob_bytes FROM source_blobs", [], |r| r.get(0)); }' \
    > "$scratch/test-subtree/rs/x/tests/raw.rs"
printf '%s\n' 'fn t(c: &Connection) { c.query_row("SELECT blob_bytes FROM source_blobs", [], |r| r.get(0)); }' \
    > "$scratch/test-subtree/rs/x/src/tree_tests.rs"
expect 0 test-subtree

# Build output in a separate target directory (`task release:hypervisor:record`
# builds in rs/target-hypervisor) is not source.
tree build-output <<'EOF'
fn real() {}
EOF
mkdir -p "$scratch/build-output/rs/target-hypervisor/debug/build/x/out"
printf '%s\n' 'fn t(c: &Connection) { c.query_row("SELECT blob_bytes FROM source_blobs", [], |r| r.get(0)); }' \
    > "$scratch/build-output/rs/target-hypervisor/debug/build/x/out/bindgen.rs"
expect 0 build-output

tree no-accessor <<'EOF'
fn real() {}
EOF
rm -f "$scratch/no-accessor/rs/ll-core/core/src/source_blob.rs"
expect 2 no-accessor

# The shipped tree is clean.
set +e
sh "$lint" "$repo" > "$scratch/shipped.out" 2>&1
got=$?
set -e
if [ "$got" -ne 0 ]; then
    printf 'FAIL shipped tree: exit %s\n' "$got" >&2
    sed 's/^/    /' "$scratch/shipped.out" >&2
    failures=$((failures + 1))
else
    printf 'ok   shipped tree\n'
fi

if [ "$failures" -ne 0 ]; then
    printf '%s source-blob-reads fixture case(s) failed\n' "$failures" >&2
    exit 1
fi
printf 'source-blob-reads fixture test: all cases pass\n'
