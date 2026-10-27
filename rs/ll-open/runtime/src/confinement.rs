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
//! The digest is over canonical JSON, per `confinement/v1` README §7: UTF-8
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

/// One UNIX socket grant (README §6).
///
/// Connect-only is the bare-string form on the wire and the two bind-capable
/// modes take the object form, so the cheaper spelling is the safer one — the
/// same shape [`FsGrant`] uses, for the same reason.
///
/// These are not a convenience set; each names a distinct authority over the
/// path. `Connect` requires the socket to already exist: the grant is "you may
/// talk to whatever is listening there". `Bind` permits `bind(2)`, which
/// CREATES the socket file, and withholds `connect(2)` — it is the serving half
/// alone. `ConnectBind` is both. A capability that dials a shim wants
/// `Connect`; the shim itself wants `Bind`; only a process that must both own
/// an endpoint and dial through it wants `ConnectBind`.
///
/// `Bind` exists because a mechanism under us enforces exactly it, not because
/// the vocabulary looked symmetric. libkrun's vsock muxer pairs a host UNIX
/// socket to a guest port with a `listen` flag, and a guest-originated
/// `VSOCK_OP_REQUEST` against a `listen=true` mapping is answered with a reset
/// rather than a connection — `process_op_request` in
/// `src/devices/src/virtio/vsock/muxer.rs` takes that branch explicitly. Its
/// serve-without-dial is a real, enforced state, so the manifest must be able
/// to name it. Omitting `Bind` would leave the wire vocabulary strictly less
/// expressive than the strongest mechanism beneath it, and a mode refused
/// everywhere is recoverable while a missing mode is a v2 break.
///
/// Note the asymmetry this creates for tiers. `Connect` and `ConnectBind` are
/// purely positive grants, but `Bind` carries a negative clause — MUST NOT dial
/// — and a tier that cannot enforce that clause has to refuse the mode rather
/// than widen it to `ConnectBind`. That is the §3/§5 rule, not a special case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnixSocketGrant {
    Connect(String),
    Bind { path: String },
    ConnectBind { path: String },
}

impl UnixSocketGrant {
    pub fn connect(path: impl Into<String>) -> Self {
        Self::Connect(path.into())
    }

    pub fn bind(path: impl Into<String>) -> Self {
        Self::Bind { path: path.into() }
    }

    pub fn connect_bind(path: impl Into<String>) -> Self {
        Self::ConnectBind { path: path.into() }
    }

    pub fn path(&self) -> &str {
        match self {
            Self::Connect(path) => path,
            Self::Bind { path, .. } => path,
            Self::ConnectBind { path, .. } => path,
        }
    }

    /// The wire spelling of this grant's mode, and the single source of it.
    /// `canonical_value`, the schema's enum, and every diagnostic read from
    /// here so a mode cannot be spelled two ways in one build.
    pub fn mode(&self) -> &'static str {
        match self {
            Self::Connect(_) => "connect",
            Self::Bind { .. } => "bind",
            Self::ConnectBind { .. } => "connect-bind",
        }
    }

    /// True when this grant permits `bind(2)` — i.e. permits creating the
    /// socket rather than only reaching it.
    pub fn permits_bind(&self) -> bool {
        matches!(self, Self::Bind { .. } | Self::ConnectBind { .. })
    }

    /// True when this grant permits `connect(2)`. Distinct from
    /// [`Self::permits_bind`] because `Bind` withholds exactly this, and a tier
    /// that cannot withhold it must refuse the grant.
    pub fn permits_connect(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::ConnectBind { .. })
    }
}

/// The address §4 binds when a grant omits one.
pub const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1";

/// One §4 listener grant: a port the bundle may bind, and the address.
///
/// Deliberately not `Serialize` — canonical bytes are built through
/// [`ConfinementManifest::canonical_value`], and a derive here would offer a
/// second, subtly different way to emit it. Constructed only through
/// [`ConfinementManifest::with_port_bind`], so every value carries the §4
/// port floor.
///
/// Equality is exact, including the spelling of the address: an omitted
/// address and an explicit `"127.0.0.1"` bind the same socket, but they are
/// different declarations and digest differently. That is what the meet
/// compares ([`ConfinementManifest::meet`]). Two grants for the same socket in
/// one document are refused under either spelling — see
/// [`ConfinementManifest::with_port_bind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortGrant {
    bind: u16,
    address: Option<String>,
}

