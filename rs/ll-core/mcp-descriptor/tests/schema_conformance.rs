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

use leyline_mcp_descriptor::{
    ArtifactMeta, GroupRef, PackageMeta, SCHEMA_URL, ServerMeta, ToolRef, TransportMeta, render,
};

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
        version: "0.13.0",
        repository_url: "https://github.com/agentic-research/ley-line-open.git",
        repository_source: "github",
        packages: vec![PackageMeta {
            oci_image: "ghcr.io/agentic-research/ley-line-open",
            oci_version: "v0.13.0",
            transport: Some(TransportMeta {
                typ: "streamable-http",
                url: "http://localhost:8384/mcp",
            }),
        }],
        artifacts: vec![],
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
fn the_vendored_schema_is_tracked_by_git() {
    // This gate shipped once already validating against a file that existed
    // ONLY on the author's disk. `.gitignore` line 3 is `*` — a deny-all
    // allowlist — so `git add -A` silently skipped the new `schema/`
    // directory, every local run passed, and CI on a clean checkout failed
    // with "No such file or directory".
    //
    // A local `task ci` cannot catch that by construction: it runs against a
    // working tree where the file is present. So the claim "the spec is part
    // of the artifact" has to be asserted directly, or the next vendored file
    // repeats it.
    let (path, _) = vendored_schema();

    let out = match std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", &path])
        .output()
    {
        Ok(o) => o,
        // Not in a git checkout (e.g. a packaged source tarball). The file was
        // readable — `vendored_schema()` just read it — so it shipped. Nothing
        // to assert.
        Err(_) => return,
    };
    if !out.status.success() && out.status.code() == Some(128) {
        // git present but this is not a repository — same reasoning.
        return;
    }

    assert!(
        out.status.success(),
        "the vendored schema at {path} is NOT tracked by git, so it will be \
         absent on a clean checkout even though it is present here. This \
         repo's .gitignore denies by default — add an explicit `!` allow rule.",
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
    meta.packages[0].transport = Some(TransportMeta {
        typ: "streamable-http",
        url: "unix:///run/leyline/mcp.sock",
    });

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

// ── Multi-package and non-MCP producers (`ley-line-open-44cc45`) ────────────
//
// Fixtures are notme's REAL descriptor shape, not one invented here. notme
// publishes two OCI images from a single `v*` tag and serves no MCP at all —
// it is an identity authority, and its own `_meta` note says declaring a
// transport "would be worse than omitting it: cloister would generate backends
// for tools that do not exist."
//
// Using the real shape matters: an invented fixture would have quietly assumed
// LLO's conventions, and the point of this change is that other producers do
// not share them.

/// notme: two images, one version, no MCP transport, no cloister groups.
fn notme_meta() -> ServerMeta<'static> {
    ServerMeta {
        name: "io.github.agentic-research/notme",
        description: "Self-hostable identity authority.",
        version: "0.1.0-rc3",
        repository_url: "https://github.com/agentic-research/notme",
        repository_source: "github",
        packages: vec![
            PackageMeta {
                oci_image: "ghcr.io/agentic-research/notme",
                // NOT `v`-prefixed. notme pushes `:0.1.0-rc3` from a `v0.1.0-rc3`
                // tag; LLO pushes `v0.12.0`. Both satisfy ADR-0041, whose real
                // invariant is "matches the tag actually pushed" — this crate
                // must not impose either convention on the other.
                oci_version: "0.1.0-rc3",
                transport: None,
            },
            PackageMeta {
                oci_image: "ghcr.io/agentic-research/notme-proxy",
                oci_version: "0.1.0-rc3",
                transport: None,
            },
        ],
        artifacts: vec![],
    }
}

/// notme's ACTUAL shape once it adopts `artifacts` instead of `packages`
/// (`ley-line-open-0135fa`): two OCI images, no `packages[]` at all, both
/// addresses carried in the publisher-provided `_meta` extension. This is
/// the fixture the bead asks to mirror byte-for-byte — same two images, same
/// version, no packages.
fn notme_artifacts_meta() -> ServerMeta<'static> {
    ServerMeta {
        name: "io.github.agentic-research/notme",
        description: "Self-hostable identity authority.",
        version: "0.1.0-rc3",
        repository_url: "https://github.com/agentic-research/notme",
        repository_source: "github",
        packages: vec![],
        artifacts: vec![
            ArtifactMeta {
                oci_image: "ghcr.io/agentic-research/notme",
                oci_version: "0.1.0-rc3",
            },
            ArtifactMeta {
                oci_image: "ghcr.io/agentic-research/notme-proxy",
                oci_version: "0.1.0-rc3",
            },
        ],
    }
}

