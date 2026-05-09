# Changelog

All notable changes to ley-line-open are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
the project adheres to [Semantic Versioning](https://semver.org/).

Each entry references the bead ID(s) tracking the work in
[rsry](https://github.com/agentic-research/rosary) so the full design
context, scoping notes, and review history are recoverable.

## [Unreleased]

### Added
- Static-assertions on the control-block field offsets (`OFF_GENERATION`,
  `OFF_ARENA_SIZE` must be 8-byte aligned). Compiles fail rather than
  silently producing UB on architectures requiring naturally-aligned
  atomics if a future field reorder violates the invariant.
- `ArenaHeader::validate_header(file_size) -> Result<u64, ArenaHeaderError>`
  with a typed `ArenaHeaderError` enum so callers can surface
  `VersionMismatch` distinctly from `BadMagic`, `BadActiveBuffer`, or
  `TruncatedFile` without parsing error strings.
- Test `t24_reader_rejects_zero_root_with_data` — pins the closure of
  the downgrade hole described under "Security" below.
- Test `t24_reader_accepts_zero_root_with_empty_data` — pins the
  legitimate fresh-arena path that is no longer rejected.

### Changed
- `verify_arena_root` now returns the verified slice
  `&buf[..data_size]` so the producer of "what got hashed" is the
  producer of "what gets deserialized." Eliminates the prior
  asymmetry where the verifier hashed a prefix but
  `sqlite3_deserialize` received the full padded buffer (works today
  because SQLite ignores trailing zeros; would silently corrupt any
  future non-SQLite backend).
- `ArenaHeader::buffer_size` now saturates at 0 for files smaller
  than the 4096-byte header, instead of underflowing `u64` and
  producing a near-`u64::MAX` value that panicked on slice indexing.
- `ArenaHeader::active_buffer_offset` returns `None` for truncated
  files (`file_size < HEADER_SIZE`).
- `set_current_root` is now `#[cfg(test)]`. The unfenced direct-write
  setter was previously `pub` but explicitly documented as test-only;
  gating it behind `cfg(test)` prevents production code from reaching
  the unfenced path even by accident.
- Dropped the redundant explicit `std::sync::atomic::fence(Release)`
  before each `bump_sync_counter_release` call. The Release-store on
  the `AtomicU64` already provides the ordering — the explicit fence
  was a belt-and-braces holdover that signaled uncertainty about the
  fence model.
- Stale doc comments referencing the V1 `generation` API have been
  rewritten across `control.rs`, `cmd_load.rs`, `cmd_daemon.rs`,
  `daemon/ops.rs`, and `fs/graph.rs`.
- Forward-reference doc-comment shortcodes (e.g. `T3.1`, `T3.3`,
  `T8.3`) have been replaced with the canonical `ley-line-open-…`
  bead IDs.

### Fixed
- `sign/src/ffi.rs`: defensive null-pointer checks before every
  `slice::from_raw_parts` call. Per the Rust safety contract,
  `from_raw_parts` requires non-null pointers even when `len == 0`
  — without these checks, a C caller passing `NULL` for any input
  would invoke undefined behavior. Mirrors the pattern in
  `fs/src/lib.rs`.

### Security
- **Removed the zero-root sentinel skip in `verify_arena_root`.**
  Pre-T2.4, when `controller.current_root() == [0; 32]`, the reader
  silently bypassed BLAKE3 verification under the rationale of
  "legacy compat with V1 writers." With v0.2.0's hard V2 cutover,
  no legacy V2 writer can produce data without a published root, so
  the skip became a dead-but-active downgrade path: any process
  able to write 32 bytes to the control block could disable content
  verification while leaving arbitrary bytes in the arena buffer for
  `sqlite3_deserialize`. The new contract:
  - `data_size == 0` → fresh arena, accepted (nothing to verify).
  - `data_size > 0` and `current_root == [0; 32]` → rejected loudly.
  - `data_size > 0` and `current_root != [0; 32]` → BLAKE3 compare.
- `ArenaHeader::validate_header` surfaces `VersionMismatch` as a
  typed error so operators can distinguish "stale arena, run the
  cutover" from "disk corruption" — previously both produced a
  generic "invalid arena header" string.

## [0.2.0] — 2026-05-09

Coordinated breaking release with mache v0.8.0 (paired via
[mache PR #365](https://github.com/agentic-research/mache/pull/365)).

This is the cutover wave to a content-addressed Σ substrate. **Old
binaries and new binaries are incompatible by design.** A v0.1.x
binary opening a v0.2.0 control or arena file (or vice versa) hits
an explicit VERSION-mismatch error rather than silently misreading
the byte layout.

### Added
- **Σ substrate type surface** — bead `ley-line-open-9e3a5f`. New
  module `leyline_core::substrate` declaring `Hash`,
  `ContentAddressed`, `BlobStore`, `RootPointer`, `RootSigner`. No
  behavior — implementations land in subsequent threads (T2 / T3).
- **`Controller::current_root()`** as the substrate's primary public
  identity field — bead `ley-line-open-baa90a`. Returns the 32-byte
  BLAKE3 root of the active arena payload.
- **`Controller::set_arena_with_root(path, size, root)`** — bead
  `ley-line-open-babf6a`. Atomic publish of `(path, size, root)`
  under a single Release-store on a private sync counter.
- **`ArenaHeader.data_size: u64`** — exact length of the live
  payload in the active buffer. Lets readers hash `buf[..data_size]`
  without parsing format-specific headers (replaces the unreliable
  SQLite `page_count` parse path).
- **Reader-side σ verification before deserialize** — bead
  `ley-line-open-bad8f1`. `SqliteGraph::from_arena` and
  `SqliteGraphAdapter::from_arena_writable` BLAKE3-hash the buffer
  prefix and refuse to load on mismatch.
- **`HotSwapGraph` polls `current_root`** — was `current_gen`.
  Idempotent root semantics: re-snapshot of unchanged db produces
  the same root, no spurious swap.
- **`leyline-vcs` and `leyline-sign` crates** — bead
  `ley-line-open-889173`. Lifted from the (private) ley-line repo
  with license bump Apache → AGPL with NOTICE. `leyline-vcs` is
  migrated to root-polling; `leyline-sign` is unchanged behaviorally.

### Changed
- **Wire-format breaking change.** Every state-publishing daemon op
  (`status`, `flush`, `load`, `reparse`, `enrich`, `snapshot`)
  replaces `"generation": <u64>` with
  `"current_root": "<64-char-hex>"`. Fresh controllers emit the
  zero-sentinel `"0000…0000"`. Pin test enforces both presence of
  `current_root` and absence of legacy `generation`. Bead
  `ley-line-open-baee26`.
- **Controller `.ctrl` and ArenaHeader `.arena` VERSION 1 → 2.**
  `Controller::open_or_create` and `ArenaHeader::active_buffer_offset`
  reject the opposite version with a descriptive error pointing at
  the coordinated-cutover requirement.
- **`Controller::generation()` removed from the public API.** The
  byte slot is preserved as a private sync atom (`AtomicU64`) used
  only for Acquire/Release fencing. Callers cannot reach it; all
  polling is by root.
- **`set_arena(path, size)` is now 2-arg** (was 3 with a generation
  parameter). Re-advertises path/size without advancing the root.
- **License: Apache 2.0 → AGPL-3.0-only with NOTICE.** Applies to
  the lifted `vcs` and `sign` crates; the rest of LLO was already
  AGPL.

### Removed
- Public `Controller::generation()` method (was `pub`, now gone).
- `"generation"` field from every daemon JSON response.
- `crates/vcs/` and `crates/sign/` from the (private) `ley-line`
  workspace — both moved to `rs/ll-open/{vcs,sign}/` here. Companion
  ley-line commit deletes the originals once this release is tagged.

### Fixed
- BLAKE3-hash recovery from arena bytes no longer relies on parsing
  SQLite's in-header `page_count` (which drifted from the actual
  serialized byte count under WAL/freelist patterns). Replaced by
  the explicit `ArenaHeader.data_size` field.

### Migration

Producers and consumers must update simultaneously. There is no
forward-compatibility shim. The recommended migration order:

1. Stop all running v0.1.x daemons and consumers (mache).
2. Delete or archive existing `.ctrl` and `.arena` files; v0.1.x
   files cannot be read by v0.2.0 binaries.
3. Upgrade LLO and mache to v0.2.0.
4. Restart producers; they will create v2 control + arena files.
5. Restart consumers; they will read the new format.

If upgrading produces a startup error like
`control block VERSION mismatch: file has v1, this binary expects v2`,
that is the cutover working as intended — remove the stale file.

## [0.1.1] — pre-changelog

See `git log v0.1.0..v0.1.1` for the commit-level history. This
release predates the structured changelog.

## [0.1.0] — pre-changelog

Initial public release of ley-line-open as the OSS substrate for the
ley-line stack. See `git log v0.1.0` for the initial-commit set.

[Unreleased]: https://github.com/agentic-research/ley-line-open/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.2.0
[0.1.1]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.1.1
[0.1.0]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.1.0
