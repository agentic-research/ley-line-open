//! Version + wire-compatibility constants for the `leyline_version` op
//! (bead `ley-line-open-cb8960`).
//!
//! These are the daemon's runtime answers to "what binary / schema /
//! wire-format am I, and what schema-client versions am I compatible
//! with?" Clients call `leyline_version` at connect time, compare these
//! against their own embedded versions, and fail-fast on mismatch.
//!
//! # Discipline
//!
//! `RELEASE_VERSION` comes from `CARGO_PKG_VERSION` — bumps automatically
//! every release — and `BINARY_VERSION` is it plus a `-dev+<shortsha>`
//! suffix on every build that is not the tagged release (bead
//! `ley-line-open-5139a3`). `BUILD_DATE` comes from `$LLO_BUILD_DATE` if set in
//! the build environment, otherwise `"unspecified"`. The other three
//! constants are hand-pinned here — there is no source of truth for
//! "minimum compatible schema-client version" or "current wire-format
//! major" other than the daemon's own decision, so they live here:
//!
//! - `WIRE_FORMAT_MAJOR` — bump on incompatible JSON envelope changes.
//!   The v0.4.2→v0.4.3 transition (data nesting + u64 stringification)
//!   would have been a major bump had this op existed at the time.
//! - `COMPAT_MIN_SCHEMA_VERSION` — earliest schema-client that can
//!   safely talk to this daemon. Bump when older clients lose a
//!   field they depend on; raise the floor.
//! - `SCHEMA_VERSION` — latest published public schema-client contract,
//!   shared by the Rust schema crates and nested Go module.
//!   Bump only when the public Cap'n Proto/schema surface changes and a
//!   matching `clients/go/leyline-schema/vX.Y.Z` tag is published. Private
//!   storage changes do not move it.
//!
//! These are the *only* hand-maintained version facts; the rest of
//! the substrate's compatibility surface (cbea02) derives from them.

/// The version this build reports on every surface: `leyline --version`,
/// the `leyline_version` op's `binary_version`, MCP `serverInfo`, the
/// `leyline self` user agent and the arena owner record. A release build
/// (`LEYLINE_RELEASE_VERSION` set by `release.yml` from the tag) reports the
/// bare [`RELEASE_VERSION`]; every other build reports
/// `X.Y.Z-dev+<shortsha>[.dirty]`, so an unreleased build is never mistaken
/// for the release (bead `ley-line-open-5139a3`). Computed by `build.rs`
/// from `src/build_version.rs`, from inputs only, never the clock.
pub const BINARY_VERSION: &str = env!("LEYLINE_BUILD_VERSION");

/// The release line this build belongs to: the crate version
/// (`CARGO_PKG_VERSION`), without a build suffix. Committed artifacts that
/// must not vary per commit (`compatibility.json`, `server.json`) and
/// version arithmetic (`leyline self update`'s downgrade check) use this.
pub const RELEASE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The public Rust/Go schema-client version this daemon's wire shapes target.
/// This is intentionally independent from `BINARY_VERSION`, even though the
/// v0.11.0 release advances both. Binary-only releases leave this literal
/// unchanged; a public consumer-contract release requires a matching nested
/// Go-module tag.
pub const SCHEMA_VERSION: &str = "0.25.0";

