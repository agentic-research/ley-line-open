@0x9bd2953355bd438c;
# SourceFile — projected file metadata.
#
# T8.3 (decade ley-line-open-9d30ac, thread T8/capnp-as-protocol).
# Producer: rs/ll-open/cli-lib/src/cmd_parse.rs::parse_into_conn.
# Local SQL projection: `_source` table.
#
# Snapshot semantics: each parse run produces a fresh SourceFile log
# (truncate-and-rewrite). One message per file in the projected set.

using Common = import "common.capnp";

struct SourceFile {
  # Stable repo-relative path. Matches `_source.id`.
  id @0 :Text;

  # Tree-sitter language name (e.g. "go", "python", "rust").
  language @1 :Text;

  # Canonicalized absolute path. Equivalent to `_source.path`
  # post-be6136 (stored canonical so file:// URIs from LSP match
  # the join key on lookup).
  canonicalPath @2 :Text;

  # BLAKE3 of file bytes (T8.5 will use this in segment hashing).
  # Empty `bytes` if not yet populated — producer fills in during
  # parse if `compute_hash` is enabled.
  contentHash @3 :Common.Hash;

  # File mtime (Unix seconds) and size (bytes) — what the parse
  # was conducted against; lets consumers detect drift.
  mtime @4 :UInt64;
  size @5 :UInt64;
}