#[test]
fn a_transportless_package_is_refused_because_the_schema_requires_transport() {
    // notme's real shape: two images, no MCP. The MCP schema lists `transport`
    // in Package.required, so this CANNOT render to a schema-valid document —
    // and notme's own committed server.json, which omits it on both packages,
    // is invalid against the schema its `$schema` key declares.
    //
    // The emitter refuses rather than emitting a file that fails its own
    // declared spec. That is a real spec limitation surfaced loudly, not a
    // missing feature: the MCP schema has no way to describe a producer that
    // publishes images and serves no MCP.
    let err = render(&notme_meta(), &[], &[]).expect_err(
        "a transport-less package must be refused, not silently emitted as \
         schema-invalid JSON",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("packages[0]") && msg.contains("Package.required"),
        "the error must name the offending package AND why the schema forbids \
         it, or the reader cannot tell this is a spec conflict rather than a \
         bug here; got: {msg}",
    );
}

#[test]
fn two_packages_that_both_serve_mcp_render_and_validate() {
    // The multi-package half of ley-line-open-44cc45, which IS expressible:
    // repeated packages[] entries, each with its own transport. mache's case
    // (stdio + streamable-http) is this shape — the schema has `transport` as
    // a single object, so two transports are two PACKAGES.
    let mut meta = notme_meta();
    for (i, p) in meta.packages.iter_mut().enumerate() {
        p.transport = Some(TransportMeta {
            typ: "streamable-http",
            url: if i == 0 {
                "http://localhost:9000/mcp"
            } else {
                "http://localhost:9001/mcp"
            },
        });
    }
    let rendered = render(&meta, &[], &[]).expect("render");
    let doc: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");

    let pkgs = doc["packages"].as_array().expect("packages array");
    assert_eq!(pkgs.len(), 2, "both packages must render\n\n{rendered}");
    assert_eq!(pkgs[0]["identifier"], "ghcr.io/agentic-research/notme");
    assert_eq!(
        pkgs[1]["identifier"],
        "ghcr.io/agentic-research/notme-proxy"
    );
    assert_eq!(pkgs[0]["transport"]["url"], "http://localhost:9000/mcp");
    assert_eq!(pkgs[1]["transport"]["url"], "http://localhost:9001/mcp");

    // Not `v`-prefixed: notme pushes `:0.1.0-rc3`, LLO pushes `v0.12.0`. The
    // invariant is "matches the tag actually pushed", so this crate must not
    // impose either convention.
    assert_eq!(pkgs[0]["version"], "0.1.0-rc3");

    assert!(
        doc.get("_meta").is_none(),
        "with no groups there is no cloister surface; an empty \
         `art.cloister/v1` block would claim one\n\n{rendered}",
    );

    let errors = validate(&doc);
    assert!(
        errors.is_empty(),
        "a multi-package MCP producer must satisfy the pinned schema:\n  {}\n\n{rendered}",
        errors.join("\n  "),
    );
}

/// notme's shape, but with transports so the per-package checks below are
/// reached — otherwise the transport refusal fires first and masks them.
fn mcp_serving_two_package_meta() -> ServerMeta<'static> {
    let mut m = notme_meta();
    for p in m.packages.iter_mut() {
        p.transport = Some(TransportMeta {
            typ: "streamable-http",
            url: "http://localhost:9000/mcp",
        });
    }
    m
}

