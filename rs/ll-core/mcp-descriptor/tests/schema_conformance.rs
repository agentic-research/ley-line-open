//! The emitted descriptor conforms to the MCP registry schema this crate pins
//! (`ley-line-open-891dd5`).
//!
//! ## Why this gate exists
//!
//! `lib.rs` declares `SCHEMA_URL`, writes it into every descriptor's `$schema`
//! key, and nothing ever checked the document against it. The crate asserted
//! conformance to a spec it never read.
//!
//! That is not hypothetical. v0.11.2 shipped a package whose `identifier` was
//! tagless with no `version` sibling, and it took a human reading the artifact
//! to notice. (Worth stating precisely, because this bead was originally filed
//! on the wrong premise: `version` is NOT in the schema's `required` array, so
//! that descriptor was schema-VALID. What it broke was cloister ADR-0038's
//! derive rule — `image = "<identifier>:<version>"`. The emitter now enforces
//! that pairing itself; this file covers the other contract, the schema.)
//!
//! Nothing downstream reads `packages[]` yet — cloister ADR-0038 is Proposed
//! and a grep for `packages` across its `src/` finds no read — so a malformed
//! descriptor is invisible by construction until the first consumer adopts it.
//! A published artifact nobody validates and nobody reads is indistinguishable
//! from a correct one.
//!
//! ## Offline by construction
//!
//! The schema is vendored at `schema/mcp/`. A gate that fetches its own spec
//! fails open the moment the network does, which would make it exactly the
//! kind of mechanism that reports success without doing the work.
//!
//! ## What breaks this gate
//!
//! - An emitted descriptor that violates the pinned schema (a missing
//!   `transport`, a non-http transport `url`, a malformed transport type).
//! - `SCHEMA_URL` bumped to a release that has not been vendored, so the
//!   document claims conformance to a spec the gate cannot read.

use leyline_mcp_descriptor::{GroupRef, SCHEMA_URL, ServerMeta, ToolRef, render};

/// Vendored copy of the registry schema named by [`SCHEMA_URL`].
fn vendored_schema() -> (String, serde_json::Value) {
    let name = SCHEMA_URL
        .rsplit('/')
        .next()
        .expect("SCHEMA_URL has no final path segment");
    // `2025-12-11/server.schema.json` flattens to `server.schema.2025-12-11.json`
    // so the vendored release is visible in the filename.
    let release = SCHEMA_URL
        .trim_end_matches(name)
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .expect("SCHEMA_URL has no release segment");
    let stem = name.trim_end_matches(".json");
    let path = format!(
        "{}/../../../schema/mcp/{stem}.{release}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the vendored schema at {path}: {e}\n\n\
             SCHEMA_URL is {SCHEMA_URL}. If it was just bumped, vendor the new \
             release alongside it — a descriptor must not claim conformance to \
             a spec this gate cannot read."
        )
    });
    let doc = serde_json::from_str(&text).expect("vendored schema is not valid JSON");
    (path, doc)
}

/// The real LLO descriptor shape, minus the full tool list.
fn llo_meta() -> ServerMeta<'static> {
    ServerMeta {
        name: "io.github.agentic-research/ley-line-open",
        description: "Open-source data plane primitives.",
        version: "0.11.3",
        repository_url: "https://github.com/agentic-research/ley-line-open.git",
        repository_source: "github",
        oci_image: "ghcr.io/agentic-research/ley-line-open",
        oci_version: "v0.11.3",
        transport_type: "streamable-http",
        transport_url: "http://localhost:8384/mcp",
    }
}

fn validate(doc: &serde_json::Value) -> Vec<String> {
    let (path, schema) = vendored_schema();
    let compiled = jsonschema::validator_for(&schema)
        .unwrap_or_else(|e| panic!("vendored schema at {path} does not compile: {e}"));
    compiled.iter_errors(doc).map(|e| e.to_string()).collect()
}

#[test]
fn emitted_descriptor_conforms_to_the_pinned_schema() {
    let tools = [ToolRef { name: "query" }, ToolRef { name: "lsp_hover" }];
    let groups = [
        GroupRef {
            name: "query",
            advertised_prefix: "",
            upstream_names: vec!["query"],
        },
        GroupRef {
            name: "lsp",
            advertised_prefix: "lsp_",
            upstream_names: vec!["lsp_hover"],
        },
    ];

    let rendered = render(&llo_meta(), &tools, &groups).expect("render");
    let doc: serde_json::Value =
        serde_json::from_str(&rendered).expect("emitter produced invalid JSON");

    let errors = validate(&doc);
    assert!(
        errors.is_empty(),
        "the emitted descriptor violates the schema it pins ({SCHEMA_URL}):\n  {}\n\n{rendered}",
        errors.join("\n  "),
    );
}

#[test]
fn the_committed_server_json_conforms_to_the_pinned_schema() {
    // The artifact that actually ships. The test above validates a descriptor
    // this file constructs, which proves the EMITTER is sound but says nothing
    // about the committed document — and the committed document is what
    // consumers fetch by SHA. `gen:server-json:check` proves committed ==
    // emitted; this proves emitted is valid; neither alone covers the file.
    let path = format!("{}/../../../server.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read the committed descriptor at {path}: {e}"));
    let doc: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("the committed server.json is not valid JSON: {e}"));

    assert_eq!(
        doc["$schema"].as_str(),
        Some(SCHEMA_URL),
        "the committed server.json claims a different $schema than this crate \
         pins, so it is being validated against the wrong spec",
    );

    let errors = validate(&doc);
    assert!(
        errors.is_empty(),
        "the committed server.json violates the schema it declares:\n  {}",
        errors.join("\n  "),
    );
}

#[test]
fn the_declared_schema_url_is_the_one_actually_validated_against() {
    // Without this, bumping SCHEMA_URL would silently keep validating against
    // the stale vendored copy, and the `$schema` key would advertise
    // conformance nobody checked.
    let (path, schema) = vendored_schema();
    assert_eq!(
        schema["$id"].as_str(),
        Some(SCHEMA_URL),
        "the vendored schema at {path} declares a different $id than the \
         SCHEMA_URL this crate writes into every descriptor",
    );
}

#[test]
fn a_non_http_transport_url_is_rejected_by_the_schema() {
    // Proves the gate BITES rather than passing vacuously, and pins the
    // constraint that decides ley-line-open-6569de: the schema's transport
    // `url` pattern is `^https?://[^\s]+$`, so a Unix socket cannot be
    // advertised in server.json at all.
    let mut meta = llo_meta();
    meta.transport_url = "unix:///run/leyline/mcp.sock";

    let tools = [ToolRef { name: "query" }];
    let groups = [GroupRef {
        name: "query",
        advertised_prefix: "",
        upstream_names: vec!["query"],
    }];

    let rendered = render(&meta, &tools, &groups).expect("render");
    let doc: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");

    let errors = validate(&doc);
    assert!(
        !errors.is_empty(),
        "a `unix:` transport url must fail schema validation — if this passes, \
         the gate is not actually validating anything\n\n{rendered}",
    );
}