/// Current major version of the JSON wire envelope shape. Bumps on
/// incompatible changes (renames, removals, type changes); additions
/// are non-breaking inside the same major.
///
/// **History:**
/// - 1: the v0.4.3 wire shape — `data: {...}` nested under `Event`,
///   `generation`/`prior_generation` as quoted strings per capnp_json u64
///   convention.
/// - 2: current (ADR-0047 M1, bead `ley-line-open-73c6ee`). `read_source`
///   and `propose` answer `store_root` (the parser-free `storeRoot`) and
///   never send `tree_root`. A major-1 client that decodes leniently would
///   read the missing `tree_root` as "no root recorded", so the major moves
///   and the handshake refuses it instead. ADR-0047 M2 (merkle-ast-v3) does
///   not move it: no field is renamed, removed or retyped; `node_hash` values
///   change lineage, which `ir_schema_version` on the same handshake names.
///
/// A v0.4.2 daemon at wire-format major 0 would not satisfy a v0.4.3+
/// client's expectations, even though both predate this constant — the
/// op didn't exist then, so the mismatch surfaced as silent
/// `parseUint64`-returns-0 drift instead of a clean handshake failure.
/// The `node_hash` ADDRESS LINEAGE this binary produces, published on the
/// version handshake and in `compatibility.json` (bead `ley-line-open-348de6`).
///
/// `node_hash`'s preimage carries the canonical κ kind, so a change to the κ
/// map — or to a tree-sitter grammar that renames productions — rewrites
/// addresses for byte-identical sources. `_meta.ir_schema_version` records that
/// inside a projection, which is enough for LLO's own incremental guard but
/// useless to a consumer that reads the projection and never re-parses.
///
/// v0.11.0 crossed `merkle-ast-v1` -> `merkle-ast-v2` with no consumer-facing
/// channel at all. This is that channel. A consumer memoizing on `node_hash`
/// compares this against the lineage it cached under; a mismatch means its
/// cached addresses are not comparable and must be discarded.
///
/// `merkle-ast-v3` (ADR-0047 D-i, step M2, bead `ley-line-open-73add7`) keys
/// the fold with `derive_key("leyline/ast-node/v3", …)`; every `node_hash`
/// moved from its v2 value. The wire carries `node_hash` with the same type
/// and framing (hex in `validate`'s `emit_ast` rows, member hashes behind a
/// sheaf region hash), so [`WIRE_FORMAT_MAJOR`] stays 2: this field, on the
/// version handshake, is the lineage signal for a client that reads it (the
/// daemon does not require the handshake first), and a v2 value compared
/// with a v3 one is unequal, never a false match.
///
/// Bump lives in `cmd_parse::IR_SCHEMA_VERSION`, which re-exports this.
pub const IR_SCHEMA_VERSION: &str = "merkle-ast-v3";