#[test]
fn every_package_is_validated_not_just_the_first() {
    // The failure that would matter in practice: a SECOND package smuggling a
    // tagged identifier past a check that only looked at packages[0]. That is
    // the gap that made check-image-versions.ts necessary downstream.
    let mut meta = mcp_serving_two_package_meta();
    meta.packages[1].oci_image = "ghcr.io/agentic-research/notme-proxy:0.1.0-rc3";
    let err = render(&meta, &[], &[]).expect_err("a tagged identifier must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("packages[1]"),
        "the error must name WHICH package is wrong, or a multi-package producer \
         cannot tell which line to fix; got: {msg}",
    );

    let mut meta = mcp_serving_two_package_meta();
    meta.packages[1].oci_version = "";
    let err = render(&meta, &[], &[]).expect_err("an empty version must be rejected");
    assert!(err.to_string().contains("packages[1]"), "got: {}", err);
}

#[test]
fn a_descriptor_with_no_packages_is_rejected() {
    // Nothing to derive `<identifier>:<version>` from — the v0.11.2 failure
    // with the address absent rather than malformed.
    let mut meta = notme_meta();
    meta.packages.clear();
    let err = render(&meta, &[], &[]).expect_err("no packages must be rejected");
    assert!(err.to_string().contains("no packages"), "got: {}", err);
}

// ── Artifact-only producers (`ley-line-open-0135fa`) ────────────────────────
//
// notme publishes OCI images and serves no MCP. `packages[]` requires
// `transport`, and a placeholder transport is a runtime lie downstream
// (cloister derives session behaviour from it). `artifacts` renders into the
// `_meta."io.modelcontextprotocol.registry/publisher-provided"` extension
// slot instead — a Package shape minus transport, which the schema declares
// `additionalProperties: true` specifically so producer-defined data like
// this fits without a schema change.

/// (a) An artifact-only descriptor renders and validates — positive.
#[test]
fn artifact_only_descriptor_renders_and_validates() {
    let rendered = render(&notme_artifacts_meta(), &[], &[]).expect("render");
    let doc: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");

    let errors = validate(&doc);
    assert!(
        errors.is_empty(),
        "an artifact-only descriptor must satisfy the pinned schema:\n  {}\n\n{rendered}",
        errors.join("\n  "),
    );
}

/// (d) The artifact-only output has NO `packages` key — absence, not
/// emptiness. `packages[]` is optional in the schema's top-level `required`
/// (name, description, version only), so an absent key and an empty array
/// are both schema-valid; the contract is specifically absence, because an
/// empty `packages: []` would still read as "this producer declares an
/// (empty) MCP packages list" rather than "this producer has none".
#[test]
fn artifact_only_output_has_no_packages_key() {
    let rendered = render(&notme_artifacts_meta(), &[], &[]).expect("render");
    let doc: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");

    assert!(
        doc.get("packages").is_none(),
        "an artifact-only descriptor must omit `packages` entirely, not emit \
         an empty array\n\n{rendered}",
    );

    let artifacts =
        &doc["_meta"]["io.modelcontextprotocol.registry/publisher-provided"]["artifacts"];
    let arr = artifacts.as_array().expect("artifacts array");
    assert_eq!(arr.len(), 2, "both notme images must render\n\n{rendered}");
    assert_eq!(arr[0]["registryType"], "oci");
    assert_eq!(arr[0]["identifier"], "ghcr.io/agentic-research/notme");
    assert_eq!(arr[0]["version"], "0.1.0-rc3");
    assert_eq!(arr[1]["identifier"], "ghcr.io/agentic-research/notme-proxy");
    // Package shape minus transport: an artifact entry never carries one.
    assert!(arr[0].get("transport").is_none());
    assert!(arr[1].get("transport").is_none());
}

