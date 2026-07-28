//! MCP Registry `server.json` emitter, shared across ART producers.
//!
//! # Why this is a crate and not a per-repo script
//!
//! Four repos publish an MCP `server.json` and each solved it differently
//! (bead `ley-line-open-4ec276`, comment on the `#404` scope addition):
//!
//! | repo | approach |
//! | --- | --- |
//! | ley-line-open | generates from the in-code tool registry, drift-gated |
//! | mache | generates from `internal/mcpregistry`, drift-gated (Go) |
//! | rosary | hand-maintained manifest + a `server-json:check` assertion |
//! | canonical-hours | a script that string-patches the version in place |
//!
//! Three maturity levels of one rule. Per `vigil-4b304d`: *regenerable
//! deterministically → GENERATE*, because then drift is unrepresentable rather
//! than merely detected.
//!
//! # What is actually worth sharing
//!
//! Not the structs — those are twenty lines anyone can write. **The coverage
//! invariants.** A manifest that advertises a tool the server never registers
//! does not fail at build; it fails at the consumer, when a client calls a tool
//! that is not there. That is the same shape cloister ADR-0041 describes for
//! images: *"a tool that declares `ghcr.io/org/mache:0.13.0` but never pushes it
//! produces a manifest that fails at compose up, not at resolve."*
//!
//! So [`render`] validates and renders in one call. There is deliberately no
//! public `validate()`: a check you can forget to call is a check that will be
//! forgotten, which is the failure this crate exists to remove.

use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// MCP Registry schema release this crate emits against.
///
/// Genuinely shared — it pins the registry's own schema version, which is the
/// same for every producer. Bump when the registry ships a new dated release,
/// and update consumers' fixtures in lockstep.
pub const SCHEMA_URL: &str =
    "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json";

/// Per-producer identity. Everything here differs per repo; everything not here
/// is either shared ([`SCHEMA_URL`]) or derived from the tool registry.
#[derive(Debug, Clone)]
pub struct ServerMeta<'a> {
    /// Registry-facing canonical name, `<owner>/<repo>` shaped —
    /// e.g. `io.github.agentic-research/ley-line-open`.
    pub name: &'a str,
    /// One sentence, shown in registry listings and link previews.
    pub description: &'a str,
    /// The producer's own version. Pass a single source-of-truth constant
    /// (e.g. `CARGO_PKG_VERSION`), never a second literal — a version that can
    /// disagree with the binary is the drift this crate exists to prevent.
    pub version: &'a str,
    pub repository_url: &'a str,
    /// Forge identifier, e.g. `"github"`.
    pub repository_source: &'a str,
    /// The OCI packages this producer publishes. **At least one.**
    ///
    /// A list, not a single image, because real producers ship more than one:
    /// notme publishes `notme` and `notme-proxy` from one `v*` tag
    /// (`ley-line-open-44cc45`). The MCP schema models this as repeated
    /// `packages[]` entries — `transport` on a package is a single object, not
    /// an array, so two transports are also two packages rather than one
    /// package with a transport list.
    pub packages: Vec<PackageMeta<'a>>,
}

/// One published artifact: its address, its tag, and how (or whether) it
/// speaks MCP.
#[derive(Debug, Clone)]
pub struct PackageMeta<'a> {
    /// OCI image path, **without tag or digest**.
    ///
    /// cloister ADR-0041: *"`identifier` is the registry path with no tag and
    /// no digest"*. Named `oci_image` rather than `identifier` so the
    /// constraint is legible at the call site, and [`render`] rejects a value
    /// carrying `:` or `@` — see `ley-line-open-04300f`, where LLO emitted a
    /// tagged identifier for an image it never published while mache complied
    /// and asserted the rule in a test.
    pub oci_image: &'a str,
    /// The image tag, carried as a SIBLING of [`Self::oci_image`] rather than
    /// baked into it.
    ///
    /// This is the half that makes tagless correct. cloister ADR-0038's derive
    /// rule builds the image as `<identifier>:<version>`, so an identifier that
    /// already carried a tag would yield `repo:1.2.3:1.2.3` — which is why
    /// ADR-0041 requires the address alone. But removing the tag WITHOUT this
    /// field leaves an address that derives to nothing, strictly worse than the
    /// violation it replaced. `ley-line-open-04300f` shipped exactly that in
    /// v0.11.2.
    ///
    /// The invariant is that this MUST equal the tag the publish job actually
    /// pushes — NOT that it carries any particular prefix. LLO pushes
    /// `v0.12.0` and emits `v0.12.0`; notme pushes `0.1.0-rc3` and emits
    /// `0.1.0-rc3`. Both are correct, and this crate must not impose either
    /// convention on the other.
    pub oci_version: &'a str,
    /// How this package serves MCP, or `None` when it does not serve MCP.
    ///
    /// `None` is a first-class case, not a degenerate one. notme is an identity
    /// authority with no MCP tools; its own descriptor says declaring a
    /// transport there *"would be worse than omitting it: cloister would
    /// generate backends for tools that do not exist."* A producer that ships
    /// images and no MCP server is ordinary, and the emitter must be able to
    /// say so.
    pub transport: Option<TransportMeta<'a>>,
}