impl PortGrant {
    /// The port the bundle may bind.
    pub fn bind(&self) -> u16 {
        self.bind
    }

    /// The address as declared. `None` is §4's 127.0.0.1 default, not "any".
    pub fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    /// The address this grant binds, with §4's default applied.
    pub fn effective_address(&self) -> &str {
        self.address.as_deref().unwrap_or(DEFAULT_BIND_ADDRESS)
    }

    /// Canonical list order (README §7 rule 5): `bind`, then effective
    /// address. The key is also socket identity, whatever the spelling, which
    /// is how `with_port_bind` refuses a second grant for one socket — so the
    /// order is total over any document's grants.
    fn canonical_order(&self) -> (u16, &str) {
        (self.bind, self.effective_address())
    }

    fn canonical_value(&self) -> serde_json::Value {
        let mut block: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        block.insert("bind".into(), serde_json::Value::from(self.bind));
        if let Some(address) = &self.address {
            block.insert("address".into(), serde_json::Value::String(address.clone()));
        }
        json_object(block)
    }
}

/// A list clause in canonical form (README §7 rule 5): one element is emitted
/// as the element itself, two or more as a list. The scalar spelling is what
/// every document written before lists existed already uses, so collapsing a
/// one-element list onto it is what keeps their digests where they were.
fn scalar_or_list(mut values: Vec<serde_json::Value>) -> serde_json::Value {
    if values.len() == 1 {
        values.remove(0)
    } else {
        serde_json::Value::Array(values)
    }
}

/// Read a clause that is either one `T` or a non-empty list of them.
///
/// An empty list is refused rather than read as "none": it would declare
/// nothing while still differing, on the wire, from the omitted block that
/// already means none — the `{"allow": []}` round-trip defect, not repeated.
fn one_or_many<'v>(
    value: &'v serde_json::Value,
    clause: &str,
) -> Result<Vec<&'v serde_json::Value>, ExecutionError> {
    match value {
        serde_json::Value::Array(items) if items.is_empty() => {
            Err(ExecutionError::invalid(format!(
                "confinement/v1 {clause} is an empty list, which declares nothing; \
                 omit the clause instead — an omitted clause is already the refusal"
            )))
        }
        serde_json::Value::Array(items) => Ok(items.iter().collect()),
        single => Ok(vec![single]),
    }
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
    /// Kept in canonical order (README §7 rule 5) by `with_port_bind`, so
    /// structural equality is document equality.
    ports: Vec<PortGrant>,
    unix_sockets: Vec<UnixSocketGrant>,
    /// Ascending and distinct, kept so by `with_connect_local`.
    connect_local: Vec<u16>,
}

