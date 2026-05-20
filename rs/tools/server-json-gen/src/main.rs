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

use anyhow::{Context, Result, bail};
use leyline_cli_lib::daemon::mcp;
use serde::Serialize;
use std::collections::HashMap;

/// MCP Registry schema URL pinned to the 2025-12-11 release. Bumps
/// when the registry schema itself ships a new dated version; update
/// the test fixtures and any registry-side consumers in lockstep.
const SCHEMA_URL: &str =
    "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json";

/// Registry-facing canonical name for this server. Matches the GitHub
/// `<owner>/<repo>` shape registries dispatch on.
const SERVER_NAME: &str = "io.github.agentic-research/ley-line-open";

/// One-sentence description shown in registry listings and link
/// previews. Kept synced with the README opener (the existing
/// handwritten `server.json` carried this exact text; the generator
/// preserves it byte-for-byte so the initial regen produces a clean
/// diff modulo the `_meta` block + version expansion).
const SERVER_DESCRIPTION: &str =
    "Open-source data plane primitives — tree-sitter parse, LSP, sheaf cache, observation lattice.";

/// Source-of-truth version. Equals `CARGO_PKG_VERSION` of
/// `leyline-cli-lib`, threaded through this binary via the same
/// cargo-env mechanism the daemon's `leyline_version` op uses.
/// `leyline-cli-lib`'s `package.version` is the workspace's single
/// authoritative version string today.
const VERSION: &str = leyline_cli_lib::daemon::version::BINARY_VERSION;

#[derive(Serialize)]
struct ServerDoc<'a> {
    #[serde(rename = "$schema")]
    schema: &'static str,
    name: &'static str,
    description: &'static str,
    version: &'static str,
    repository: Repository,
    packages: Vec<Package>,
    #[serde(rename = "_meta")]
    meta: Meta<'a>,
}

#[derive(Serialize)]
struct Repository {
    url: &'static str,
    source: &'static str,
}

#[derive(Serialize)]
struct Package {
    #[serde(rename = "registryType")]
    registry_type: &'static str,
    identifier: String,
    transport: Transport,
    #[serde(rename = "environmentVariables")]
    environment_variables: Vec<()>,
}

#[derive(Serialize)]
struct Transport {
    #[serde(rename = "type")]
    typ: &'static str,
    url: &'static str,
}

#[derive(Serialize)]
struct Meta<'a> {
    #[serde(rename = "art.cloister/v1")]
    art_cloister_v1: ArtCloisterV1<'a>,
}

#[derive(Serialize)]
struct ArtCloisterV1<'a> {
    groups: Vec<GroupOut<'a>>,
}

#[derive(Serialize)]
struct GroupOut<'a> {
    name: &'a str,
    #[serde(rename = "advertisedPrefix")]
    advertised_prefix: &'a str,
    #[serde(rename = "upstreamNames")]
    upstream_names: Vec<&'a str>,
}

fn build_doc() -> Result<ServerDoc<'static>> {
    let groups_decl = mcp::cloister_groups();
    let registry = mcp::tool_registry();

    // Coverage enforcement — every registered tool must be claimed by
    // exactly one group. Mirrors the unit test in
    // `daemon::mcp::tests::cloister_groups_cover_every_registered_tool_exactly_once`
    // so the generator can run standalone (and refuse to emit a stale
    // artifact) without depending on `cargo test` having run.
    let mut owner: HashMap<&str, Vec<&str>> = HashMap::new();
    for g in &groups_decl {
        if g.name.is_empty() {
            bail!("cloister group has empty `name` — spec violation");
        }
        if g.upstream_names.is_empty() {
            bail!(
                "cloister group `{}` has empty upstream_names — spec violation",
                g.name
            );
        }
        for tool in &g.upstream_names {
            owner.entry(tool).or_default().push(g.name);
        }
    }

    let registered: std::collections::HashSet<&str> = registry.iter().map(|t| t.name).collect();

    let mut orphans: Vec<&str> = registered
        .iter()
        .filter(|t| !owner.contains_key(*t))
        .copied()
        .collect();
    orphans.sort();
    if !orphans.is_empty() {
        bail!(
            "{} tool(s) in tool_registry() are not claimed by any cloister group: {:?}",
            orphans.len(),
            orphans,
        );
    }

    let mut over: Vec<(&str, Vec<&str>)> = owner
        .iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(t, names)| (*t, names.clone()))
        .collect();
    over.sort_by_key(|(t, _)| *t);
    if !over.is_empty() {
        bail!("tools claimed by multiple cloister groups: {:?}", over);
    }

    let mut ghosts: Vec<&str> = owner
        .keys()
        .filter(|t| !registered.contains(*t))
        .copied()
        .collect();
    ghosts.sort();
    if !ghosts.is_empty() {
        bail!(
            "{} cloister group claim(s) reference tools that are not in tool_registry(): {:?}",
            ghosts.len(),
            ghosts,
        );
    }

    // Convert the declaration into the wire-shape `GroupOut`. The
    // generator preserves both the array order and the inner
    // upstream_names order from `cloister_groups()` so regens diff
    // cleanly.
    let groups: Vec<GroupOut<'static>> = groups_decl
        .into_iter()
        .map(|g| GroupOut {
            name: g.name,
            advertised_prefix: g.advertised_prefix,
            upstream_names: g.upstream_names,
        })
        .collect();

    // Package identifier embeds the version — string-formatting once
    // at build time keeps the registry artifact in lockstep with the
    // workspace's source-of-truth version constant.
    let identifier = format!("ghcr.io/agentic-research/ley-line-open:{VERSION}");

    Ok(ServerDoc {
        schema: SCHEMA_URL,
        name: SERVER_NAME,
        description: SERVER_DESCRIPTION,
        version: VERSION,
        repository: Repository {
            url: "https://github.com/agentic-research/ley-line-open.git",
            source: "github",
        },
        packages: vec![Package {
            registry_type: "oci",
            identifier,
            transport: Transport {
                typ: "streamable-http",
                url: "http://localhost:8384/mcp",
            },
            environment_variables: vec![],
        }],
        meta: Meta {
            art_cloister_v1: ArtCloisterV1 { groups },
        },
    })
}

fn main() -> Result<()> {
    let doc = build_doc().context("build server.json document")?;
    // Pretty-print with a trailing newline — diffs against the
    // committed file should compare line-for-line. Same convention as
    // `compat-gen`.
    let mut s = serde_json::to_string_pretty(&doc)?;
    s.push('\n');
    print!("{s}");
    Ok(())
}
