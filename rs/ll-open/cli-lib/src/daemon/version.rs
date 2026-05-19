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
//! - `SCHEMA_VERSION` — today equals `BINARY_VERSION` (we release
//!   them in lockstep). Reserved as a separate constant so the two
//!   can diverge later without a wire shape change.
//!
//! These are the *only* hand-maintained version facts; the rest of
//! the substrate's compatibility surface (cbea02) derives from them.

/// The daemon binary's version. Derived from `CARGO_PKG_VERSION` at
/// compile time — no separate source of truth to drift against.
pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Go schema-client version this daemon's wire shapes target.
/// Today equals `BINARY_VERSION` since the two release in lockstep.
/// Reserved as separate so they can diverge without a wire change.
pub const SCHEMA_VERSION: &str = env!("CARGO_PKG_VERSION");

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
pub const WIRE_FORMAT_MAJOR: u32 = 1;

/// Earliest schema-client version compatible with this daemon binary.
/// Consumers compare their embedded schema-client version against this
/// at handshake time and fail loudly if older.
///
/// Today: "0.4.1" — the schema-client release that introduced the δ⁰
/// sheaf input shape (`SheafStalkInput.data`). Older clients can still
/// drive non-sheaf ops but can't send δ⁰-mode topology; we treat
/// "missing field" as still-compatible-but-degraded and let the
/// handler decide. Raise this floor when removing a field a client
/// depends on.
pub const COMPAT_MIN_SCHEMA_VERSION: &str = "0.4.1";

/// ISO-8601 date of this daemon build. Populated from `$LLO_BUILD_DATE`
/// at compile time if present (CI sets it on release builds), else
/// `"unspecified"` for local dev binaries. Surfaces to consumers via
/// the `leyline_version` op so a support exchange can distinguish two
/// builds that report the same `BINARY_VERSION`.
pub const BUILD_DATE: &str = match option_env!("LLO_BUILD_DATE") {
    Some(d) => d,
    None => "unspecified",
};
