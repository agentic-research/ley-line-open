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
//! `BINARY_VERSION` comes from `CARGO_PKG_VERSION` — bumps automatically
//! every release. `BUILD_DATE` comes from `$LLO_BUILD_DATE` if set in
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

/// The daemon binary's version. Derived from `CARGO_PKG_VERSION` at
/// compile time — no separate source of truth to drift against.
pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The public Rust/Go schema-client version this daemon's wire shapes target.
/// This is intentionally independent from `BINARY_VERSION`, even though the
/// v0.11.0 release advances both. Binary-only releases leave this literal
/// unchanged; a public consumer-contract release requires a matching nested
/// Go-module tag.
pub const SCHEMA_VERSION: &str = "0.11.2";

/// Current major version of the JSON wire envelope shape. Bumps on
/// incompatible changes (renames, removals, type changes); additions
/// are non-breaking inside the same major.
///
/// **History:**
/// - 1: current. Includes the v0.4.3 wire shape — `data: {...}`
///   nested under `Event`, `generation`/`prior_generation` as quoted
///   strings per capnp_json u64 convention.
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
/// Bump lives in `cmd_parse::IR_SCHEMA_VERSION`, which re-exports this.
pub const IR_SCHEMA_VERSION: &str = "merkle-ast-v2";

pub const WIRE_FORMAT_MAJOR: u32 = 1;

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