/// How a package serves MCP.
#[derive(Debug, Clone, Copy)]
pub struct TransportMeta<'a> {
    /// e.g. `"streamable-http"`.
    pub typ: &'a str,
    pub url: &'a str,
}

/// A tool the server registers. Only the name participates in coverage.
#[derive(Debug, Clone, Copy)]
pub struct ToolRef<'a> {
    pub name: &'a str,
}

/// A cloister group claim over a set of registered tools.
#[derive(Debug, Clone)]
pub struct GroupRef<'a> {
    pub name: &'a str,
    pub advertised_prefix: &'a str,
    pub upstream_names: Vec<&'a str>,
}

// ── wire shapes ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ServerDoc<'a> {
    #[serde(rename = "$schema")]
    schema: &'a str,
    name: &'a str,
    description: &'a str,
    version: &'a str,
    repository: Repository<'a>,
    packages: Vec<Package<'a>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<Meta<'a>>,
}

#[derive(Serialize)]
struct Repository<'a> {
    url: &'a str,
    source: &'a str,
}

#[derive(Serialize)]
struct Package<'a> {
    #[serde(rename = "registryType")]
    registry_type: &'a str,
    identifier: &'a str,
    version: &'a str,
    // Omitted entirely when the package serves no MCP. An empty or
    // placeholder transport would be worse than absence: cloister derives
    // session behaviour from `packages[].transport.type`, and a present-but-
    // meaningless one makes it generate backends for tools that do not exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<Transport<'a>>,
    #[serde(rename = "environmentVariables")]
    environment_variables: Vec<()>,
}

#[derive(Serialize)]
struct Transport<'a> {
    #[serde(rename = "type")]
    typ: &'a str,
    url: &'a str,
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

// ── the one entry point ──────────────────────────────────────────────────────