/// Shape of the SQLite projection: which tables exist and which columns they
/// carry.
///
/// Distinct from [`IR_SCHEMA_VERSION`], which versions the CONTENT of
/// `node_hash` addresses, and from [`SCHEMA_VERSION`], which versions the
/// public schema-client contract. Neither says anything about the table
/// shape, so until now a consumer could only detect a projection change by
/// querying `sqlite_master` — which is exactly why mache carries a
/// byte-identical DDL pin and a conformance test to catch drift.
///
/// This is the same gap `IR_SCHEMA_VERSION` was added to close, one layer
/// down: that comment records v0.11.0 crossing `merkle-ast-v1` ->
/// `merkle-ast-v2` "with no consumer-facing channel at all". The projection
/// has crossed shapes with no channel either.
///
/// Recorded as `_meta.projection_schema_version`.
///
/// - ABSENT — written before this key existed; carries the per-node
///   `_ast_pointer` table rather than the per-file `_ast_blob` map.
/// - `projection-v2` — `_ast_blob` + `_ast.blob_ord`.
/// - `projection-v3` — `node_defs`/`node_refs` carry their own span and
///   grammar kind, so resolving a definition no longer JOINs `_ast`.
/// - `projection-v4` — `nodes.parent_id` is DERIVED, not stored: a VIRTUAL
///   generated column over `id` and `name`. Reads are unaffected —
///   `SELECT parent_id`, `SELECT *`, and `WHERE parent_id = ?` all behave as
///   before, the column keeps its declared position, and the index still
///   serves the lookup. Writers are NOT: an INSERT or UPDATE naming
///   `parent_id` is rejected at prepare time ("cannot INSERT into generated
///   column"). Two introspection surfaces move with it — `pragma_table_info`
///   does not list generated columns AT ALL (only `pragma_table_xinfo`
///   does), and the `sqlite_master` DDL text changes, so a byte-identical
///   pin needs re-pinning.
/// - `projection-v5` — file-scoped integer nids (bead `ley-line-open-17c271`).
///   Every node-keyed column re-types: `nodes.id`/`_ast.node_id`/
///   `node_refs.node_id`/`node_defs.node_id`/`_lsp*.node_id` (TEXT ancestry
///   paths) become integer `nid = (file_id << 24) | ordinal`, directories in
///   negative space (`-dir_id`); `parent_id` → stored `parent_nid`;
///   `container_node_id` → `container_nid`; `referrer_node_id` →
///   `referrer_nid`; sibling `ord` stored. New interning tables `names`/
///   `dirs`/`files`/`kinds`; `_source` gains `file_id`; `_ast` loses
///   `source_id`/`node_kind`/`blob_ord` (file = `nid >> 24`, kind interned,
///   blob ordinal = `nid & 0xFFFFFF`); `node_refs`/`node_defs` lose
///   `source_id`; `_ast_blob` re-keys on `file_id`; `nodes` loses
///   `name` (interned `name_id`; AST display names derive from kind + ord).
///   Display paths are DERIVED: `v_node_path`/`v_node_name` views and the
///   `node_path`/`resolve_path` resolvers in `leyline-schema`. A pre-v5
///   arena is REFUSED at parse open (the projection is derived-only —
///   rebuild by cold reparse), never migrated in place. The capnp wire
///   surface is untouched: Phase A evicted the locator from every hashed
///   preimage, which is what makes this a projection-only break.
/// - `projection-v6` — `_file_index` removed (bead `ley-line-open-8f37c4`).
///   Its `(path, mtime, size)` was a second copy of the file row's own
///   `nodes.size`/`nodes.mtime` (stamped from the filesystem since #381)
///   keyed by `_source.file_id`; the incremental-reparse prefilter reads
///   that join (`leyline_ts::schema::read_file_stats`). The "a projection
///   exists here" probe moves from `_file_index` to `_source`. Nothing else
///   changes; a v5 arena is refused at parse open like every earlier shape.
/// - `projection-v7` — the store's manifest and the parse's diagnostics
///   (ADR-0045, bead `ley-line-open-bb8aa6`). `_tree (path, mode, blob_hash,
///   mtime, size)` holds every file the store holds, with its git mode, and
///   every directory; `_source` keeps only the files the parse projects.
///   `_parse_diagnostics (path, blob_hash, projected, start_byte, end_byte,
///   start_row, start_col, message)` names each stored file that did not
///   parse cleanly. `treeRoot` folds `_tree`. A v6 arena is refused at
///   parse open.
/// - `projection-v8` — the ADR-0026 pointer store leaves the projection
///   (ADR-0040 D5, bead `ley-line-open-f30fdf`). `capnp_blobs` (one
///   canonical `AstNodeList` per file) and `_ast_blob` (`file_id →
///   blob_hash`) are no longer created or written: they were a second stored
///   copy of the syntax tree, 35.6% of the kibana arena, with no reader. No
///   identity moves: `Head.rootHash` folds the segment FILES
///   (`source.capnp`, `ast.capnp`, `bindings.capnp`), whose bytes do not
///   depend on the blob, and `treeRoot` folds `_tree`. `parserId` changes,
///   as it does at every bump, because it folds this label. A v7 arena is
///   refused at parse open.
/// - `projection-v9` — the merkle-AST edges are interned (bead
///   `ley-line-open-9b7d28`). `node_content` gains `content_id INTEGER
///   PRIMARY KEY` (arena-local, not an identity) with `node_hash` kept as
///   `NOT NULL UNIQUE`, the FK target it always was. The edges move to a
///   new table `node_edge (parent_id, ordinal, child_id, field)`, `WITHOUT
///   ROWID`, keyed on content ids. `node_child` becomes a read-only VIEW with
///   the v8 columns (`parent_hash`, `ordinal`, `child_hash`, `field`), so
///   every v8 read runs unchanged; a write to it fails. A v8 arena is refused
///   at parse open.
/// - `projection-v10` — the occurrence tables point at their subtree by
///   `content_id` (bead `ley-line-open-738dd9`). `_ast.node_hash`,
///   `node_refs.node_hash` and `node_defs.node_hash` (32-byte BLOBs) become
///   `content_id INTEGER REFERENCES node_content(content_id)`, and
///   `idx_ast_node_hash` becomes `idx_ast_content` on `_ast(content_id)`.
///   `node_content.node_hash` stays the identity, and `_cfg` (keyed BY a
///   subtree, not pointing at one) keeps its hash key. The read-only views
///   `v_ast_hash`, `v_node_refs_hash` and `v_node_defs_hash` carry the v9
///   column sets, `node_hash` included. No identity moves: `node_hash`,
///   `Head.rootHash` and `treeRoot` are byte-identical. A v9 arena is
///   refused at parse open.
/// - `projection-v11` — the store identity is parser-free (ADR-0047 D-ii /
///   M1, bead `ley-line-open-73c6ee`). No table or column moves; the
///   meaning of a stamped value does: `_meta.derived_from` becomes the v2
///   value `v2 <storeRoot> <parserId> <projection_schema_version>`, whose
///   first component is the directory Merkle `storeRoot`
///   (`leyline/store-tree/v1`) where v1 carried the flat, parser-qualified
///   `treeRoot`. The head beside the arena moves to head digest v3
///   (`Head.storeRoot @10`; `treeRoot @7` is retired and refused), and the
///   daemon wire's `tree_root` becomes `store_root`. `node_hash` and
///   `Head.rootHash` are byte-identical; `parserId` changes, as at every
///   bump, because it folds this label. A v10 arena is refused at parse
///   open, and `leyline rebuild` refuses a v1 `derived_from`.
/// - `projection-v12` — `node_hash` is keyed and `node_content` is a function
///   of it (ADR-0047 M2: D-i and D-iii, beads `ley-line-open-73add7` and
///   `ley-line-open-b5d354`). `node_content` loses `raw_kind` and `lang`: they
///   are not in `node_hash`'s preimage, so the row kept whichever file's
///   occurrence landed first and a write left the replaced file's value
///   behind. Each occurrence keeps its own through `_ast.kind_id → kinds(lang,
///   raw_kind)`; an anonymous token has no occurrence row, and its grammar
///   kind is its `kind`, because the merkle-ast-v3 fold applies κ to named
///   nodes only (an anonymous token hashes its grammar kind). Every
///   `node_hash` moves to merkle-ast-v3 (`IR_SCHEMA_VERSION`), and with it
///   `parserId`; `storeRoot` and `Head.rootHash` do not move. `node_defs` and
///   `node_refs` gain `kind_id` (→ `kinds`), the occurrence's own `(lang,
///   raw_kind)`: an injected-language occurrence has no `_ast` row, and this
///   is where its language and grammar kind live (bead
///   `ley-line-open-cc5c55`). A v11 arena is refused at parse open.
///
/// Bump on ANY table added or removed, or column added or removed. Two
/// changes in flight at once need two numbers: v3 and v4 landed in the same
/// window, and both first claimed v3 — a clean textual merge, because both
/// wrote the same string, describing two different table shapes. A consumer
/// reading that would have had no way to tell which one it held.
pub const PROJECTION_SCHEMA_VERSION: &str = "projection-v12";

