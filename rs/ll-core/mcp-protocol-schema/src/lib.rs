//! Pins the MCP wire protocol schema — the JSON-RPC message shapes
//! (`tools/call`, `server/discover`, notifications, results, errors) — that
//! this repo's hand-encoded protocol facts (method-name string literals,
//! session/`initialize` references) are supposed to agree with.
//!
//! # Why this crate exists
//!
//! LLO hand-mirrors MCP protocol facts in several places, and one went
//! stale within days: SEP-2567 / SEP-2575 removed sessions and
//! `initialize` from the spec, and nothing here noticed. The durable fix
//! is generating those facts from a pinned spec instead of hand-copying
//! them.
//!
//! This crate is step 1 of that fix, and step 1 ONLY: pin the wire schema
//! so "our pinned revision is 2026-07-28" is a checkable fact rather than
//! a belief. It ships no generator and no emitter — see bead
//! `ley-line-open-60f0d3` for the inventory of hand-encoded facts a
//! follow-up generator would need to replace.
//!
//! # Why the digest, not just the revision string
//!
//! The upstream repo publishes each revision under its own path
//! (`schema/<REVISION>/schema.json`), so the revision directory is stable
//! by convention — but `main` itself is a mutable ref, and a revision
//! directory could theoretically be edited in place upstream (force-push,
//! history rewrite, or a correction commit) without the pinned string
//! changing. [`PINNED_SHA256`] is the load-bearing half of the pin: it is
//! re-derived from the vendored bytes on every test run
//! (`tests/protocol_schema_pin.rs`), so drift between "what we pinned" and
//! "what is on disk" fails loudly instead of silently.

/// The MCP wire protocol revision this repo is pinned to. Each revision is
/// published under its own immutable-by-convention directory upstream.
pub const PINNED_REVISION: &str = "2026-07-28";

/// Canonical upstream source for [`PINNED_REVISION`]. Fetched once at
/// vendoring time; the gate never re-fetches this at runtime — see the
/// module docs for why the digest, not this URL, is what's actually
/// checked.
pub const SOURCE_URL: &str = "https://raw.githubusercontent.com/modelcontextprotocol/modelcontextprotocol/main/schema/2026-07-28/schema.json";

/// sha256 of `schema/mcp/protocol.2026-07-28.json` as vendored, re-derived
/// (not trusted) at the time this crate was written. See
/// `tests/protocol_schema_pin.rs::the_pinned_digest_matches_the_vendored_file`.
pub const PINNED_SHA256: &str = "ef70b61f99b6d2e5e3b46863822eab08dff6a45bedc7a08914e0e5b133f40203";

/// Path to the vendored schema, relative to this crate's manifest dir.
pub fn vendored_relative_path() -> String {
    format!("../../../schema/mcp/protocol.{PINNED_REVISION}.json")
}
