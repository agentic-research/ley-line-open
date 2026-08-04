//! `cloister/confinement/v1` manifests, and the digest a backend commits to.
//!
//! This is ADR-0035 §1's "single declaration": one manifest from which the
//! applied `nono::CapabilitySet`, the `confinementDigest`, and the digest a
//! backend declares are all projections. They cannot drift because they are
//! computed from the same value.
//!
//! Before this existed, `build_process_capabilities` compiled a hardcoded
//! policy with no relationship to the `confinementDigest` the grant named
//! (PR #312 finding 2). A grant could name any policy, the worker applied a
//! different one, and the receipt attested the named one — so a verifier
//! comparing the receipt against its own policy pin got a true answer to the
//! wrong question.
//!
//! ## Canonical form
//!
//! The digest is over canonical JSON, per `confinement/v1` README §6: UTF-8
//! with no BOM, object keys sorted in ASCII byte order at every level,
//! two-space indent, **no trailing newline**, and absent fields omitted
//! rather than emitted as `null`. `BTreeMap` gives the key order and
//! `to_string_pretty` gives the indent; the pinned vector round-trips through
//! this byte-for-byte, which `tests/confinement_commitment.rs` asserts.
//!
//! ## Why JSON and not capnp
//!
//! Because the digest is defined over JSON. Generating the JSON from a capnp
//! IDL would make it a projection of a different source of truth, leaving two
//! definitions for one signed surface — ADR-0035 §8, and the rule recorded in
//! schema-spec's `LAYOUT.md`.

use std::collections::BTreeMap;

use leyline_core::ContentAddressed;

use crate::ExecutionError;

/// The contract identifier every v1 manifest carries.
pub const CONFINEMENT_SCHEMA_VERSION: &str = "cloister/confinement/v1";

/// Build a JSON object whose key order is the map's, not insertion order.
///
/// `serde_json::Value::Object` wraps a `Map` whose ordering depends on the
/// `preserve_order` feature — enabled in this workspace. Collecting from a
/// `BTreeMap` therefore preserves ASCII key order through the conversion,
/// which §6 requires at every nesting level.
fn json_object(entries: BTreeMap<String, serde_json::Value>) -> serde_json::Value {
    serde_json::Value::Object(entries.into_iter().collect())
}

/// One filesystem path grant (README §2).
///
/// Read-only is the bare-string form on the wire and read-write is the object
/// form, so the cheaper spelling is the safer one. There is deliberately no
/// `"mode": "ro"`: one grant, one spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsGrant {
    ReadOnly(String),
    ReadWrite { path: String },
}

impl FsGrant {
    pub fn read_only(path: impl Into<String>) -> Self {
        Self::ReadOnly(path.into())
    }

    pub fn read_write(path: impl Into<String>) -> Self {
        Self::ReadWrite { path: path.into() }
    }

    pub fn path(&self) -> &str {
        match self {
            Self::ReadOnly(path) => path,
            Self::ReadWrite { path, .. } => path,
        }
    }
}

/// The `port` block (README §4). Deliberately not `Serialize` — canonical
/// bytes are built through [`ConfinementManifest::canonical_value`], and a
/// derive here would offer a second, subtly different way to emit it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PortBlock {
    bind: u16,
    address: Option<String>,
}

/// A `cloister/confinement/v1` manifest.
///
/// Every dimension defaults to DENY: an omitted block is a refusal, never an
/// escape hatch. That is why the builder only adds grants — there is no way
/// to express "unrestricted" because the spec has no such state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinementManifest {
    credential_source: Option<String>,
    fs_allow: Vec<FsGrant>,
    allow_hosts: Vec<String>,
    port: Option<PortBlock>,
}

/// The four dimensions of `confinement/v1`, decomposed so a compiler must name
/// all of them. See [`ConfinementManifest::dimensions`] for why this exists as
/// a struct rather than as four accessors.
///
/// Adding a dimension here is deliberately a breaking change for every
/// consumer. That is the feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions<'a> {
    /// §2 — filesystem grants. Trailing slash marks a directory subtree.
    pub fs_allow: &'a [FsGrant],
    /// §3 — hosts egress is permitted to. Empty means none.
    pub allow_hosts: &'a [String],
    /// §4 — the single listener, as `(port, address)`. `None` address means
    /// §4's 127.0.0.1 default, not "any".
    pub port: Option<(u16, Option<&'a str>)>,
    /// §5 — the vault backend this workload authenticates against.
    pub credential_source: Option<&'a str>,
}

impl Default for ConfinementManifest {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfinementManifest {
    /// A manifest that allows nothing — the correct starting point, because
    /// every dimension denies by default.
    pub fn new() -> Self {
        Self {
            credential_source: None,
            fs_allow: Vec::new(),
            allow_hosts: Vec::new(),
            port: None,
        }
    }

