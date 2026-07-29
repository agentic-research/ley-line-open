//! MCP wire-protocol facts, GENERATED from a digest-pinned schema —
//! the JSON-RPC method names (`tools/call`, `server/discover`,
//! notifications) that this repo's daemon and tests previously
//! hand-encoded as string literals.
//!
//! # Why generated, not recorded
//!
//! LLO hand-mirrors MCP protocol facts in several places, and one went
//! stale within days: SEP-2567 / SEP-2575 removed sessions and
//! `initialize` from the spec, and nothing here noticed. Recording the
//! corrected fact anywhere by hand — a constant, a comment, a bead —
//! is the same failure with a delay: a plausible value with nothing
//! checking it against the source. The only fact in this crate that is
//! typed by a human is the sha256 in `build.rs`, and a digest verifies
//! its subject instead of trusting it.
//!
//! # How the pin is enforced
//!
//! `build.rs` reads the vendored `schema/mcp/protocol.<REV>.json`,
//! fails COMPILATION on a digest mismatch, and derives every exported
//! constant from the verified bytes: the `METHOD_*` constants and
//! [`METHODS`] are the schema definitions' `properties.method.const`
//! values, enumerated rather than curated. A schema edit re-derives
//! them or fails the gate; a revision bump is a deliberate two-field
//! change (vendored file + expected digest) that regenerates the whole
//! surface. `tests/protocol_schema_pin.rs` independently re-derives
//! the digest and cross-checks the generated set against its own read
//! of the JSON, so the generator itself cannot silently drift either.
//!
//! # Consumers
//!
//! Import the constants instead of writing method-name literals —
//! `cli-lib`'s daemon handshake is the first consumer (bead
//! `ley-line-open-1227f2`). [`PINNED_REVISION`] is also MCP's
//! `protocolVersion` wire value for this revision.

include!(concat!(env!("OUT_DIR"), "/protocol_facts.rs"));

/// Path to the vendored schema, relative to this crate's manifest dir.
pub fn vendored_relative_path() -> String {
    format!("../../../schema/mcp/protocol.{PINNED_REVISION}.json")
}
