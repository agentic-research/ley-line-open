# Turso `sqlite3_deserialize` Zero-Copy — Arena Validation (Result)

**Date:** 2026-08-03 · **Verdict: zero-copy attach works, verified against the real LLO arena.**

Follow-up to
[2026-07-21-turso-arena-compat-falsifier.md](2026-07-21-turso-arena-compat-falsifier.md).
That falsifier landed a **copy-based** `sqlite3_deserialize` MVP
(`jamestexas/turso@feat/sqlite3-deserialize-mvp`, commit `61810d9e5`) as PR
[#7947](https://github.com/tursodatabase/turso/pull/7947), which was
auto-closed by upstream's PR bot rather than reviewed. The copy-based path
proves the API works end-to-end but doesn't buy LLO's attach economics — it
still pays an image-sized copy on every attach, the exact thing a
content-addressed, immutable-snapshot arena wants to avoid.

This entry documents the **zero-copy** follow-up
(`jamestexas/turso@feat/sqlite3-deserialize-mvp`, commit `22496141a`), built
and verified locally before filing an issue upstream (rather than resubmitting
another PR for review).

## What changed

`sqlite3_deserialize` now branches on `SQLITE_DESERIALIZE_READONLY`:

- **READONLY set:** the caller's pointer is wrapped in a `BorrowedImage`
  (owns it only if `FREEONCLOSE` is set) and attached via a
  `BorrowedImageFile` through `OpenOptions::storage` — no copy of the image is
  made. Page reads slice directly into the caller's buffer.
- **READONLY unset:** unchanged copy-based path (mutable zero-copy needs
  fixed-capacity or `RESIZEABLE` support — tracked as a further follow-up, not
  done here).

`RESIZEABLE` is still rejected. Ownership: with `FREEONCLOSE`, `sqlite3_free`
runs when the connection (the last `Arc` holder) drops; without it, the
caller must keep the buffer valid for the connection's lifetime — matching
upstream SQLite's actual contract (P is used in place regardless of
`FREEONCLOSE`; the flag only governs whether `sqlite3_free` runs on close).

## Verification

1. **Unit tests** (`cargo test -p turso_sqlite3`, 4 deserialize tests, all
   pass): correctness round-trip, write-rejected-on-readonly, and a
   **live-mutation falsifier** — corrupt the source buffer's tail *after* a
   successful `READONLY` deserialize but *before* any query touches that
   page, then show the very first read observes the corruption
   (`SQLITE_CORRUPT`). This can only happen if reads are live over the
   caller's memory, not a copy.
2. **C differential suite** (`bindings/c/tests/sqlite3_tests.c`): the same
   round-trip + write-rejection test runs against both real `-lsqlite3` and
   `-lturso_sqlite3` — passes identically on both, confirming behavioral
   parity for the new path, not just Turso-side correctness.
3. **Real arena validation** (this entry): the exact 38MB, 19,261-node arena
   image from the 2026-07-21 falsifier
   (`~/.cache/turso-llo-probe/arena_compat.db`, `source_blobs` generated
   column dropped) was loaded into memory and attached through the new
   `READONLY` zero-copy path:
   - All table counts matched exactly: `nodes`=19,261, `_ast`=19,260,
     `node_defs`=336, `node_refs`=1,773.
   - A 200-row join (`node_defs` ⋈ `node_content` on `node_hash`) reading
     `hex(node_hash)` blob bytes succeeded, mirroring the original F1
     falsifier's query shape.
   - **Falsification on real data:** used `pragma dbstat` to confirm `nodes`'
     own B-tree leaf pages (pages 30–123 of this image, page size 65536,
     verified via `pagetype='leaf'`), corrupted a range of those pages
     (50–70) in the source buffer *before* any query on a fresh connection,
     then ran `SELECT count(*), sum(length(source_file)) FROM nodes`
     (`source_file` is not covered by any index — confirmed via `EXPLAIN
     QUERY PLAN`: `SCAN nodes`, so the base table's own pages must be read).
     Result: `SQLITE_CORRUPT` ("Invalid page type: 242") on the first read of
     the corrupted region — proving the read path pulled live from the
     mutated source buffer, not a copy taken at deserialize time.
     (First attempt at this falsifier corrupted the file's last page, which
     `dbstat` showed actually belongs to `sqlite_autoindex__ast_pointer_1`,
     not `nodes` — a reminder that page-physical-layout assumptions need
     `pragma dbstat`, not guesswork, on a real multi-table image.)

## What this means

The zero-copy attach is not just an architectural gesture — it's demonstrated,
end-to-end, against the actual LLO arena this feature exists for. The
remaining gap to LLO's target shape (mutable, growable Turso-side writes,
still snapshotting to vanilla-format immutable reads) is unchanged from the
07-21 entry: this closes the read-side "zero-copy buffer attach" gap
specifically.

**Next step:** file the upstream issue (draft at
`~/remotes/art/turso-issue-draft.md`) referencing this working zero-copy
branch instead of resubmitting a PR, since the prior PR (#7947) was
auto-closed without review.

## Reproduction

Branch: `jamestexas/turso@feat/sqlite3-deserialize-mvp`, commit `22496141a`.
Real arena bytes: `~/.cache/turso-llo-probe/arena_compat.db` (from the 07-21
falsifier run). Ad hoc validation harness (not committed — a throwaway C
program linking the locally built `libturso_sqlite3` dylib) is available on
request; the same assertions are captured permanently in
`bindings/c/src/lib.rs`'s `sqlite3_deserialize_readonly_reads_are_live_over_the_source_buffer`
test, which reproduces the same falsifier against a smaller synthetic image
under `cargo test -p turso_sqlite3`.