    /// Declare §5's vault backend.
    ///
    /// The scheme enumeration is closed by the spec, on the stated grounds that
    /// "a scheme this spec does not name is a scheme the reference validator
    /// will refuse, and accepting it here would move the refusal to a less
    /// obvious place." Accepting an arbitrary string here did exactly that.
    pub fn with_credential_source(
        mut self,
        uri: impl Into<String>,
    ) -> Result<Self, ExecutionError> {
        const SCHEMES: [&str; 6] = [
            "keychain://",
            "secret-tool://",
            "keyring://",
            "file://",
            "op://",
            "apple-password://",
        ];
        let uri = uri.into();
        let scheme_ok = SCHEMES
            .iter()
            .any(|scheme| uri.starts_with(scheme) && uri.len() > scheme.len());
        if !scheme_ok {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §5 credentialSource {uri:?} does not use one of \
                 the schemes the spec closes over ({}), each with a non-empty \
                 remainder.",
                SCHEMES.join(", ")
            )));
        }
        self.credential_source = Some(uri);
        Ok(self)
    }

    /// Declare a §2 filesystem grant.
    ///
    /// `AbsolutePath` in the schema is `^/(?!.*(?:^|/)\.\.(?:/|$)).*$` — leading
    /// slash, no `..` component. Symlink resolution is explicitly the runner's
    /// obligation and stays out of scope here, as the schema says.
    pub fn with_fs_grant(mut self, grant: FsGrant) -> Result<Self, ExecutionError> {
        let path = grant.path();
        if !path.starts_with('/') {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §2 fs.allow path {path:?} must be absolute"
            )));
        }
        if path.split('/').any(|component| component == "..") {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §2 fs.allow path {path:?} must not traverse with `..`"
            )));
        }
        self.fs_allow.push(grant);
        Ok(self)
    }

    /// Declare a §3 egress host.
    ///
    /// One leading `*.` or none. The spec rejects an interior wildcard because
    /// "a pattern whose match set is hard to read is a pattern whose grant is
    /// hard to audit" — which is a property of the grant, not of the parser,
    /// and so belongs at the point the grant is made.
    pub fn with_allowed_host(mut self, host: impl Into<String>) -> Result<Self, ExecutionError> {
        let host = host.into();
        let labels = host.strip_prefix("*.").unwrap_or(&host);
        let well_formed = !labels.is_empty()
            && labels.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        if !well_formed {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §3 network.allowHosts entry {host:?} is not a \
                 hostname with at most one leading `*.` wildcard"
            )));
        }
        self.allow_hosts.push(host);
        Ok(self)
    }

    /// Declare §4's single listener.
    ///
    /// Fallible because `confinement.schema.json` constrains `bind` to
    /// 1024–65535 and this type is named for that spec. Accepting a wider `u16`
    /// made `ConfinementManifest` decorative: it could produce, and digest, a
    /// document its own schema refuses — with the refusal asserted only against
    /// `json!` literals in another crate, never against the type that computes
    /// the attested bytes.
    ///
    /// Port 0 is the reason this is a refusal rather than a lint. nono
    /// documents it as the macOS `localhost:*` wildcard and emits
    /// `(allow network-outbound (remote tcp "localhost:*"))`, so a `u16` field
    /// admitted a value whose compiled meaning is "every localhost port" — the
    /// exact inverse of a single-port grant.
    pub fn with_port_bind(
        mut self,
        bind: u16,
        address: Option<&str>,
    ) -> Result<Self, ExecutionError> {
        if bind < 1024 {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §4 port.bind must be 1024-65535, got {bind}. \
                 Privileged ports are out of scope in v1, and 0 is nono's \
                 macOS `localhost:*` wildcard — a value whose compiled meaning \
                 is every port rather than one."
            )));
        }
        self.port = Some(PortBlock {
            bind,
            address: address.map(str::to_owned),
        });
        Ok(self)
    }

    pub fn fs_grants(&self) -> &[FsGrant] {
        &self.fs_allow
    }

    /// The hosts §3 permits egress to, if any.
    ///
    /// Exposed for the same reason `port_bind` is: a compiler that cannot read
    /// a dimension cannot refuse it either, and silently dropping a dimension
    /// the manifest declares is what `ley-line-open-17536d` is about.
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allow_hosts
    }

    /// The vault backend §5 binds this workload to, if any.
    pub fn credential_source(&self) -> Option<&str> {
        self.credential_source.as_deref()
    }

    /// Every dimension `confinement/v1` defines, in a shape a consumer must
    /// name exhaustively.
    ///
    /// Accessors alone were not enough, and this crate has the scar: a compiler
    /// that calls `fs_grants()`, `allowed_hosts()` and `port_bind()` reads three
    /// dimensions and silently ignores the fourth, which is exactly what
    /// happened to §5 — `credential_source` had no accessor at all, so the
    /// omission could not even be seen at the call site. Adding a dimension
    /// compiled clean everywhere and no-oped somewhere.
    ///
    /// Destructured with a struct pattern and NO `..`, this becomes a compile
    /// error instead:
    ///
    /// ```text
    /// error[E0027]: pattern does not mention field `gpu`
    ///    --> backends/libkrun/confinement.rs:96:9
    ///     |
    ///     |     let Dimensions { fs_allow, allow_hosts, port, credential_source } =
    ///     |         ^^^^^^^^^^^^^ missing field `gpu`
    /// ```
    ///
    /// Note this is checked at the CONSUMER, which makes it stronger than
    /// `EnforcedCeilings`' per-tier table: that one gets its teeth from every
    /// construction site being a full struct literal, and a single
    /// `..Default::default()` would absorb a new field in silence. The only
    /// escape here is a `..` in the pattern — one visible, greppable token.
    /// For the same reason, `EnforcedCeilings` must never derive `Default`.
    pub fn dimensions(&self) -> Dimensions<'_> {
        Dimensions {
            fs_allow: &self.fs_allow,
            allow_hosts: &self.allow_hosts,
            port: self.port_bind(),
            credential_source: self.credential_source(),
        }
    }

    /// The single listener §4 permits, as `(port, address)`. `None` means the
    /// manifest declares no listener, which §4 defines as MUST NOT bind.
    ///
    /// The address is returned unresolved — `None` here means "§4's default",
    /// not "any address", and only the caller knows whether it can enforce the
    /// distinction.
    pub fn port_bind(&self) -> Option<(u16, Option<&str>)> {
        self.port
            .as_ref()
            .map(|port| (port.bind, port.address.as_deref()))
    }

    /// Serialize to the canonical JSON the digest is computed over (§6).
    ///
    /// Every object is built as a `BTreeMap`, at every level, rather than as a
    /// `#[derive(Serialize)]` struct. That is not stylistic: this workspace
    /// enables serde_json's `preserve_order`, so a derived struct serializes
    /// in *declaration* order while §6 requires ASCII key order everywhere.
    /// The two agree by accident for some structs and disagree for others —
    /// the `port` block was emitted `bind, address` against a vector that
    /// says `address, bind`, and nothing but a byte comparison caught it.
    ///
    /// Ordering a struct's fields alphabetically would "work" and would be a
    /// rule no compiler checks, silently broken by the next person who adds a
    /// field. `BTreeMap` makes the order a property of the container instead.
    fn canonical_value(&self) -> serde_json::Value {
        let mut root: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        root.insert(
            "version".into(),
            serde_json::Value::String(CONFINEMENT_SCHEMA_VERSION.to_owned()),
        );
        if let Some(source) = &self.credential_source {
            root.insert(
                "credentialSource".into(),
                serde_json::Value::String(source.clone()),
            );
        }
        if !self.fs_allow.is_empty() {
            let allow: Vec<serde_json::Value> = self
                .fs_allow
                .iter()
                .map(|grant| match grant {
                    FsGrant::ReadOnly(path) => serde_json::Value::String(path.clone()),
                    FsGrant::ReadWrite { path, .. } => {
                        let mut entry: BTreeMap<String, serde_json::Value> = BTreeMap::new();
                        entry.insert("mode".into(), serde_json::Value::String("rw".into()));
                        entry.insert("path".into(), serde_json::Value::String(path.clone()));
                        json_object(entry)
                    }
                })
                .collect();
            let mut fs: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            fs.insert("allow".into(), serde_json::Value::Array(allow));
            root.insert("fs".into(), json_object(fs));
        }
        if !self.allow_hosts.is_empty() {
            let hosts: Vec<serde_json::Value> = self
                .allow_hosts
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect();
            let mut network: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            network.insert("allowHosts".into(), serde_json::Value::Array(hosts));
            root.insert("network".into(), json_object(network));
        }
        if let Some(port) = &self.port {
            let mut block: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            block.insert("bind".into(), serde_json::Value::from(port.bind));
            if let Some(address) = &port.address {
                block.insert("address".into(), serde_json::Value::String(address.clone()));
            }
            root.insert("port".into(), json_object(block));
        }
        json_object(root)
    }

    pub fn to_canonical_json(&self) -> Result<String, ExecutionError> {
        // `to_string_pretty` emits two-space indent and no trailing newline,
        // which is exactly §6.
        serde_json::to_string_pretty(&self.canonical_value())
            .map_err(|error| ExecutionError::invalid(format!("cannot canonicalize: {error}")))
    }

    /// The `confinementDigest` — BLAKE3 over the canonical bytes, in the
    /// `blake3-256:<hex>` form the wire uses.
    pub fn confinement_digest(&self) -> Result<String, ExecutionError> {
        let canonical = self.to_canonical_json()?;
        Ok(format!("blake3-256:{}", canonical.as_bytes().hash()))
    }

    /// Refuse unless this manifest is exactly what `committed` names.
    ///
    /// Equality, not containment. A narrower policy is refused as firmly as a
    /// wider one: the commitment says *which* policy was authorized, and a
    /// receipt attesting capabilities the workload never had is a false
    /// attestation even though it is the operationally safe direction.
    pub fn assert_matches(&self, committed: &str) -> Result<(), ExecutionError> {
        let actual = self.confinement_digest()?;
        if actual == committed {
            return Ok(());
        }
        Err(ExecutionError::identity_mismatch(format!(
            "confinement drift: the policy this backend compiles digests to \
             {actual}, but the grant commits to {committed}"
        )))
    }
}