/// (b) A descriptor with BOTH packages and artifacts renders both and
/// validates. mache's shape once it ships a non-MCP-serving sidecar image
/// alongside its MCP-serving binary would look like this.
#[test]
fn descriptor_with_both_packages_and_artifacts_renders_both_and_validates() {
    let mut meta = notme_artifacts_meta();
    // Give it one MCP-serving package alongside the two bare artifacts, so
    // both lists are populated in the same descriptor.
    meta.packages.push(PackageMeta {
        oci_image: "ghcr.io/agentic-research/notme-mcp-sidecar",
        oci_version: "0.1.0-rc3",
        transport: Some(TransportMeta {
            typ: "streamable-http",
            url: "http://localhost:9100/mcp",
        }),
    });

    let tools = [ToolRef { name: "whoami" }];
    let groups = [GroupRef {
        name: "identity",
        advertised_prefix: "",
        upstream_names: vec!["whoami"],
    }];

    let rendered = render(&meta, &tools, &groups).expect("render");
    let doc: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");

    let pkgs = doc["packages"].as_array().expect("packages array present");
    assert_eq!(
        pkgs.len(),
        1,
        "the one MCP-serving package renders\n\n{rendered}"
    );
    assert_eq!(
        pkgs[0]["identifier"],
        "ghcr.io/agentic-research/notme-mcp-sidecar"
    );

    let artifacts =
        doc["_meta"]["io.modelcontextprotocol.registry/publisher-provided"]["artifacts"]
            .as_array()
            .expect("artifacts array present");
    assert_eq!(
        artifacts.len(),
        2,
        "both bare artifacts render\n\n{rendered}"
    );

    // Both `_meta` occupants coexist — the cloister group AND the
    // publisher-provided artifacts — proving one does not clobber the other.
    assert!(doc["_meta"]["art.cloister/v1"]["groups"].is_array());

    let errors = validate(&doc);
    assert!(
        errors.is_empty(),
        "a descriptor mixing packages and artifacts must satisfy the pinned schema:\n  {}\n\n{rendered}",
        errors.join("\n  "),
    );
}

/// (c) The existing failure mode stays a failure: a transport-less
/// `packages[]` entry is still refused even now that `artifacts` exists as
/// an alternative. A producer that wants to publish an image with no
/// transport must use `artifacts`, not lean on a softened `packages` check.
#[test]
fn transportless_package_still_refuses_even_with_artifacts_available() {
    let mut meta = notme_artifacts_meta();
    meta.packages.push(PackageMeta {
        oci_image: "ghcr.io/agentic-research/notme-mcp-sidecar",
        oci_version: "0.1.0-rc3",
        transport: None,
    });

    let err = render(&meta, &[], &[])
        .expect_err("a transport-less package must still be refused, artifacts notwithstanding");
    let msg = err.to_string();
    assert!(
        msg.contains("packages[0]") && msg.contains("Package.required"),
        "got: {msg}",
    );
}

/// An artifact carrying a tagged identifier is rejected the same way a
/// package's is — the ADR-0041 rule applies to both lists.
#[test]
fn a_tagged_artifact_identifier_is_rejected() {
    let mut meta = notme_artifacts_meta();
    meta.artifacts[0].oci_image = "ghcr.io/agentic-research/notme:0.1.0-rc3";
    let err = render(&meta, &[], &[]).expect_err("a tagged artifact identifier must be rejected");
    assert!(err.to_string().contains("artifacts[0]"), "got: {err}");
}

/// An artifact with an empty version is rejected the same way a package's
/// is.
#[test]
fn an_empty_artifact_version_is_rejected() {
    let mut meta = notme_artifacts_meta();
    meta.artifacts[1].oci_version = "";
    let err = render(&meta, &[], &[]).expect_err("an empty artifact version must be rejected");
    assert!(err.to_string().contains("artifacts[1]"), "got: {err}");
}

/// A descriptor with neither packages nor artifacts is still rejected —
/// `artifacts` widens what counts as "something to derive from", it does
/// not remove the requirement that there be at least one.
#[test]
fn a_descriptor_with_neither_packages_nor_artifacts_is_rejected() {
    // notme_artifacts_meta() already declares `packages: vec![]`; clearing
    // `artifacts` too leaves nothing to derive `<identifier>:<version>` from.
    let mut meta = notme_artifacts_meta();
    meta.artifacts.clear();
    let err = render(&meta, &[], &[])
        .expect_err("neither packages nor artifacts declared must be rejected");
    assert!(
        err.to_string().contains("no packages or artifacts"),
        "got: {err}"
    );
}
