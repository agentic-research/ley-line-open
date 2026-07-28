//! `leyline-mcp-descriptor` — render a validated MCP `server.json`.
//!
//! # Why a binary and not only a library
//!
//! The library half is reachable from Rust alone, so mache (Go) keeps its own
//! copy of the same emitter and the coverage rules live in two places in two
//! languages. `ley-line-open-4ec276` already settled the shape for this
//! ecosystem: **one binary, invoked, not linked. JSON in, JSON out.** vigil,
//! rosary and Taskfile all integrate by shelling out; requiring FFI or a shared
//! runtime would be inventing a problem this ecosystem does not have.
//!
//! So: read a descriptor on stdin, write `server.json` on stdout, exit non-zero
//! with the reason on stderr. Go, TypeScript, Python, CI and a human all invoke
//! it identically.
//!
//! ```text
//! leyline-mcp-descriptor < descriptor.json > server.json
//! ```
//!
//! Input shape — `meta` mirrors [`ServerMeta`], `tools` is a list of names,
//! `groups` carries the cloister claims:
//!
//! ```json
//! {
//!   "meta": {
//!     "name": "io.github.org/thing",
//!     "description": "A thing.",
//!     "version": "1.2.3",
//!     "repository_url": "https://github.com/org/thing.git",
//!     "repository_source": "github",
//!     "oci_image": "ghcr.io/org/thing",
//!     "oci_version": "v1.2.3",
//!     "transport_type": "streamable-http",
//!     "transport_url": "http://localhost:8384/mcp"
//!   },
//!   "tools": ["a", "b"],
//!   "groups": [
//!     { "name": "g", "advertised_prefix": "g_", "upstream_names": ["a", "b"] }
//!   ]
//! }
//! ```
//!
//! # Known-unsettled: transport shape
//!
//! `meta.transport_type` / `transport_url` are SCALARS and carry no session
//! attribute, so a server with more than one transport cannot be described and
//! per-transport session semantics is unrepresentable. That is already wrong for
//! LLO, which serves both TCP and (since v0.11.2) a Unix socket.
//!
//! mache raised this before any consumer existed, and it is being decided on
//! `ley-line-open-4ec276`. **This input shape WILL change.** It is shipped
//! anyway because pre-1.0 with a real release story is exactly the setting where
//! you ship, learn, and break cleanly with a CHANGELOG note — and because an
//! integrator hitting the gap concretely is worth more than one reasoning about
//! it in the abstract.
//!
//! # Exit codes
//!
//! - `0` — rendered; `server.json` is on stdout
//! - `1` — the descriptor is invalid (unreadable JSON, or a coverage/ADR-0041
//!   violation). The reason is on stderr and **stdout is empty**, so a caller
//!   redirecting to a file never truncates a good artifact into a bad one.
//!
//! That last property is the point of writing only after `render` succeeds:
//! `tool > server.json` must not leave a half-written file when validation
//! fails, or a drift gate would compare against garbage.

use std::io::Read;

use anyhow::{Context, Result};
use leyline_mcp_descriptor::{GroupRef, ServerMeta, ToolRef, render};
use serde::Deserialize;

#[derive(Deserialize)]
struct Descriptor {
    meta: MetaIn,
    tools: Vec<String>,
    groups: Vec<GroupIn>,
}

#[derive(Deserialize)]
struct MetaIn {
    name: String,
    description: String,
    version: String,
    repository_url: String,
    repository_source: String,
    oci_image: String,
    oci_version: String,
    transport_type: String,
    transport_url: String,
}

#[derive(Deserialize)]
struct GroupIn {
    name: String,
    advertised_prefix: String,
    upstream_names: Vec<String>,
}

fn main() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .context("read descriptor from stdin")?;
    let d: Descriptor = serde_json::from_str(&raw).context("parse descriptor JSON")?;

    let meta = ServerMeta {
        name: &d.meta.name,
        description: &d.meta.description,
        version: &d.meta.version,
        repository_url: &d.meta.repository_url,
        repository_source: &d.meta.repository_source,
        oci_image: &d.meta.oci_image,
        oci_version: &d.meta.oci_version,
        transport_type: &d.meta.transport_type,
        transport_url: &d.meta.transport_url,
    };
    let tools: Vec<ToolRef<'_>> = d.tools.iter().map(|t| ToolRef { name: t }).collect();
    let groups: Vec<GroupRef<'_>> = d
        .groups
        .iter()
        .map(|g| GroupRef {
            name: &g.name,
            advertised_prefix: &g.advertised_prefix,
            upstream_names: g.upstream_names.iter().map(String::as_str).collect(),
        })
        .collect();

    // Render FIRST, print second. Nothing reaches stdout unless the descriptor
    // validated, so a caller redirecting into a file cannot end up with a
    // truncated artifact on failure.
    let out = render(&meta, &tools, &groups)?;
    print!("{out}");
    Ok(())
}
