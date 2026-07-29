//! The vendored MCP wire protocol schema is what it claims to be
//! (`ley-line-open-60f0d3`, step 1).
//!
//! ## Why this gate exists
//!
//! LLO hand-mirrors MCP protocol facts (method-name string literals like
//! `tools/call`, session/`initialize` references) in a handful of places,
//! and one went stale within days of SEP-2567 / SEP-2575 removing
//! sessions and `initialize` from the spec. Nothing here would have
//! caught that automatically — the fix is to pin the actual wire schema
//! so "we're on revision 2026-07-28" is checkable, not believed.
//!
//! This file mirrors the four properties `ley-line-open-891dd5` shipped
//! for the MCP *registry* schema (`schema/mcp/server.schema.2025-12-11.json`,
//! `rs/ll-core/mcp-descriptor/tests/schema_conformance.rs`):
//!
//! 1. Offline structural validation — the vendored file parses as JSON
//!    Schema and contains the JSON-RPC message definitions this repo
//!    actually depends on, while lacking any session/`initialize`
//!    definition (that absence is what makes this the 2026-07-28
//!    revision and not an earlier one).
//! 2. The file is asserted to be git-tracked, not just present on the
//!    author's disk (`.gitignore` line 3 is `*`, a deny-all allowlist;
//!    `schema/**` already has an allow rule from 891dd5, but nothing
//!    proved this specific file rides it).
//! 3. A digest gate: [`leyline_mcp_protocol_schema::PINNED_SHA256`] is
//!    re-derived from the vendored bytes on every run.
//! 4. A negative case proving the validator actually bites.
//!
//! ## What this crate is NOT
//!
//! It ships no generator and no emitter. Nothing here reads this schema
//! to produce protocol facts elsewhere in the repo — that is deliberately
//! out of scope for this step. See the bead for the inventory of
//! hand-encoded facts a follow-up generator would need to replace.
//!
//! ## Offline by construction
//!
//! The schema is vendored at `schema/mcp/`. A gate that fetches its own
//! spec fails open the moment the network does.

use leyline_mcp_protocol_schema::{
    PINNED_REVISION, PINNED_SHA256, SOURCE_URL, vendored_relative_path,
};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn vendored_path() -> PathBuf {
    PathBuf::from(format!(
        "{}/{}",
        env!("CARGO_MANIFEST_DIR"),
        vendored_relative_path()
    ))
}

fn vendored_bytes() -> Vec<u8> {
    let path = vendored_path();
    std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the vendored MCP wire schema at {}: {e}\n\n\
             PINNED_REVISION is {PINNED_REVISION} (source: {SOURCE_URL}). If it \
             was just bumped, vendor the new revision alongside it — nothing \
             may claim conformance to a spec this gate cannot read.",
            path.display()
        )
    })
}

fn vendored_doc() -> serde_json::Value {
    let bytes = vendored_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "vendored MCP wire schema at {} is not valid JSON: {e}",
            vendored_path().display()
        )
    })
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[test]
fn the_pinned_digest_matches_the_vendored_file() {
    // The digest, not the revision string, is the load-bearing half of
    // the pin (see module docs on this crate's `lib.rs`). Re-derive
    // rather than trust: if the vendored bytes ever drift from
    // PINNED_SHA256 — hand-edited, re-fetched from a moved `main`, or
    // the wrong revision copied in — this fails instead of silently
    // shipping a mismatch between the claimed and actual pin.
    let bytes = vendored_bytes();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = hex_lower(&hasher.finalize());
    assert_eq!(
        actual,
        PINNED_SHA256,
        "the vendored MCP wire schema at {} no longer hashes to \
         PINNED_SHA256. Either the file was edited in place (re-derive and \
         update the constant deliberately, with a comment explaining why) \
         or the wrong content was vendored under this filename.",
        vendored_path().display(),
    );
}

#[test]
fn the_vendored_schema_is_tracked_by_git() {
    // This is exactly the failure mode `ley-line-open-891dd5` shipped
    // once already: `.gitignore` denies by default, a local run reads a
    // file that exists only on disk, and CI on a clean checkout gets
    // "No such file or directory". A local test run cannot catch that by
    // construction — it always runs against a tree where the file is
    // present — so the git-tracked claim has to be asserted directly.
    let path = vendored_path();

    let out = match std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch"])
        .arg(&path)
        .output()
    {
        Ok(o) => o,
        // Not a git checkout (e.g. a packaged source tarball). The file
        // was readable — `vendored_bytes()` already read it — so it
        // shipped. Nothing to assert.
        Err(_) => return,
    };
    if !out.status.success() && out.status.code() == Some(128) {
        // git present but this is not a repository — same reasoning.
        return;
    }

    assert!(
        out.status.success(),
        "the vendored MCP wire schema at {} is NOT tracked by git, so it \
         will be absent on a clean checkout even though it is present \
         here. `.gitignore` denies by default (line 3 is `*`); confirm the \
         `!schema/**` allow rule from ley-line-open-891dd5 still covers \
         this path.",
        path.display(),
    );
}