/// Validate coverage, then render `server.json`.
///
/// Returns the pretty-printed document with a trailing newline, so a committed
/// copy diffs line-for-line against a regeneration — which is what makes a
/// drift gate readable.
///
/// # Errors
///
/// Every one of these is a manifest that would advertise something untrue:
///
/// - a group with an empty `name` or empty `upstream_names`
/// - an **orphan**: a registered tool no group claims — it exists but is
///   unreachable through any advertised prefix
/// - a **double-claim**: a tool claimed by more than one group, so which prefix
///   serves it is undefined
/// - a **ghost**: a group naming a tool that is not registered — the manifest
///   advertises a tool the server does not have
/// - a tagged or digest-pinned `oci_image` (cloister ADR-0041)
///
/// Group order and `upstream_names` order are preserved exactly as given, so a
/// producer controls its own diff stability.
pub fn render(
    meta: &ServerMeta<'_>,
    tools: &[ToolRef<'_>],
    groups: &[GroupRef<'_>],
) -> Result<String> {
    // A descriptor with no packages names no artifact, so `<identifier>:<version>`
    // derives from nothing. That is the v0.11.2 failure with the address absent
    // rather than malformed.
    if meta.packages.is_empty() {
        bail!(
            "no packages declared. A descriptor exists so a consumer can derive \
             `<identifier>:<version>` (cloister ADR-0038); with no packages there is \
             nothing to derive and the file describes an artifact that cannot be \
             fetched.",
        );
    }

    // Per-entry, not once. Checking only the first would let a second package
    // carry a tagged identifier through — which is exactly the shape
    // check-image-versions.ts already guards downstream, and the reason notme
    // could not adopt this crate (`ley-line-open-44cc45`).
    for (i, pkg) in meta.packages.iter().enumerate() {
        if pkg.oci_image.contains(':') || pkg.oci_image.contains('@') {
            bail!(
                "packages[{i}].oci_image `{}` carries a tag or digest; cloister \
                 ADR-0041 requires the registry path alone. A tagged identifier \
                 promises an image that must then be published under exactly that \
                 tag, and fails at compose up rather than at resolve when it is not.",
                pkg.oci_image,
            );
        }

        if pkg.oci_version.is_empty() {
            bail!(
                "packages[{i}].oci_version is empty. Rejecting a tagged identifier \
                 without requiring the version that replaces it enforces half of \
                 cloister ADR-0041: the derive rule is `<identifier>:<version>`, so \
                 an address with no version resolves to nothing — strictly worse \
                 than the tag it replaced. `ley-line-open-04300f` shipped that in \
                 v0.11.2.",
            );
        }

        // The MCP registry schema lists `transport` in `Package.required`:
        //
        //     Package.required = ["registryType", "identifier", "transport"]
        //
        // So a package without one is INVALID against the very schema the
        // document's `$schema` key declares. This crate will not emit a file
        // that fails its own declared spec — that is the "well-formed but
        // wrong" shape ley-line-open-891dd5 exists to prevent, and emitting it
        // silently would be worse than refusing.
        //
        // This is a real spec limitation, not an oversight here: the MCP
        // schema has no way to describe a producer that publishes images and
        // serves no MCP. notme is such a producer, and its own descriptor says
        // the file "exists for the ADR-0041 image-publish contract … not an
        // MCP registry entry" — which is precisely the case the schema cannot
        // express. Its committed server.json omits `transport` on both
        // packages and is therefore schema-invalid today.
        //
        // Resolving that needs an ecosystem decision (a package-identity
        // manifest distinct from an MCP registry entry), not a quiet
        // workaround here. Tracked on `ley-line-open-44cc45`.
        if pkg.transport.is_none() {
            bail!(
                "packages[{i}] (`{}`) declares no transport, but the MCP registry \
                 schema lists `transport` in Package.required — the emitted file \
                 would fail the schema its own `$schema` key names. The MCP schema \
                 cannot describe a producer that publishes images and serves no \
                 MCP; that needs a package-identity manifest distinct from an MCP \
                 registry entry. See `ley-line-open-44cc45`.",
                pkg.oci_image,
            );
        }
    }

    let mut owner: HashMap<&str, Vec<&str>> = HashMap::new();
    for g in groups {
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

    let registered: HashSet<&str> = tools.iter().map(|t| t.name).collect();

    let mut orphans: Vec<&str> = registered
        .iter()
        .filter(|t| !owner.contains_key(*t))
        .copied()
        .collect();
    orphans.sort_unstable();
    if !orphans.is_empty() {
        bail!(
            "{} registered tool(s) are not claimed by any cloister group: {:?}",
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
        bail!("tool(s) claimed by multiple cloister groups: {:?}", over);
    }

    let mut ghosts: Vec<&str> = owner
        .keys()
        .filter(|t| !registered.contains(*t))
        .copied()
        .collect();
    ghosts.sort_unstable();
    if !ghosts.is_empty() {
        bail!(
            "{} cloister group claim(s) reference tools that are not registered: {:?}",
            ghosts.len(),
            ghosts,
        );
    }

    let doc = ServerDoc {
        schema: SCHEMA_URL,
        name: meta.name,
        description: meta.description,
        version: meta.version,
        repository: Repository {
            url: meta.repository_url,
            source: meta.repository_source,
        },
        packages: meta
            .packages
            .iter()
            .map(|p| Package {
                registry_type: "oci",
                identifier: p.oci_image,
                version: p.oci_version,
                transport: p.transport.map(|t| Transport {
                    typ: t.typ,
                    url: t.url,
                }),
                environment_variables: vec![],
            })
            .collect(),
        // Omitted entirely when the producer declares no groups. An empty
        // `art.cloister/v1` block would advertise a cloister surface that does
        // not exist; notme's own descriptor makes the point — declaring one
        // there would make cloister "generate backends for tools that do not
        // exist." Absence is the correct signal, not an empty object.
        meta: if groups.is_empty() {
            None
        } else {
            Some(Meta {
                art_cloister_v1: ArtCloisterV1 {
                    groups: groups
                        .iter()
                        .map(|g| GroupOut {
                            name: g.name,
                            advertised_prefix: g.advertised_prefix,
                            upstream_names: g.upstream_names.clone(),
                        })
                        .collect(),
                },
            })
        },
    };

    let mut s = serde_json::to_string_pretty(&doc)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> ServerMeta<'static> {
        ServerMeta {
            name: "io.github.example/thing",
            description: "A thing.",
            version: "1.2.3",
            repository_url: "https://github.com/example/thing.git",
            repository_source: "github",
            packages: vec![PackageMeta {
                oci_image: "ghcr.io/example/thing",
                oci_version: "v1.2.3",
                transport: Some(TransportMeta {
                    typ: "streamable-http",
                    url: "http://localhost:1234/mcp",
                }),
            }],
        }
    }

    fn group<'a>(name: &'a str, prefix: &'a str, tools: Vec<&'a str>) -> GroupRef<'a> {
        GroupRef {
            name,
            advertised_prefix: prefix,
            upstream_names: tools,
        }
    }

    #[test]
    fn renders_when_every_tool_is_claimed_exactly_once() {
        let out = render(
            &meta(),
            &[ToolRef { name: "a" }, ToolRef { name: "b" }],
            &[group("g", "g_", vec!["a", "b"])],
        )
        .expect("valid input renders");
        assert!(
            out.ends_with('\n'),
            "trailing newline keeps diffs line-for-line"
        );
        assert!(out.contains("\"art.cloister/v1\""));
    }

    /// A registered tool no group claims exists but is unreachable through any
    /// advertised prefix — the server has it, the manifest hides it.
    #[test]
    fn orphan_tool_is_rejected() {
        let err = render(
            &meta(),
            &[ToolRef { name: "a" }, ToolRef { name: "unclaimed" }],
            &[group("g", "g_", vec!["a"])],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("unclaimed"),
            "error must name the orphan: {err}"
        );
    }

    /// The failure this crate exists for: the manifest advertises a tool the
    /// server does not register, so it fails at the CONSUMER, not at build.
    #[test]
    fn ghost_claim_is_rejected() {
        let err = render(
            &meta(),
            &[ToolRef { name: "a" }],
            &[group("g", "g_", vec!["a", "does_not_exist"])],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("does_not_exist"),
            "error must name the ghost: {err}",
        );
    }

    /// Two groups claiming one tool leaves it undefined which prefix serves it.
    #[test]
    fn double_claim_is_rejected() {
        let err = render(
            &meta(),
            &[ToolRef { name: "a" }],
            &[group("g1", "g1_", vec!["a"]), group("g2", "g2_", vec!["a"])],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("multiple"), "{err}");
    }

    #[test]
    fn empty_group_name_or_tool_list_is_rejected() {
        assert!(
            render(
                &meta(),
                &[ToolRef { name: "a" }],
                &[group("", "g_", vec!["a"])]
            )
            .is_err()
        );
        assert!(render(&meta(), &[], &[group("g", "g_", vec![])]).is_err());
    }

    /// cloister ADR-0041 — `ley-line-open-04300f`. A tagged identifier promises
    /// an image that must be published under exactly that tag. LLO emitted one
    /// for an image it never publishes; mache is tagless and tests for it.
    /// Rejecting it here means no adopter can inherit that bug.
    /// The other half of ADR-0041. Rejecting the tag while allowing an absent
    /// version enforces half a rule and yields an address that derives to
    /// nothing — which is what `ley-line-open-04300f` shipped in v0.11.2.
    #[test]
    fn empty_oci_version_is_rejected() {
        let mut m = meta();
        m.packages[0].oci_version = "";
        let err = render(&m, &[ToolRef { name: "a" }], &[group("g", "g_", vec!["a"])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("derive rule"), "must explain why: {err}");
    }

    /// The pair is what makes the shape resolvable: `<identifier>:<version>`.
    #[test]
    fn identifier_and_version_are_emitted_as_siblings() {
        let out = render(
            &meta(),
            &[ToolRef { name: "a" }],
            &[group("g", "g_", vec!["a"])],
        )
        .unwrap();
        assert!(
            out.contains("\"identifier\": \"ghcr.io/example/thing\""),
            "{out}"
        );
        assert!(out.contains("\"version\": \"v1.2.3\""), "{out}");
    }

    #[test]
    fn tagged_or_digest_pinned_oci_image_is_rejected() {
        for bad in [
            "ghcr.io/example/thing:1.2.3",
            "ghcr.io/example/thing@sha256:abc",
        ] {
            let mut m = meta();
            m.packages[0].oci_image = bad;
            let err = render(&m, &[ToolRef { name: "a" }], &[group("g", "g_", vec!["a"])])
                .unwrap_err()
                .to_string();
            assert!(err.contains("ADR-0041"), "must cite the rule: {err}");
        }
    }

    /// Producers control their own diff stability, so ordering must be
    /// preserved rather than normalised.
    #[test]
    fn group_and_tool_order_are_preserved() {
        let out = render(
            &meta(),
            &[ToolRef { name: "z" }, ToolRef { name: "a" }],
            &[
                group("second", "s_", vec!["z"]),
                group("first", "f_", vec!["a"]),
            ],
        )
        .unwrap();
        assert!(
            out.find("second").unwrap() < out.find("first").unwrap(),
            "group order must be as supplied, not sorted",
        );
    }
}
