#!/bin/sh
# Documentation CONTRACT test — bead ley-line-open-ef5c5a.
#
# Distinct from tools/check_architecture_vocabulary.sh, which asserts that
# certain strings are present or absent. Presence is a weak signal: a document
# can contain every required term and still make a false claim, which is how
# the defects this script catches survived a green vocabulary gate.
#
# Every check here is a CLAIM verified against the repository. If the code
# moves, the check fails — it cannot drift into decoration.
#
# Scope is the five documents named by the bead.
set -eu

root=${1:-.}
docs="$root/README.md $root/docs/ARCHITECTURE.md $root/docs/TABLE_CONTRACT.md $root/rs/README.md"
rust_docs="$root/rs/ll-open/fs/src/chunked.rs"
all="$docs $rust_docs"
failed=0

fail() {
  printf 'doc-claim FAILED: %s\n' "$1" >&2
  failed=1
}

# --- Claim 1: every decade a document names must exist -----------------------
#
# Catches renamed or imagined decades. `dataflow-substrate` was referenced five
# times across these files while the decade on disk is `analysis-substrate`.
for decade in $(grep -ohE '[a-z0-9-]+-substrate decade' $all 2>/dev/null \
                 | sed 's/ decade$//' | sort -u); do
  if [ ! -f "$root/docs/decades/$decade.md" ]; then
    fail "names a '$decade' decade; docs/decades/$decade.md does not exist"
  fi
done

# --- Claim 2: the SHA-256 exception set matches the code ---------------------
#
# Sigma is BLAKE3-locked (ADR-0032 D5). Exactly three crates may carry sha2
# as a real dependency, all for INTEROP digests owned by external specs
# rather than substrate addresses: leyline-sign (canonical_kid lineage,
# signet ADR-012), leyline-envelope (in-toto Statement v1 subject digests —
# the in-toto spec names sha256, and rosary byte-compat requires computing
# it; bead be5f86), and leyline-cli-lib (self update verifies release assets
# against SHA256SUMS, sha256 by coreutils/GitHub-release convention; bead
# 321ded). If this set changes, the documented exception in ADR-0032 D5 is
# stale and must be rewritten rather than quietly broadened.
expected_sha2_crates="rs/ll-open/cli-lib/Cargo.toml rs/ll-open/envelope/Cargo.toml rs/ll-open/sign/Cargo.toml"
actual_sha2_crates=$(
  find "$root/rs" -name Cargo.toml -not -path '*/target/*' -not -path '*worktree*' 2>/dev/null \
    | while read -r f; do
        awk '/^\[dependencies\]/{d=1;next} /^\[/{d=0} d&&/^sha2/{print FILENAME}' "$f"
      done | sed "s|^$root/||" | sort -u | tr '\n' ' ' | sed 's/ $//'
)
if [ "$actual_sha2_crates" != "$expected_sha2_crates" ]; then
  fail "sha2 dependency set changed: expected '$expected_sha2_crates', found '$actual_sha2_crates' — update ADR-0032 D5's stated exception and this check together"
fi

# --- Claim 3: authority assertions the ADR settles ---------------------------
#
# These are contradictions, not vocabulary. Each was simultaneously true and
# false across the doc set before ADR-0032 §D4 assigned authority.
for forbidden in \
  'The .db file is the contract' \
  'The Σ substrate — runtime model' \
  'core tables are the canonical substrate' \
  'SHA-256 appears in exactly two places' \
  'Reads the arena via pure-Go capnp deserialization'
do
  if grep -F "$forbidden" $all >/dev/null 2>&1; then
    fail "forbidden authority assertion present: $forbidden"
  fi
done

# --- Claim 4: CDC's private cache is not the canonical substrate -------------
#
# content_chunks/content_manifest are a DERIVED accelerator; nodes.record stays
# authoritative (ADR-0033 D1). Calling the cache canonical inverts that.
if grep -nE '(content_chunks|content_manifest|chunk (manifest|cache))[^.]{0,80}(canonical substrate|is the substrate|authoritative substrate)' \
     $all >/dev/null 2>&1; then
  fail "CDC's private chunk cache is described as the canonical substrate"
fi

# --- Claim 5: the identity vocabulary is present ------------------------------
#
# Retained from check_architecture_vocabulary.sh, now over five files. Weak on
# its own — kept because absence is still a real regression.
for required in \
  "Cap'n Proto segment root" \
  'SQL projection ABI'
do
  if ! grep -F "$required" $docs >/dev/null 2>&1; then
    fail "missing identity term: $required"
  fi
done

if [ "$failed" -ne 0 ]; then
  printf '\nSee docs/adr/0032-declared-decompositions.md §D4 for which domain owns what.\n' >&2
  exit 1
fi
printf 'doc claims verified against code\n'
