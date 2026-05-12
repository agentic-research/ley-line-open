# Changelog

All notable changes to ley-line-open are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
the project adheres to [Semantic Versioning](https://semver.org/).

Each entry references the bead ID(s) tracking the work in
[rsry](https://github.com/agentic-research/rosary) so the full design
context, scoping notes, and review history are recoverable.

## [Unreleased]

Post-v0.2.0 work landed on `main` but the LLO daemon binary has not
been re-tagged. The OCI image and the Go bindings module *have* been
versioned independently — see the per-section notes.

### Added

- **`clients/go/leyline-schema` Go module** tagged at
  `clients/go/leyline-schema/v0.2.1` (#3, 2026-05-10). Multi-module
  monorepo: one versionable contract per Go-side consumer. Ships
  generated `*.capnp.go` for `common`, `ast`, `binding`, `head`,
  `source`, and `daemon`. CI gate `.github/workflows/leyline-schema-go.yml`
  enforces regen-diff parity plus `go test ./...` on the bindings.
- **Distroless OCI image `ley-line-open:0.2.1`** built via
  [`krust`](https://github.com/imjasonh/krust) (#3, 2026-05-10).
  cargo-zigbuild produces a static musl binary; `image.Dockerfile`
  COPYs it onto `cgr.dev/chainguard/static`. ~20 MB. Default CMD is
  `daemon --mcp-port 8384 --mcp-bind 0.0.0.0` (headless, MCP HTTP
  only). `task image` and `task image:smoke` wrap the build + probe.
  Replaces the apko/melange path that stalled on Apple Silicon with a
  30 GB workspace.
- **Five new daemon ops mache needed** (#7, 2026-05-11): `find_callees`,
  `get_refs_map`, `get_defs_map`, `get_schema`, `get_db_path`. All
  added to `daemon.capnp` additively (new requests + responses + a
  `SchemaTier`) and wired through `socket.rs` + `mcp.rs`. Tagged into
  the Go module at `clients/go/leyline-schema/v0.2.2`.
- **Cross-runtime fixture gate for the daemon JSON protocol** —
  bead `ley-line-open-b5a77b` (A-1, PR #6, 2026-05-11). Single fixture
  file `rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json` pins
  each op's request shape, response shape, and required-key set. Two
  gates consume it: a Rust integration test asserts the handler emits
  every required key, and a Go test (`daemon_protocol_test.go`)
  decodes each fixture into the typed Go binding without
  `UnmarshalTypeError`. Extends the T8.10 segment-file fixture pattern
  (`6b7d43`) to the JSON wire.
- **Schema↔reality reconciliation** — bead `ley-line-open-b631c8`
  (A-2, PR #8 first half, 2026-05-11). `daemon.capnp` extended
  additively per ADR-0014 §2: `StatusResponse` gained `phase`,
  `currentRoot`, `enrichment`, `headSha`, `lastReparseAtMs`, `error`
  (@4-@9); `ReparseResponse` gained `currentRoot`, `parsed`,
  `unchanged`, `deleted`, `errors`, `changedFiles`;
  `SnapshotResponse`, `LoadResponse`, `EnrichResponse` each gained
  `currentRoot`. New `FlushRequest`/`Response`. 13 of 14 fixture ops
  decode into typed Go bindings; `query` is a documented structural
  skip (column-keyed maps vs `List(Text)`) tracked as a follow-up.
- **Typed JSON wire (`daemon/wire.rs`)** — bead `ley-line-open-b69606`
  (A-3, PR #8 second half, 2026-05-11). New module of serde-derived
  request/response structs mirroring `daemon.capnp` plus a tagged
  `BaseRequest` enum (`#[serde(tag = "op", rename_all = "snake_case")]`)
  covering all 23 base ops. `ops.rs::handle_base_op(ctx, wire_line)`
  is the typed entrypoint; `handle_base_op_value(ctx, Value)` is the
  Value-accepting sibling `socket.rs` uses to skip a stringify+reparse
  round-trip. Every handler now takes typed args (e.g.
  `op_lsp_symbols(ctx, args: &LspFile)`) and builds typed response
  structs serialized via `wire::to_wire`. JSON-as-carrier doctrine
  preserved: shell debuggability unchanged, MCP HTTP unchanged, the
  schema is the load-bearing contract for shape. Tagged into the Go
  module at `clients/go/leyline-schema/v0.2.3`.

### Changed

- **`op_list_children` no longer SELECTs `nodes.record`** — A-3
  Copilot review (PR #8). Directory listings could carry the full
  file contents of every child (megabytes per row in some repos).
  `record` is now `Option<String>` on `Node` with
  `skip_serializing_if`, so listings simply omit the field. Consumers
  needing a specific node's record call `op_get_node` or
  `op_read_content`. SQL NULL is preserved end-to-end as `None`
  rather than collapsed to `""`.
- **CI action SHAs pinned to Node 24** — PR #5, 2026-05-10. README's
  mermaid sequence diagram fixed (parse error caught upstream).

### Notes

- LLO daemon binary tag remains `v0.2.0`. The image tag `ley-line-open:0.2.1`
  and the three `clients/go/leyline-schema/*` tags above are
  independent versioning surfaces — see ADR-0014 for the why (one
  schema, one tag per consumer-language module).
- Cloister-side `udsForward` integration is the forcing function for a
  future RPC framing ADR (ADR-0014 line 105). Not triggered yet;
  tracked under bead `ley-line-open-` follow-up.

## [0.2.0] — 2026-05-09

Coordinated breaking release with [mache v0.8.0](https://github.com/agentic-research/mache/releases/tag/v0.8.0)
(paired via mache PR #365 + #366). Cutover wave to a content-addressed
Σ substrate. **Old binaries and new binaries are incompatible by
design.** A v0.1.x binary opening a v0.2.0 control or arena file (or
vice versa) hits an explicit VERSION-mismatch error rather than
silently misreading the byte layout.

### Added
- **Σ substrate type surface** — bead `ley-line-open-9e3a5f`. New
  module `leyline_core::substrate` declaring `Hash`,
  `ContentAddressed`, `BlobStore`, `RootPointer`, `RootSigner`. No
  behavior — implementations land in subsequent threads.
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
- **Static-assertions on control-block field alignment**
  (`OFF_GENERATION`, `OFF_ARENA_SIZE` must be 8-byte aligned).
  Compile fails rather than silently producing UB on architectures
  requiring naturally-aligned atomics if a future field reorder
  violates the invariant.
- **`ArenaHeader::validate_header(file_size) -> Result<u64,
  ArenaHeaderError>`** with a typed `ArenaHeaderError` enum so
  callers surface `VersionMismatch` distinctly from `BadMagic`,
  `BadActiveBuffer`, or `TruncatedFile` without parsing error
  strings.
- New tests pinning the closed downgrade hole
  (`t24_reader_rejects_zero_root_with_data`) and the legitimate
  fresh-arena path (`t24_reader_accepts_zero_root_with_empty_data`).

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
- **`bump_sync_counter_release` uses `fetch_add(1, Release)`** —
  was load-modify-store under a single-writer assumption that the
  file-backed mmap doesn't enforce across processes. The Release
  ordering still gives the happens-before pair with
  `sync_counter_acquire`; the atomic `fetch_add` closes the lost-
  increment window if the assumption is ever violated.
- **`set_arena(path, size)` is now 2-arg** (was 3 with a generation
  parameter). Re-advertises path/size without advancing the root.
- **`verify_arena_root` returns `&buf[..data_size]`** so the
  producer of "what got hashed" is the producer of "what gets
  deserialized." Eliminates the prior asymmetry where the verifier
  hashed a prefix but `sqlite3_deserialize` received the full
  padded buffer.
- **`ArenaHeader::buffer_size` saturates at 0** for files smaller
  than the 4096-byte header, instead of underflowing `u64` and
  producing a near-`u64::MAX` value that panicked on slice indexing.
- **`ArenaHeader::active_buffer_offset`** returns `None` for
  truncated files (`file_size < HEADER_SIZE`).
- **`cmd_daemon::warm_start_from_arena`** now uses `validate_header`
  → operators see "stale VERSION 1 arena, run the cutover"
  distinctly from "torn header" or "truncated file" rather than a
  generic "header invalid" warning.
- **`set_current_root` is `#[cfg(test)]`.** The unfenced direct-
  write setter was previously `pub` but documented as test-only;
  cfg-gating prevents production code from reaching the unfenced
  path even by accident.
- **License: Apache 2.0 → AGPL-3.0-only with NOTICE.** Applies to
  the lifted `vcs` and `sign` crates; the rest of LLO was already
  AGPL.
- Stale doc comments referencing the V1 `generation` API rewritten
  in the substrate's hot-path files: `control.rs`, `cmd_load.rs`,
  `cmd_daemon.rs`, `daemon/ops.rs`, `fs/graph.rs`,
  `lsp/src/project.rs`.
- Operator-facing strings (error messages, log warnings) scrubbed
  of T-shortcodes: `ArenaHeaderError::VersionMismatch` Display,
  `Controller::open_or_create` VERSION-mismatch bail, the
  `cmd_parse::write_head_after_parse` warn line. Substrate
  internals still use `T2.x` / `T8.x` shorthand in dev-facing
  comments paired with the canonical `ley-line-open-…` bead ID
  in adjacent context (full sweep deferred — comment churn).

### Removed
- Public `Controller::generation()` method.
- `"generation"` field from every daemon JSON response.
- `crates/vcs/` and `crates/sign/` from the (private) `ley-line`
  workspace — both moved to `rs/ll-open/{vcs,sign}/` here.
- Zero-root sentinel skip in `verify_arena_root` (see Security).

### Fixed
- BLAKE3-hash recovery from arena bytes no longer relies on parsing
  SQLite's in-header `page_count` (drifted from actual serialized
  byte count under WAL/freelist patterns). Replaced by
  `ArenaHeader.data_size`.
- `sign/src/ffi.rs`: defensive null-pointer checks before every
  `slice::from_raw_parts` call. Per the Rust safety contract,
  `from_raw_parts` requires non-null pointers even when `len == 0`;
  without these checks, a C caller passing `NULL` would invoke UB.
  Mirrors the pattern in `fs/src/lib.rs`.

### Security
- **Removed the zero-root sentinel skip in `verify_arena_root`.**
  Pre-cutover, when `controller.current_root() == [0; 32]`, the
  reader silently bypassed BLAKE3 verification under the rationale
  of "legacy compat with V1 writers." With this release's hard V2
  cutover, no legacy V2 writer can produce data without a published
  root, so the skip became a dead-but-active downgrade path: any
  process able to write 32 bytes to the control block could disable
  content verification while leaving arbitrary bytes in the arena
  buffer for `sqlite3_deserialize`. The new contract:
  - `data_size == 0` → fresh arena, accepted (nothing to verify).
  - `data_size > 0` and `current_root == [0; 32]` → rejected loudly.
  - `data_size > 0` and `current_root != [0; 32]` → BLAKE3 compare.
- `ArenaHeader::validate_header` surfaces `VersionMismatch` as a
  typed error so operators distinguish "stale arena, run the
  cutover" from "disk corruption."

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
[clients/go/leyline-schema/v0.2.1]: https://github.com/agentic-research/ley-line-open/releases/tag/clients%2Fgo%2Fleyline-schema%2Fv0.2.1
[clients/go/leyline-schema/v0.2.2]: https://github.com/agentic-research/ley-line-open/releases/tag/clients%2Fgo%2Fleyline-schema%2Fv0.2.2
[clients/go/leyline-schema/v0.2.3]: https://github.com/agentic-research/ley-line-open/releases/tag/clients%2Fgo%2Fleyline-schema%2Fv0.2.3
[0.2.0]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.2.0
[0.1.1]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.1.1
[0.1.0]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.1.0