#[test]
fn the_vendored_schema_compiles_as_json_schema() {
    let doc = vendored_doc();
    let compiled = jsonschema::validator_for(&doc).unwrap_or_else(|e| {
        panic!("vendored MCP wire schema does not compile as a JSON Schema: {e}")
    });
    // Compiling is necessary but not sufficient — confirm the validator
    // can actually run against an instance (the trivially-permissive
    // empty object), proving this is loadable machinery and not merely
    // parseable JSON.
    let _ = compiled.iter_errors(&serde_json::json!({})).count();
}

#[test]
fn the_vendored_schema_defines_the_jsonrpc_message_shapes() {
    let doc = vendored_doc();
    let defs = doc["$defs"]
        .as_object()
        .expect("vendored MCP wire schema has no top-level $defs object");

    for name in [
        "JSONRPCMessage",
        "JSONRPCRequest",
        "JSONRPCNotification",
        "JSONRPCResponse",
        "CallToolRequest",
        "CallToolResult",
        "DiscoverRequest",
        "DiscoverResult",
    ] {
        assert!(
            defs.contains_key(name),
            "vendored MCP wire schema is missing $defs/{name} — this is not \
             the JSON-RPC message schema ley-line-open-60f0d3 pinned",
        );
    }
}

#[test]
fn the_vendored_schema_has_no_session_or_initialize_definitions() {
    // SEP-2567 / SEP-2575's removal of sessions and `initialize` is the
    // change that made LLO's hand-copied facts stale in the first place,
    // and it is the defining structural difference of the 2026-07-28
    // revision. Vendoring an earlier revision by mistake (wrong date in
    // the URL, `main` having moved backward some other way) would
    // silently reintroduce exactly the staleness this pin exists to
    // prevent — so the absence is asserted, not just the presence of the
    // definitions above.
    let doc = vendored_doc();
    let defs = doc["$defs"]
        .as_object()
        .expect("vendored MCP wire schema has no top-level $defs object");

    let offending: Vec<&String> = defs
        .keys()
        .filter(|name| {
            let lower = name.to_lowercase();
            lower.contains("session") || lower.contains("initialize")
        })
        .collect();
    assert!(
        offending.is_empty(),
        "vendored MCP wire schema still defines session/initialize types: \
         {offending:?} — this looks like a pre-SEP-2567/2575 revision, not \
         2026-07-28",
    );

    let raw = serde_json::to_string(&doc).expect("re-serialize vendored doc for a text scan");
    let lower = raw.to_lowercase();
    assert!(
        !lower.contains("\"initialize\""),
        "vendored MCP wire schema contains an `initialize` string literal \
         (e.g. a lingering method const) even though no $defs entry is named \
         for it",
    );
}

#[test]
fn a_call_tool_request_with_the_wrong_method_const_is_rejected() {
    // Proves the gate BITES rather than passing vacuously. `tools/call`
    // is exactly the kind of hand-encoded method-name literal
    // ley-line-open-60f0d3's inventory is tracking elsewhere in this
    // repo — if a document claiming a DIFFERENT method name validated
    // clean against $defs/CallToolRequest, this schema would not be
    // constraining anything a generator could rely on.
    let doc = vendored_doc();
    let wrapper = serde_json::json!({
        "$schema": doc["$schema"],
        "$ref": "#/$defs/CallToolRequest",
        "$defs": doc["$defs"],
    });
    let compiled = jsonschema::validator_for(&wrapper).expect("wrapper schema compiles");

    let params = serde_json::json!({
        "_meta": {
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/protocolVersion": PINNED_REVISION,
        },
        "name": "example",
    });

    // Control: the real method name validates clean, so the negative
    // case below is isolated to the one field it claims to be.
    let good = serde_json::json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": params.clone(),
    });
    assert!(
        compiled.iter_errors(&good).next().is_none(),
        "a well-formed CallToolRequest using the real \"tools/call\" method \
         name must validate clean, or the negative case below proves nothing",
    );

    let bad = serde_json::json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": "tools/invoke",
        "params": params,
    });
    let errors: Vec<_> = compiled.iter_errors(&bad).collect();
    assert!(
        !errors.is_empty(),
        "a CallToolRequest with method \"tools/invoke\" must be rejected — \
         the schema pins the const \"tools/call\" — but the validator \
         reported no errors",
    );
}
