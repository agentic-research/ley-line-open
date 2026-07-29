//! Build-time gate + generator for the pinned MCP wire schema (bead
//! `ley-line-open-60f0d3`, step 2).
//!
//! Step 1 vendored the schema and checked its digest in `tests/` — which
//! meant a consumer's `cargo build` never ran the gate, so the pin was
//! not load-bearing at build time. Cloister's review named the deeper
//! problem: any hand-typed protocol fact (a method-name literal, a
//! version string, a digest in a comment) is a plausible value with
//! nothing checking it against the source — the same shape as the
//! v0.7.9 generator that went five minor versions stale while exiting 0.
//!
//! So this script is now the single authority:
//!
//! 1. It reads the vendored bytes and FAILS COMPILATION unless their
//!    sha256 equals [`EXPECTED_SHA256`]. A tampered, truncated, or
//!    swapped schema cannot produce a compiling crate.
//! 2. Every protocol fact the crate exports is GENERATED from those
//!    verified bytes: the method-name constants are the
//!    `properties.method.const` values of the schema's definitions —
//!    enumerated, not curated. A fact that is not derivable from the
//!    schema is not emitted.
//! 3. It refuses to generate a fact for the deleted handshake: if any
//!    definition carries `method: "initialize"` the build fails,
//!    because that would mean the vendored bytes are not the revision
//!    the pin claims (SEP-2567/SEP-2575 removed `initialize`; that
//!    absence is the 2026-07-28 revision's defining change).
//!
//! The one hand-typed value left anywhere is [`EXPECTED_SHA256`] itself
//! — the root of trust has to live somewhere, and a digest is the only
//! kind of fact that verifies rather than trusts its subject.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

/// The pinned revision. Named by the vendored file (which these bytes
/// are verified against), not by anything inside the bytes: the wire
/// schema carries no `$id`, and in MCP the revision string IS the
/// protocol-version value peers exchange.
const REVISION: &str = "2026-07-28";

/// Canonical upstream source, recorded for provenance. Never fetched
/// here — the digest below is what is actually checked.
const SOURCE_URL: &str = "https://raw.githubusercontent.com/modelcontextprotocol/modelcontextprotocol/main/schema/2026-07-28/schema.json";

/// sha256 of the vendored schema bytes. The root of trust: re-derived
/// from the fetched bytes at vendoring time (matches the value
/// independently derived on bead `ley-line-open-60f0d3`), enforced on
/// every build below.
const EXPECTED_SHA256: &str = "ef70b61f99b6d2e5e3b46863822eab08dff6a45bedc7a08914e0e5b133f40203";

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let schema_path =
        Path::new(&manifest_dir).join(format!("../../../schema/mcp/protocol.{REVISION}.json"));
    // Re-run when the vendored bytes change — the whole point is that a
    // schema edit re-derives every fact (or fails the digest gate).
    println!("cargo::rerun-if-changed={}", schema_path.display());

    let bytes = fs::read(&schema_path).unwrap_or_else(|e| {
        panic!(
            "cannot read the vendored MCP wire schema at {}: {e}. \
             The pin is the file + its digest; a missing file fails the build.",
            schema_path.display()
        )
    });

    let derived = hex(&Sha256::digest(&bytes));
    assert_eq!(
        derived,
        EXPECTED_SHA256,
        "vendored schema digest mismatch for {}: the bytes on disk are not \
         the pinned 2026-07-28 revision. Refusing to generate protocol \
         facts from unverified bytes. If the pin is being bumped \
         deliberately, update EXPECTED_SHA256 in build.rs together with \
         the vendored file (and its filename revision).",
        schema_path.display()
    );

    let schema: serde_json::Value =
        serde_json::from_slice(&bytes).expect("digest-verified schema must parse as JSON");
    let definitions = schema
        .get("$defs")
        .and_then(|d| d.as_object())
        .expect("the wire schema is a 2020-12 document with a $defs map");

    // Every `properties.method.const` in the schema, deduped by value
    // (union types like ClientNotification repeat their members'
    // methods). Enumerated, not curated: the emitted set IS the
    // schema's set.
    let mut methods: Vec<String> = definitions
        .values()
        .filter_map(|d| {
            d.get("properties")?
                .get("method")?
                .get("const")?
                .as_str()
                .map(str::to_string)
        })
        .collect();
    methods.sort();
    methods.dedup();
    assert!(
        !methods.is_empty(),
        "no method constants found — the verified bytes do not look like \
         the MCP wire schema; refusing to emit an empty fact set"
    );
    assert!(
        !methods.iter().any(|m| m == "initialize"),
        "the schema defines an `initialize` method, which SEP-2567/2575 \
         removed — these bytes cannot be the 2026-07-28 revision"
    );

    let mut out = String::new();
    out.push_str(
        "// GENERATED by build.rs from the digest-verified vendored schema.\n\
         // Do not edit; do not add hand-typed protocol facts next to these.\n\n",
    );
    let _ = writeln!(
        out,
        "/// The pinned MCP wire protocol revision — in MCP this string is\n\
         /// also the `protocolVersion` value peers exchange.\n\
         pub const PINNED_REVISION: &str = \"{REVISION}\";\n\n\
         /// Canonical upstream source for the pin (provenance only; the\n\
         /// digest is what is checked).\n\
         pub const SOURCE_URL: &str = \"{SOURCE_URL}\";\n\n\
         /// sha256 the vendored bytes were verified against when this\n\
         /// crate compiled. A mismatch fails the BUILD, not a test.\n\
         pub const PINNED_SHA256: &str = \"{EXPECTED_SHA256}\";\n"
    );
    for m in &methods {
        let _ = writeln!(
            out,
            "/// `{m}` — generated from the schema definition's `method` const.\n\
             pub const {}: &str = \"{m}\";",
            method_const_name(m)
        );
    }
    let _ = writeln!(
        out,
        "\n/// Every method the pinned revision defines, sorted. Enumerated\n\
         /// from the schema, not curated — a method absent here is absent\n\
         /// from the revision.\n\
         pub const METHODS: &[&str] = &["
    );
    for m in &methods {
        let _ = writeln!(out, "    \"{m}\",");
    }
    out.push_str("];\n");

    let out_path =
        Path::new(&env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("protocol_facts.rs");
    fs::write(&out_path, out).expect("write generated protocol facts");
}

/// `tools/call` → `METHOD_TOOLS_CALL`, `sampling/createMessage` →
/// `METHOD_SAMPLING_CREATE_MESSAGE`: path segments join with `_`,
/// camelCase splits on the case boundary.
fn method_const_name(method: &str) -> String {
    let mut name = String::from("METHOD_");
    for ch in method.chars() {
        match ch {
            '/' | '-' | '_' => name.push('_'),
            c if c.is_ascii_uppercase() => {
                name.push('_');
                name.push(c);
            }
            c => name.push(c.to_ascii_uppercase()),
        }
    }
    name
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
