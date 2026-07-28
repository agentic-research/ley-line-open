//! `server-json-gen` — emit the LLO `server.json` from the daemon's
//! MCP tool registry. Self-maintaining MCP Registry surface per bead
//! `ley-line-open-f10abb`.
//!
//! This binary reads two things out of `leyline_cli_lib::daemon::mcp`:
//!
//! - `tool_registry()` — the canonical list of tools the daemon exposes
//!   on the MCP wire (also surfaced live by the `tools/list` op).
//! - `cloister_groups()` — the operator-facing partitioning of those
//!   tools into cloister-resolver backends, per the wire contract at
//!   `cloister/cloister-spec/mcp-tool/v1/wire/meta-groups.md`.
//!
//! It prints an MCP Registry `server.json` document (schema 2025-12-11)
//! to stdout, with the `_meta.art.cloister/v1.groups[]` block populated
//! from `cloister_groups()`. The CI invariant (Taskfile.yml
//! `gen:server-json:check`) regenerates and diffs against the committed
//! `server.json` at the repo root, failing the build on drift — the
//! same discipline `compat-gen` uses for `compatibility.json`.
//!
//! # Coverage policy
//!
//! Every tool in `tool_registry()` MUST appear in exactly one group's
//! `upstream_names`. The generator enforces this at runtime — partial
//! coverage exits non-zero with a message naming the orphan tool(s).
//! The matching unit test
//! (`mcp::tests::cloister_groups_cover_every_registered_tool_exactly_once`)
//! enforces the same invariant at `cargo test` time so the
//! generator-side check is a belt-and-braces backstop, not the only
//! gate.
//!
//! # Reproducibility
//!
//! Two consecutive runs MUST produce byte-identical output. Field order
//! within each JSON object is fixed by the serde struct definitions;
//! the inner `tools` array order matches `tool_registry()` order; the
//! `groups[]` array order matches `cloister_groups()` order. No
//! HashMap iteration — every container is a Vec.

use anyhow::Result;
use leyline_cli_lib::daemon::mcp;
use leyline_mcp_descriptor::{GroupRef, ServerMeta, ToolRef};

/// Registry-facing canonical name for this server. Matches the GitHub
/// `<owner>/<repo>` shape registries dispatch on.
const SERVER_NAME: &str = "io.github.agentic-research/ley-line-open";

/// One-sentence description shown in registry listings and link previews.
const SERVER_DESCRIPTION: &str =
    "Open-source data plane primitives — tree-sitter parse, LSP, sheaf cache, observation lattice.";

/// Source-of-truth version — `CARGO_PKG_VERSION` of `leyline-cli-lib`, the
/// workspace's single authoritative version string.
const VERSION: &str = leyline_cli_lib::daemon::version::BINARY_VERSION;

/// OCI path, **tagless**, per cloister ADR-0041 (bead `ley-line-open-04300f`).
///
/// This previously read `format!("ghcr.io/agentic-research/ley-line-open:{VERSION}")`
/// and so regenerated a fresh ADR-0041 violation on every release: a tag
/// promising an image LLO does not build and never pushes. `leyline-mcp-descriptor`
/// now rejects a tagged identifier outright, so the violation cannot come back
/// here or land in any repo that adopts the emitter.
const OCI_IMAGE: &str = "ghcr.io/agentic-research/ley-line-open";

fn main() -> Result<()> {
    // The emitter owns the coverage invariants — orphans, ghosts,
    // double-claims — so this binary supplies identity and the registry, and
    // cannot render a manifest that advertises something untrue.
    let groups: Vec<GroupRef<'_>> = mcp::cloister_groups()
        .into_iter()
        .map(|g| GroupRef {
            name: g.name,
            advertised_prefix: g.advertised_prefix,
            upstream_names: g.upstream_names,
        })
        .collect();
    let tools: Vec<ToolRef<'_>> = mcp::tool_registry()
        .iter()
        .map(|t| ToolRef { name: t.name })
        .collect();

    let meta = ServerMeta {
        name: SERVER_NAME,
        description: SERVER_DESCRIPTION,
        version: VERSION,
        repository_url: "https://github.com/agentic-research/ley-line-open.git",
        repository_source: "github",
        oci_image: OCI_IMAGE,
        transport_type: "streamable-http",
        transport_url: "http://localhost:8384/mcp",
    };

    print!(
        "{}",
        leyline_mcp_descriptor::render(&meta, &tools, &groups)?
    );
    Ok(())
}