/// The five dimensions of `confinement/v1`, decomposed so a compiler must name
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
    /// §4 — every listener grant, in canonical order. Empty means the bundle
    /// MUST NOT bind. A grant's `None` address means §4's 127.0.0.1 default,
    /// not "any".
    ///
    /// A list, not an `Option`: a consumer that read only the first grant
    /// would attest N listeners and deliver one, so the type no longer offers
    /// "the" listener to read (`ley-line-open-e2c33f`).
    pub ports: &'a [PortGrant],
    /// §5 — the vault backend this workload authenticates against.
    pub credential_source: Option<&'a str>,
    /// §6 — UNIX sockets this workload may reach. Empty means none.
    pub unix_sockets: &'a [UnixSocketGrant],
    /// §3 `network.connectLocal` — the loopback ports this workload may
    /// `connect(2)` to, ascending. Empty means none, which is the default.
    ///
    /// A sixth dimension living under §3's `network` object rather than in a
    /// section of its own: the JSON already groups it there, and this document
    /// has been renumbered three times, the last of which left a citation
    /// pointing at the section §6 had taken over. A fourth renumber to buy a
    /// heading is a bad trade while the number+title citation lint
    /// (`ley-line-open-6dac72`) is still unbuilt.
    ///
    /// **This does not authenticate the peer.** A loopback port is a shared
    /// host namespace; the grant says which port the workload may dial, not
    /// who is listening on it. §6 gets peer identity from the filesystem, this
    /// gets it from the protocol or not at all — see ADR-0037.
    pub connect_local: &'a [u16],
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
            ports: Vec::new(),
            unix_sockets: Vec::new(),
            connect_local: Vec::new(),
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

    /// Declare a §4 listener. Each call adds one grant; the manifest keeps
    /// them in canonical order, so the order of calls does not reach the
    /// digest.
    ///
    /// A second grant for a socket already granted is refused — under either
    /// spelling of the default address. Silently merging the two would leave
    /// the document's bytes, the parsed value and the digest disagreeing about
    /// how many grants there are, and picking one spelling would be a choice
    /// the issuer did not make.
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
        let grant = PortGrant {
            bind,
            address: address.map(str::to_owned),
        };
        // One search answers both questions. The canonical order key is
        // (port, effective address), which is exactly socket identity, so an
        // equal key IS the duplicate and the insertion point is where a new
        // grant keeps the list canonical.
        match self
            .ports
            .binary_search_by(|held| held.canonical_order().cmp(&grant.canonical_order()))
        {
            Ok(index) => Err(ExecutionError::invalid(format!(
                "confinement/v1 §4 port grants {bind} on {} twice (declared as \
                 {:?} and {:?}); a document names each socket once",
                grant.effective_address(),
                self.ports[index].address(),
                grant.address(),
            ))),
            Err(at) => {
                self.ports.insert(at, grant);
                Ok(self)
            }
        }
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

    /// Declare a §6 UNIX socket grant.
    ///
    /// Same path rules as §2 — absolute, no `..` — because the schema reuses
    /// the `AbsolutePath` definition for both. A socket is a filesystem object,
    /// so there is no reason for the two dimensions to disagree about what a
    /// path is.
    pub fn with_unix_socket(mut self, grant: UnixSocketGrant) -> Result<Self, ExecutionError> {
        let path = grant.path();
        if !path.starts_with('/') {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §6 unixSocket.allow path {path:?} must be absolute"
            )));
        }
        if path.split('/').any(|component| component == "..") {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §6 unixSocket.allow path {path:?} must not traverse with `..`"
            )));
        }
        self.unix_sockets.push(grant);
        Ok(self)
    }

    /// The UNIX sockets §6 permits this workload to reach.
    pub fn unix_sockets(&self) -> &[UnixSocketGrant] {
        &self.unix_sockets
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
            ports: self.port_binds(),
            credential_source: self.credential_source(),
            unix_sockets: &self.unix_sockets,
            connect_local: self.connect_local(),
        }
    }

    /// The loopback ports §3 `connectLocal` permits dialing, ascending.
    pub fn connect_local(&self) -> &[u16] {
        &self.connect_local
    }

    /// Declare a §3 `connectLocal` port — a loopback port the workload may
    /// `connect(2)` to. Each call adds one; the manifest keeps them ascending,
    /// and refuses a port already declared.
    ///
    /// A list since `ley-line-open-e2c33f` (cloister ADR-0069 broker
    /// endpoints). An earlier version kept this single-valued on the grounds
    /// that `nono::CapabilitySet::proxy_only` takes one port and is the only
    /// builder compiling to connect-WITHOUT-bind on both platforms. That
    /// remains true of the builder, not of the mechanisms: Landlock takes one
    /// `ConnectTcp` rule per port, and Seatbelt's per-port outbound rule is
    /// the very rule `proxy_only` emits, repeatable as a platform rule. The
    /// compiler (`capabilities_from_manifest`) does exactly that.
    ///
    /// No 1024 floor, unlike §4. That floor exists because binding a
    /// privileged port is a privileged act; CONNECTING to one is not — a
    /// proxy on :443 is ordinary — so the same floor here would refuse a
    /// legitimate document for a reason that does not apply.
    pub fn with_connect_local(mut self, port: u16) -> Result<Self, ExecutionError> {
        if port == 0 {
            return Err(ExecutionError::invalid(
                "confinement/v1 §3 network.connectLocal must name a port in \
                 1-65535; 0 is not an address a workload can dial, and on some \
                 mechanisms it compiles to a wildcard",
            ));
        }
        match self.connect_local.binary_search(&port) {
            Ok(_) => Err(ExecutionError::invalid(format!(
                "confinement/v1 §3 network.connectLocal declares {port} twice; a \
                 document names each port once"
            ))),
            Err(at) => {
                self.connect_local.insert(at, port);
                Ok(self)
            }
        }
    }

    /// Every listener §4 permits, in canonical order. Empty means the
    /// manifest declares no listener, which §4 defines as MUST NOT bind.
    ///
    /// Each grant's address is unresolved — `None` means "§4's default", not
    /// "any address", and only the caller knows whether it can enforce the
    /// distinction.
    pub fn port_binds(&self) -> &[PortGrant] {
        &self.ports
    }

    /// The meet of two documents: the policy both permit.
    ///
    /// Defined for the two list clauses — §4 `port` and §3 `connectLocal` —
    /// as set intersection (README §7, "Meet"). A grant survives only when
    /// both operands carry it, compared as canonical grants: an omitted
    /// address and an explicit `"127.0.0.1"` are different declarations, so
    /// they do not meet. That can only drop a grant, never add one, which is
    /// the direction a meet must err in. An empty intersection omits the
    /// clause, and a `network` block left with no clause is omitted with it,
    /// so a meet with nothing in common is byte-identical to a document that
    /// never declared the dimension.
    ///
    /// Every other dimension must be EQUAL on both sides, or the meet is
    /// refused naming the dimensions that differ. v1 defines no meet for path
    /// prefixes, host patterns, socket modes or vault backends, and inventing
    /// one here would let this function decide policy no document stated.
    ///
    /// Commutative and idempotent, and the result is in canonical order
    /// because both operands are.
    pub fn meet(&self, other: &Self) -> Result<Self, ExecutionError> {
        let Dimensions {
            fs_allow,
            allow_hosts,
            ports: _,
            credential_source,
            unix_sockets,
            connect_local: _,
        } = self.dimensions();
        let theirs = other.dimensions();
        let mut differing = Vec::new();
        if fs_allow != theirs.fs_allow {
            differing.push("§2 fs.allow");
        }
        if allow_hosts != theirs.allow_hosts {
            differing.push("§3 network.allowHosts");
        }
        if credential_source != theirs.credential_source {
            differing.push("§5 credentialSource");
        }
        if unix_sockets != theirs.unix_sockets {
            differing.push("§6 unixSocket.allow");
        }
        if !differing.is_empty() {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 meet is defined for §4 port and §3 \
                 network.connectLocal only; the operands differ in {}, which v1 \
                 gives no meet for",
                differing.join(", ")
            )));
        }
        let mut meet = self.clone();
        meet.ports
            .retain(|grant| other.ports.iter().any(|theirs| theirs == grant));
        meet.connect_local
            .retain(|port| other.connect_local.binary_search(port).is_ok());
        Ok(meet)
    }

    /// Serialize to the canonical JSON the digest is computed over (§7).
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
        // One `network` object carrying whichever of its two clauses are
        // declared. Emitted only when at least one is, so a manifest using
        // neither digests exactly as it did before `connectLocal` existed —
        // which is what keeps the pinned v1 vector still.
        if !self.allow_hosts.is_empty() || !self.connect_local.is_empty() {
            let mut network: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            if !self.allow_hosts.is_empty() {
                let hosts: Vec<serde_json::Value> = self
                    .allow_hosts
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect();
                network.insert("allowHosts".into(), serde_json::Value::Array(hosts));
            }
            // §7 rule 5: one port is the bare integer — the only spelling that
            // existed before lists, so every such document keeps its digest.
            if !self.connect_local.is_empty() {
                let ports = self
                    .connect_local
                    .iter()
                    .copied()
                    .map(serde_json::Value::from)
                    .collect();
                network.insert("connectLocal".into(), scalar_or_list(ports));
            }
            root.insert("network".into(), json_object(network));
        }
        if !self.unix_sockets.is_empty() {
            let allow: Vec<serde_json::Value> = self
                .unix_sockets
                .iter()
                .map(|grant| match grant {
                    // Bare string for connect-only, object for every
                    // bind-capable mode — the same asymmetry §2 uses, so the
                    // cheaper spelling stays the safer one on the wire as well
                    // as in the builder. The mode string comes from
                    // `grant.mode()` rather than a literal here, so adding a
                    // mode cannot leave the canonical bytes spelling it
                    // differently from the schema and the parser.
                    UnixSocketGrant::Connect(path) => serde_json::Value::String(path.clone()),
                    UnixSocketGrant::Bind { path } | UnixSocketGrant::ConnectBind { path } => {
                        let mut entry: BTreeMap<String, serde_json::Value> = BTreeMap::new();
                        entry.insert(
                            "mode".into(),
                            serde_json::Value::String(grant.mode().to_owned()),
                        );
                        entry.insert("path".into(), serde_json::Value::String(path.clone()));
                        json_object(entry)
                    }
                })
                .collect();
            let mut block: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            block.insert("allow".into(), serde_json::Value::Array(allow));
            root.insert("unixSocket".into(), json_object(block));
        }
        // §7 rule 5: one grant is the bare object — byte-identical to the
        // single-port form, which is what keeps the pinned vector's digest —
        // and two or more are a list in canonical order.
        if !self.ports.is_empty() {
            let grants = self.ports.iter().map(PortGrant::canonical_value).collect();
            root.insert("port".into(), scalar_or_list(grants));
        }
        json_object(root)
    }

    /// Read a `confinement/v1` document — the wire form a grant carries.
    ///
    /// Without this the type was WRITE-ONLY: it could emit canonical JSON and
    /// not consume it, so a grant's confinement document could never become the
    /// manifest LLO compiles. §4 had no route to originate from an authorizer at
    /// all, which is why the microVM tier's listener handling was unreachable
    /// even once it existed.
    ///
    /// Parse, not validate. Every field goes through the same fallible builder
    /// a programmatic caller uses, so a parsed manifest carries exactly the
    /// invariants a constructed one does — there is no path that produces a
    /// `ConfinementManifest` the builders would have refused. That is also what
    /// lets the schema's refusal table be pointed at this type instead of at
    /// `json!` literals in another crate.
    pub fn parse(document: &str) -> Result<Self, ExecutionError> {
        let value: serde_json::Value = serde_json::from_str(document).map_err(|error| {
            ExecutionError::invalid(format!("invalid confinement JSON: {error}"))
        })?;
        let object = value.as_object().ok_or_else(|| {
            ExecutionError::invalid("a confinement/v1 manifest must be a JSON object")
        })?;

        // Unknown keys are refused rather than ignored. The schema sets
        // `additionalProperties: false` throughout, and a dimension we do not
        // recognise is exactly the case where silently dropping it would attest
        // a document whose clause had no effect.
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "version" | "credentialSource" | "fs" | "network" | "port" | "unixSocket"
            ) {
                return Err(ExecutionError::invalid(format!(
                    "unknown confinement/v1 dimension {key:?}; refusing rather \
                     than digesting a clause nothing reads"
                )));
            }
        }

        // The version is the document's claim about which spec it conforms to,
        // and it is the first thing that must hold: every refusal below is a
        // v1 rule, so applying them to a document announcing something else
        // would be checking the wrong contract.
        match object.get("version").and_then(serde_json::Value::as_str) {
            Some(CONFINEMENT_SCHEMA_VERSION) => {}
            Some(other) => {
                return Err(ExecutionError::invalid(format!(
                    "confinement manifest declares version {other:?}, but this \
                     reader implements {CONFINEMENT_SCHEMA_VERSION}"
                )));
            }
            None => {
                return Err(ExecutionError::invalid(
                    "a confinement manifest must declare its `version`",
                ));
            }
        }

        let mut manifest = Self::new();

        if let Some(source) = object.get("credentialSource") {
            let source = source
                .as_str()
                .ok_or_else(|| ExecutionError::invalid("credentialSource must be a string"))?;
            manifest = manifest.with_credential_source(source)?;
        }

        if let Some(fs) = object.get("fs") {
            let allow = fs
                .get("allow")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| ExecutionError::invalid("fs must carry an `allow` array"))?;
            for entry in allow {
                let grant = match entry {
                    // §2's two spellings: a bare string is read-only, an object
                    // with `mode: "rw"` is read-write. The trailing slash still
                    // marks a directory subtree in both.
                    serde_json::Value::String(path) => FsGrant::read_only(path.clone()),
                    serde_json::Value::Object(map) => {
                        let path = map
                            .get("path")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| {
                                ExecutionError::invalid("an fs.allow object needs a `path`")
                            })?;
                        match map.get("mode").and_then(serde_json::Value::as_str) {
                            Some("rw") => FsGrant::read_write(path),
                            other => {
                                return Err(ExecutionError::invalid(format!(
                                    "fs.allow mode must be \"rw\" when present, got {other:?}"
                                )));
                            }
                        }
                    }
                    other => {
                        return Err(ExecutionError::invalid(format!(
                            "an fs.allow entry must be a string or an object, got {other}"
                        )));
                    }
                };
                manifest = manifest.with_fs_grant(grant)?;
            }
        }

        if let Some(unix_socket) = object.get("unixSocket") {
            let allow = unix_socket
                .get("allow")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| ExecutionError::invalid("unixSocket must carry an `allow` array"))?;
            for entry in allow {
                let grant = match entry {
                    serde_json::Value::String(path) => UnixSocketGrant::connect(path.clone()),
                    serde_json::Value::Object(map) => {
                        let path = map
                            .get("path")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| {
                                ExecutionError::invalid("a unixSocket.allow object needs a `path`")
                            })?;
                        match map.get("mode").and_then(serde_json::Value::as_str) {
                            Some("bind") => UnixSocketGrant::bind(path),
                            Some("connect-bind") => UnixSocketGrant::connect_bind(path),
                            // "connect" is deliberately NOT accepted here: it is
                            // the bare-string form. One grant, one spelling —
                            // otherwise two documents differing only in how they
                            // spell the same grant would digest differently.
                            other => {
                                return Err(ExecutionError::invalid(format!(
                                    "unixSocket.allow mode must be \"bind\" or \"connect-bind\" \
                                     when present (connect-only is the bare-string form), \
                                     got {other:?}"
                                )));
                            }
                        }
                    }
                    other => {
                        return Err(ExecutionError::invalid(format!(
                            "a unixSocket.allow entry must be a string or an object, got {other}"
                        )));
                    }
                };
                manifest = manifest.with_unix_socket(grant)?;
            }
        }

        if let Some(network) = object.get("network") {
            // Either clause may be absent, but an empty `network` object is
            // refused: it digests as a declaration while granting nothing, and
            // §1's rule is that an omitted block is the refusal. A document
            // meaning "no egress, no local channel" omits `network` entirely.
            if network.get("allowHosts").is_none() && network.get("connectLocal").is_none() {
                return Err(ExecutionError::invalid(
                    "confinement/v1 §3 `network` must carry `allowHosts`, \
                     `connectLocal`, or both — an empty block declares nothing \
                     and an omitted one is already the refusal",
                ));
            }
            if let Some(hosts) = network.get("allowHosts") {
                let hosts = hosts.as_array().ok_or_else(|| {
                    ExecutionError::invalid("network.allowHosts must be an array")
                })?;
                for host in hosts {
                    let host = host.as_str().ok_or_else(|| {
                        ExecutionError::invalid("an allowHosts entry must be a string")
                    })?;
                    manifest = manifest.with_allowed_host(host)?;
                }
            }
            if let Some(ports) = network.get("connectLocal") {
                for port in one_or_many(ports, "§3 network.connectLocal")? {
                    let port = port.as_u64().ok_or_else(|| {
                        ExecutionError::invalid(
                            "network.connectLocal must be an integer port or a list of them",
                        )
                    })?;
                    let port = u16::try_from(port).map_err(|_| {
                        ExecutionError::invalid(format!(
                            "network.connectLocal {port} exceeds the 16-bit port space"
                        ))
                    })?;
                    manifest = manifest.with_connect_local(port)?;
                }
            }
        }

        if let Some(ports) = object.get("port") {
            for port in one_or_many(ports, "§4 port")? {
                if !port.is_object() {
                    return Err(ExecutionError::invalid(
                        "a §4 port grant must be an object carrying `bind`",
                    ));
                }
                let bind = port
                    .get("bind")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| ExecutionError::invalid("port must carry an integer `bind`"))?;
                let bind = u16::try_from(bind).map_err(|_| {
                    ExecutionError::invalid(format!(
                        "port.bind {bind} exceeds the 16-bit port space"
                    ))
                })?;
                let address = port.get("address").and_then(serde_json::Value::as_str);
                manifest = manifest.with_port_bind(bind, address)?;
            }
        }

        Ok(manifest)
    }

    pub fn to_canonical_json(&self) -> Result<String, ExecutionError> {
        // `to_string_pretty` emits two-space indent and no trailing newline,
        // which is exactly §7.
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