/// Current major of the JSON wire shape; its history is documented above,
/// beside the module's version facts. Pinned by
/// `wire_format_major_is_pinned`.
pub const WIRE_FORMAT_MAJOR: u32 = 2;

#[cfg(test)]
mod pin_tests {
    /// ADR-0047 M1 retired `tree_root` from two responses: a rename on the
    /// wire, so the major moved. Bumping or reverting it is a deliberate
    /// edit of this literal and of the history above.
    #[test]
    fn wire_format_major_is_pinned() {
        assert_eq!(super::WIRE_FORMAT_MAJOR, 2);
    }
}

/// Earliest schema-client version compatible with this daemon binary.
/// Consumers compare their embedded schema-client version against this
/// at handshake time and fail loudly if older.
///
/// Today: "0.6.0" — bumped from "0.4.1" at the v0.7.0 cut. Clients
/// below v0.6.0 don't know the source_blobs table, the capnp_blobs +
/// _ast_pointer pointer store, or the unified `daemon.sheaf.invalidate`
/// topic + `invalidated` payload key (was `region_ids` on watcher path
/// pre-0.7). Raise this floor when removing a field a client depends on.
pub const COMPAT_MIN_SCHEMA_VERSION: &str = "0.6.0";

/// ISO-8601 date of this daemon build. Populated from `$LLO_BUILD_DATE`
/// at compile time if present (CI sets it on release builds), else
/// `"unspecified"` for local dev binaries. Surfaces to consumers via
/// the `leyline_version` op so a support exchange can distinguish two
/// builds that report the same `BINARY_VERSION`.
pub const BUILD_DATE: &str = match option_env!("LLO_BUILD_DATE") {
    Some(d) => d,
    None => "unspecified",
};
