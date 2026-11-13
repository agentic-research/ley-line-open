//! OCI registry → authenticated CAS rootfs (bead `ley-line-open-97b636`).
//!
//! [`pull_oci_image`] is the registry source beside the layout source: it
//! pulls one digest-pinned image over the [OCI distribution API] and imports
//! it with [`import_oci_layout`], unchanged. The pull writes what it fetches
//! into a staged OCI image layout, owner-only, inside the CAS root, and the
//! import reads that layout exactly as it reads one skopeo wrote. So there is
//! one verifier: a pull and a layout import of the same digest yield the same
//! rootfs digest and the same [`ImportedRootfs`], provenance included.
//!
//! The pull adds a transport, never a second notion of what an image is:
//!
//! - **Pinned input only.** `[registry/]repository@sha256:<hex>`. A tag,
//!   alone or beside a digest, is refused, as the layout import refuses one.
//!   A name without a registry is Docker Hub's (`registry-1.docker.io`), and a
//!   one-component Docker Hub name is in `library/`.
//! - **Only the selected image.** The pinned document, then (for an index)
//!   the one manifest [`select_platform`] picks for the requested platform,
//!   its config and its layers; nothing else is requested.
//! - **Verified before parsed.** Each fetched document and blob passes the
//!   layout import's own [`check_blob`] (digest, and size against its
//!   descriptor) before it is parsed or kept; a blob is read at most one byte
//!   past its descriptor's size. The import then verifies the staged layout
//!   again, as it verifies any layout, so a pull trusts nothing it fetched.
//! - **HTTPS only.** Plain HTTP is refused, on the first request, on a token
//!   realm and on every redirect, unless [`RegistryPull::insecure_plain_http`]
//!   names that exact `host:port`, which must be a loopback address: the
//!   opt-in exists for a local test registry and admits nothing reachable.
//!   No proxy from the environment is used.
//! - **Public destinations only** (bead `ley-line-open-b1c18d`). A token
//!   realm or redirect is the registry's to choose, so a connection to any
//!   endpoint other than the registry the reference names and the loopback
//!   authority the opt-in names must resolve to public addresses only:
//!   loopback, private, link-local, CGNAT and the other ranges [`is_public`]
//!   lists are refused ([`OciImportError::NonPublicDestination`]) before a
//!   socket is opened, on the addresses ureq resolved for that connection.
//!   The registry itself may be private, but once it has resolved to public
//!   addresses it may not resolve to a non-public one later in the pull.
//! - **Anonymous.** The bearer-token flow ghcr.io and Docker Hub use for
//!   public images: a `401` whose `WWW-Authenticate: Bearer realm=…` names a
//!   token endpoint, one anonymous token for `repository:<name>:pull`, sent
//!   only to the registry itself, never on a redirect (a CDN gets none). A
//!   challenge for credentials (`Basic`), a token the registry refuses, and a
//!   realm that is not HTTPS are refused. The token never appears in an
//!   error, and an error names a redirect target without its query (signed
//!   CDN URLs carry their signature there).
//! - **Bounded.** [`RegistryLimits`]: the bytes downloaded in total (the
//!   documents as they arrive, then the descriptors' sizes before any blob is
//!   requested), the redirects one request may follow, the connect
//!   timeout, the longest a body may go without a byte, each request's
//!   header deadline (connecting, the TLS handshake, sending and the
//!   response headers: twice the connect timeout plus the response
//!   timeout), and each body's deadline: the response timeout plus the
//!   descriptor's size at a minimum transfer rate. Both deadlines are
//!   enforced on every read and write of the connection.
//!
//! [OCI distribution API]: https://github.com/opencontainers/distribution-spec/blob/v1.1.0/spec.md

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use ureq::http::Uri;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::time as ureq_time;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, NextTimeout, RustlsConnector, TcpConnector, Transport,
};

use super::oci::{
    DEFAULT_MAX_ROOTFS_BYTES, Document, INDEX_TYPES, ImportedRootfs, Index, MANIFEST_TYPES,
    MAX_DOCUMENT_BYTES, Manifest, OciImport, OciImportError, Platform, SymlinkPolicy,
    admit_manifest, check_blob, classify, import_oci_layout, parse, parse_reference,
    select_platform, sha256_hex,
};

/// Docker Hub's registry endpoint, for names without a registry.
const DOCKER_HUB: &str = "registry-1.docker.io";

/// A token endpoint's reply larger than this is refused.
const MAX_TOKEN_BYTES: u64 = 64 << 10;

/// Bounds on one pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryLimits {
    /// Bytes downloaded across every document and blob of the pull.
    pub max_download_bytes: u64,
    /// Redirects one request may follow (registries send blobs to a CDN).
    pub max_redirects: u32,
    /// Bound on resolving and connecting (TLS handshake included).
    pub connect_timeout: Duration,
    /// Bound on sending a request and receiving its response headers, on
    /// any wait for the next byte of a body, and the fixed part of every
    /// request's deadline.
    pub response_timeout: Duration,
    /// The slowest transfer accepted: a request for a body of `n` bytes
    /// (connecting included) must finish within
    /// `response_timeout + n / min_bytes_per_second` (whole seconds).
    pub min_bytes_per_second: u64,
}

impl Default for RegistryLimits {
    fn default() -> Self {
        Self {
            max_download_bytes: DEFAULT_MAX_ROOTFS_BYTES,
            max_redirects: 5,
            connect_timeout: Duration::from_secs(30),
            response_timeout: Duration::from_secs(60),
            min_bytes_per_second: 128 << 10,
        }
    }
}

/// One pull: which image, from where, for which platform, under which link
/// policy and bounds.
#[derive(Debug, Clone)]
pub struct RegistryPull<'a> {
    /// `[registry/]repository@sha256:<hex>` of an image index or manifest.
    pub reference: &'a str,
    pub platform: Platform,
    pub symlinks: SymlinkPolicy,
    /// As [`OciImport::max_rootfs_bytes`].
    pub max_rootfs_bytes: u64,
    pub limits: RegistryLimits,
    /// The one registry (`host:port`, a loopback IP address) this pull may
    /// reach over plain HTTP. `None`, the default, is HTTPS only.
    pub insecure_plain_http: Option<&'a str>,
}

/// Pull the image `request` pins into a staged layout inside `cas_root`, an
/// existing directory, import it from there, and return the published
/// rootfs. The staging is removed whatever the outcome.
pub fn pull_oci_image(
    request: &RegistryPull<'_>,
    cas_root: &Path,
) -> Result<ImportedRootfs, OciImportError> {
    let target = Target::parse(request.reference)?;
    let plain = plain_http_authority(request.insecure_plain_http)?;
    let cas_root = cas_root
        .canonicalize()
        .map_err(io_error("open the CAS root"))?;
    let staging = tempfile::Builder::new()
        .prefix(".oci-pull-")
        .tempdir_in(&cas_root)
        .map_err(io_error("create pull staging in the CAS root"))?;
    fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))
        .map_err(io_error("make pull staging private"))?;
    let layout = StagedLayout::create(staging.path().join("layout"))?;

    let mut client = Client::new(&target, plain, &request.limits);
    let manifest_accept = INDEX_TYPES
        .iter()
        .chain(MANIFEST_TYPES.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let top = client.document(&target.digest, None, &manifest_accept)?;
    layout.write(&target.digest, &top)?;
    let manifest_bytes = match classify(&top)? {
        Document::Index => {
            let index: Index = parse(&top, "image index")?;
            let selected = select_platform(&index, &request.platform)?;
            let bytes = client.document(&selected.digest, Some(selected.size), &manifest_accept)?;
            layout.write(&selected.digest, &bytes)?;
            bytes
        }
        Document::Manifest => top,
    };
    let manifest: Manifest = parse(&manifest_bytes, "image manifest")?;
    // What the import would refuse before reading a layer, refused before
    // requesting one.
    admit_manifest(&manifest)?;
    let blobs: Vec<_> = std::iter::once(&manifest.config)
        .chain(&manifest.layers)
        .collect();
    // Every size is known now: refuse an image over the limit before
    // requesting any of its blobs.
    let remaining = blobs
        .iter()
        .try_fold(0u64, |total, blob| total.checked_add(blob.size))
        .unwrap_or(u64::MAX);
    client.reserve(remaining)?;
    for blob in blobs {
        if !layout.holds(&blob.digest)? {
            client.blob(&blob.digest, blob.size, &layout)?;
        }
    }

    import_oci_layout(
        &OciImport {
            layout: &layout.root,
            reference: &target.digest,
            platform: request.platform.clone(),
            symlinks: request.symlinks,
            max_rootfs_bytes: request.max_rootfs_bytes,
        },
        &cas_root,
    )
}

fn io_error(action: &'static str) -> impl FnOnce(std::io::Error) -> OciImportError {
    move |source| OciImportError::Io { action, source }
}

/// What a registry reference names.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    /// `host[:port]` of the registry.
    registry: String,
    repository: String,
    /// `sha256:<hex>`.
    digest: String,
}

impl Target {
    fn parse(reference: &str) -> Result<Self, OciImportError> {
        let Some((name, _)) = reference.rsplit_once('@') else {
            // A bare digest pins an image but names nowhere to pull it from;
            // anything else is a tag (or nothing) and pins nothing.
            parse_reference(reference)?;
            return Err(OciImportError::MalformedReference(reference.to_string()));
        };
        let digest = parse_reference(reference)?;
        let malformed = || OciImportError::MalformedReference(reference.to_string());
        let (registry, repository) = match name.split_once('/') {
            Some((host, rest)) if host.contains(['.', ':']) || host == "localhost" => (host, rest),
            _ => (DOCKER_HUB, name),
        };
        // `name:tag@sha256:…` is pinned by its digest, but its tag claims a
        // second, mutable name for the same thing: refused, not ignored.
        if repository.contains(':') {
            return Err(OciImportError::MutableReference(reference.to_string()));
        }
        if !valid_host(registry) || !valid_repository(repository) {
            return Err(malformed());
        }
        let (registry, repository) =
            if matches!(registry, "docker.io" | "index.docker.io" | DOCKER_HUB) {
                let repository = if repository.contains('/') {
                    repository.to_string()
                } else {
                    format!("library/{repository}")
                };
                (DOCKER_HUB.to_string(), repository)
            } else {
                (registry.to_string(), repository.to_string())
            };
        Ok(Self {
            registry,
            repository,
            digest,
        })
    }
}

/// `host[:port]`: a socket address (`[::1]:5000`), or letters, digits, `.`
/// and `-` with an optional numeric port.
fn valid_host(host: &str) -> bool {
    if host.parse::<SocketAddr>().is_ok() {
        return true;
    }
    let (name, port) = host.split_once(':').unwrap_or((host, ""));
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-');
    let port_ok = !host.contains(':')
        || (!port.is_empty() && port.len() <= 5 && port.bytes().all(|byte| byte.is_ascii_digit()));
    name_ok && port_ok
}

/// The distribution spec's repository name: `/`-separated components of
/// lowercase letters and digits, joined within a component by `.`, `_`,
/// `__` or runs of `-`.
fn valid_repository(repository: &str) -> bool {
    fn component(component: &str) -> bool {
        let bytes = component.as_bytes();
        let alnum = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        let separator = |byte: &u8| matches!(byte, b'.' | b'_' | b'-');
        bytes.first().is_some_and(alnum)
            && bytes.last().is_some_and(alnum)
            && bytes.iter().all(|byte| alnum(byte) || separator(byte))
            && !component.contains("..")
            && !component.contains("___")
            && !component.contains("._")
            && !component.contains("_.")
            && !component.contains("-.")
            && !component.contains(".-")
            && !component.contains("-_")
            && !component.contains("_-")
    }
    repository.len() <= 255 && repository.split('/').all(component)
}

/// The authority the plain-HTTP opt-in names, which must be a loopback
/// socket address.
fn plain_http_authority(opt_in: Option<&str>) -> Result<Option<String>, OciImportError> {
    let Some(authority) = opt_in else {
        return Ok(None);
    };
    match authority.parse::<SocketAddr>() {
        Ok(address) if address.ip().is_loopback() => Ok(Some(authority.to_string())),
        _ => Err(OciImportError::InsecureTransport(format!(
            "plain HTTP may only be enabled for a loopback registry (127.0.0.1:<port> or [::1]:<port>), not {authority:?}"
        ))),
    }
}

/// The OCI image layout a pull stages its blobs in.
struct StagedLayout {
    root: PathBuf,
}

impl StagedLayout {
    fn create(root: PathBuf) -> Result<Self, OciImportError> {
        fs::create_dir_all(root.join("blobs/sha256"))
            .map_err(io_error("create the staged image layout"))?;
        fs::write(
            root.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .map_err(io_error("create the staged image layout"))?;
        Ok(Self { root })
    }

    fn path(&self, digest: &str) -> Result<PathBuf, OciImportError> {
        Ok(self.root.join("blobs/sha256").join(sha256_hex(digest)?))
    }

    /// Whether a verified blob is already staged (a manifest may list one
    /// blob twice).
    fn holds(&self, digest: &str) -> Result<bool, OciImportError> {
        Ok(self.path(digest)?.exists())
    }

    /// Stage a verified document.
    fn write(&self, digest: &str, bytes: &[u8]) -> Result<(), OciImportError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path(digest)?)
            .map_err(io_error("stage an image document"))?;
        file.write_all(bytes)
            .map_err(io_error("stage an image document"))
    }
}

/// A token endpoint's reply.
#[derive(Deserialize)]
struct TokenReply {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

/// Whether a connection to `ip` stays off the host's own and its networks'
/// private services: not loopback, unspecified, RFC 1918 private, link-local,
/// shared (CGNAT, `100.64.0.0/10`), IETF-assigned (`192.0.0.0/24`),
/// benchmarking, documentation, multicast, broadcast or reserved; for IPv6
/// also not unique-local (`fc00::/7`), link-local, site-local or
/// documentation, and an IPv4-mapped or NAT64 address is judged by the IPv4
/// address it carries (bead `ley-line-open-b1c18d`).
fn is_public(ip: IpAddr) -> bool {
    fn public_v4(ip: Ipv4Addr) -> bool {
        let [a, b, c, _] = ip.octets();
        !(a == 0
            || ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || (a == 100 && (64..128).contains(&b))
            || (a == 192 && b == 0 && c == 0)
            || (a == 198 && (18..20).contains(&b))
            || ip.is_documentation()
            || a >= 224)
    }
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return public_v4(mapped);
            }
            let segments = ip.segments();
            // NAT64: the well-known prefix (64:ff9b::/96) and the local-use
            // one (64:ff9b:1::/48, RFC 8215, which may translate to private
            // space) carry an IPv4 address in their last 32 bits; so does
            // the deprecated IPv4-compatible form (::a.b.c.d).
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
                || segments[..3] == [0x64, 0xff9b, 1]
                || (segments[..6] == [0, 0, 0, 0, 0, 0]
                    && !ip.is_loopback()
                    && !ip.is_unspecified())
            {
                let [.., a, b, c, d] = ip.octets();
                return public_v4(Ipv4Addr::new(a, b, c, d));
            }
            !(ip.is_unspecified()
                || ip.is_loopback()
                || segments[0] & 0xfe00 == 0xfc00
                || segments[0] & 0xffc0 == 0xfe80
                || segments[0] & 0xffc0 == 0xfec0
                || segments[0] & 0xff00 == 0xff00
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        }
    }
}

/// `(host, port)` a URI reaches, the port defaulted by scheme.
fn endpoint(uri: &Uri) -> Option<(String, u16)> {
    let host = uri.host()?.to_ascii_lowercase();
    let port = uri.port_u16().or(match uri.scheme_str() {
        Some("https") => Some(443),
        Some("http") => Some(80),
        _ => None,
    })?;
    Some((host, port))
}

/// What the pull's connections are held to, shared between the [`Client`]
/// and the connectors ureq calls. One pull runs on one thread, one request
/// at a time.
#[derive(Debug, Default)]
struct Guard {
    /// Endpoints a connection may reach whatever their addresses: the
    /// registry the reference names, and the loopback registry the
    /// plain-HTTP opt-in names. Everything else must resolve to public
    /// addresses only.
    admitted: Vec<(String, u16)>,
    /// Admitted endpoints that have resolved to public addresses only. One
    /// that later resolves to a non-public address is refused: a registry
    /// reached on the internet may not move its own name onto the host's
    /// network between requests.
    seen_public: Vec<(String, u16)>,
    /// When the current request's phase must end: its headers (connecting
    /// and the TLS handshake included), then its body.
    deadline: Option<Instant>,
    /// The non-public address the last refused connection resolved to.
    refused: Option<IpAddr>,
}

impl Guard {
    /// Whether a connection to `endpoint`, resolved to `addresses`, may be
    /// opened; the first non-public address when it may not.
    fn admit(
        &mut self,
        endpoint: Option<(String, u16)>,
        addresses: impl IntoIterator<Item = IpAddr>,
    ) -> Result<(), IpAddr> {
        let non_public = addresses.into_iter().find(|address| !is_public(*address));
        let admitted = endpoint
            .as_ref()
            .is_some_and(|endpoint| self.admitted.contains(endpoint));
        match (non_public, endpoint) {
            (None, Some(endpoint)) if admitted => {
                if !self.seen_public.contains(&endpoint) {
                    self.seen_public.push(endpoint);
                }
                Ok(())
            }
            (None, _) => Ok(()),
            (Some(_), Some(endpoint)) if admitted && !self.seen_public.contains(&endpoint) => {
                Ok(())
            }
            (Some(address), _) => Err(address),
        }
    }
}

fn guard(shared: &Mutex<Guard>) -> std::sync::MutexGuard<'_, Guard> {
    // A poisoned guard only means a panic elsewhere on this thread; its
    // fields are still the policy.
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The first connector of the chain: refuses, before any socket is opened, a
/// connection [`Guard::admit`] does not allow: one to an endpoint that is
/// not admitted and resolves to any non-public address, or to an admitted
/// endpoint that resolved to public addresses before and now does not.
/// Resolution is ureq's, done once per connection, so the addresses checked
/// are the addresses connected to.
#[derive(Debug)]
struct AddressGuard(Arc<Mutex<Guard>>);

impl Connector<()> for AddressGuard {
    type Out = ();

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<()>,
    ) -> Result<Option<()>, ureq::Error> {
        let mut guard = guard(&self.0);
        let addresses = details.addrs.iter().map(|address| address.ip());
        if let Err(address) = guard.admit(endpoint(details.uri), addresses) {
            guard.refused = Some(address);
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "a non-public destination",
            )));
        }
        Ok(chained)
    }
}

/// Wraps the TCP transport so every read and write, the TLS handshake's
/// included, ends at [`Guard::deadline`]. ureq's own phase timeouts do not
/// end there: once a phase deadline has passed it waits up to a second per
/// read instead of failing, so a server sending a header byte at a time
/// would otherwise be bounded only by the 64 KiB header limit.
#[derive(Debug)]
struct DeadlineConnector(Arc<Mutex<Guard>>);

impl<In: Transport> Connector<In> for DeadlineConnector {
    type Out = Deadline<In>;

    fn connect(
        &self,
        _details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| Deadline {
            inner,
            guard: Arc::clone(&self.0),
        }))
    }
}

#[derive(Debug)]
struct Deadline<T> {
    inner: T,
    guard: Arc<Mutex<Guard>>,
}

/// The error a transport returns once [`Guard::deadline`] has passed.
const PAST_DEADLINE: &str = "the response did not arrive within its deadline";

fn past_deadline() -> ureq::Error {
    ureq::Error::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        PAST_DEADLINE,
    ))
}

impl<T> Deadline<T> {
    /// What remains before the deadline, `None` without one.
    fn remaining(&self) -> Option<Duration> {
        guard(&self.guard)
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// `timeout`, shortened to what remains before the deadline, or the
    /// deadline's error once nothing remains.
    fn bound(&self, timeout: NextTimeout) -> Result<NextTimeout, ureq::Error> {
        match self.remaining() {
            None => Ok(timeout),
            Some(remaining) if remaining.is_zero() => Err(past_deadline()),
            // Struct update keeps every other field ureq carries, so a newer
            // ureq that adds fields (3.5 added per_read and per_write) still
            // compiles for a consumer whose resolve picks it.
            Some(remaining) => Ok(NextTimeout {
                after: ureq_time::Duration::Exact(remaining.min(*timeout.after)),
                ..timeout
            }),
        }
    }

    /// The inner transport's result, its timeout reported as the deadline's
    /// once the deadline has passed.
    fn within<R>(&self, result: Result<R, ureq::Error>) -> Result<R, ureq::Error> {
        match result {
            Err(ureq::Error::Timeout(_)) if self.remaining().is_some_and(|left| left.is_zero()) => {
                Err(past_deadline())
            }
            other => other,
        }
    }
}

impl<T: Transport> Transport for Deadline<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        let timeout = self.bound(timeout)?;
        let result = self.inner.transmit_output(amount, timeout);
        self.within(result)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let timeout = self.bound(timeout)?;
        let result = self.inner.await_input(timeout);
        self.within(result)
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

struct Client<'a> {
    agent: ureq::Agent,
    /// `scheme://host[:port]` of the registry.
    origin: String,
    repository: String,
    /// The one authority plain HTTP may reach.
    plain: Option<String>,
    limits: &'a RegistryLimits,
    token: Option<String>,
    downloaded: u64,
    guard: Arc<Mutex<Guard>>,
}

impl<'a> Client<'a> {
    fn new(target: &Target, plain: Option<String>, limits: &'a RegistryLimits) -> Self {
        Self::over(target, plain, limits, TcpConnector::default())
    }

    /// A client whose connections are opened by `tcp`: ureq's TCP connector,
    /// or a test's stand-in for the network.
    fn over<T: Transport>(
        target: &Target,
        plain: Option<String>,
        limits: &'a RegistryLimits,
        tcp: impl Connector<(), Out = T>,
    ) -> Self {
        let scheme = if plain.as_deref() == Some(target.registry.as_str()) {
            "http"
        } else {
            "https"
        };
        let admitted = [
            Some(format!("{scheme}://{}/", target.registry)),
            plain
                .as_ref()
                .map(|authority| format!("http://{authority}/")),
        ]
        .into_iter()
        .flatten()
        .filter_map(|url| url.parse::<Uri>().ok().as_ref().and_then(endpoint))
        .collect();
        let guard = Arc::new(Mutex::new(Guard {
            admitted,
            ..Guard::default()
        }));
        // ureq's default chain without its proxy connectors (no proxy is
        // used), with the address check first and the deadline around the
        // TCP stream, beneath TLS.
        let connector =
            ().chain(AddressGuard(Arc::clone(&guard)))
                .chain(tcp)
                .chain(DeadlineConnector(Arc::clone(&guard)))
                .chain(RustlsConnector::default());
        let config = ureq::Agent::config_builder()
            .https_only(plain.is_none())
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .accept_encoding(ureq::config::AutoHeaderValue::None)
            .user_agent(concat!("leyline-runtime/", env!("CARGO_PKG_VERSION")))
            // ureq bounds each phase by its own timeout and by the one
            // before it, measured from that phase's end. So `send_request`
            // also bounds the wait for the response headers, while
            // `recv_response` is left unset: set, it would also end every
            // body that takes longer than it, a large layer's included.
            // `recv_body` is measured from each read (an idle bound). Once a
            // ureq deadline has passed, ureq waits up to a second per read
            // instead of failing, so the absolute bounds are the guard's:
            // [`Guard::deadline`] on every read and write of the stream, and
            // `Deadlined` on the body.
            .timeout_resolve(Some(limits.connect_timeout))
            .timeout_connect(Some(limits.connect_timeout))
            .timeout_send_request(Some(limits.response_timeout))
            .timeout_recv_body(Some(limits.response_timeout))
            .build();
        let agent = ureq::Agent::with_parts(config, connector, DefaultResolver::default());
        Self {
            agent,
            origin: format!("{scheme}://{}", target.registry),
            repository: target.repository.clone(),
            plain,
            limits,
            token: None,
            downloaded: 0,
            guard,
        }
    }

    /// The bound on one request's headers: resolving, connecting (the TLS
    /// handshake included), sending the request and receiving the headers.
    fn header_timeout(&self) -> Duration {
        self.limits
            .connect_timeout
            .saturating_mul(2)
            .saturating_add(self.limits.response_timeout)
    }

    /// Account for `bytes` more downloaded, or refuse.
    fn reserve(&self, bytes: u64) -> Result<(), OciImportError> {
        match self.downloaded.checked_add(bytes) {
            Some(total) if total <= self.limits.max_download_bytes => Ok(()),
            _ => Err(OciImportError::DownloadTooLarge {
                limit: self.limits.max_download_bytes,
            }),
        }
    }

    /// The deadline for a body of at most `size` bytes.
    fn body_timeout(&self, size: u64) -> Duration {
        let transfer = size / self.limits.min_bytes_per_second.max(1);
        self.limits
            .response_timeout
            .saturating_add(Duration::from_secs(transfer))
    }

    /// Refuse a URL the pull may not reach.
    fn admit(&self, url: &str) -> Result<Uri, OciImportError> {
        let uri: Uri = url
            .parse()
            .map_err(|_| OciImportError::Registry(format!("unusable URL {:?}", redact(url))))?;
        let authority = uri.authority().map(|authority| authority.as_str());
        match (uri.scheme_str(), authority) {
            (Some("https"), Some(_)) => Ok(uri),
            (Some("http"), Some(authority)) if self.plain.as_deref() == Some(authority) => Ok(uri),
            (Some("http"), _) => Err(OciImportError::InsecureTransport(format!(
                "plain HTTP to {} (only HTTPS, or plain HTTP to the one loopback registry named by the insecure opt-in)",
                redact(url)
            ))),
            _ => Err(OciImportError::InsecureTransport(format!(
                "{}: only HTTPS URLs are followed",
                redact(url)
            ))),
        }
    }

    /// GET `url`, following redirects within the limit, answering one
    /// bearer challenge from the registry, and returning the `200` response's
    /// body, held to the deadline for `body_size` bytes from the moment its
    /// request was sent.
    fn get(
        &mut self,
        url: String,
        accept: Option<&str>,
        body_size: u64,
        registry: bool,
    ) -> Result<Deadlined, OciImportError> {
        let mut url = url;
        let mut redirects = 0;
        // The token goes to the registry's own URL and nowhere else: never to
        // a redirect target, never to the token endpoint.
        let mut to_registry = registry;
        let mut challenged = false;
        loop {
            let uri = self.admit(&url)?;
            let deadline = Instant::now() + self.body_timeout(body_size);
            let mut builder = self.agent.get(&url);
            if let Some(accept) = accept {
                builder = builder.header("Accept", accept);
            }
            if to_registry && let Some(token) = &self.token {
                builder = builder.header("Authorization", format!("Bearer {token}"));
            }
            {
                let mut guard = guard(&self.guard);
                guard.refused = None;
                guard.deadline = Some(Instant::now() + self.header_timeout());
            }
            let response =
                builder
                    .call()
                    .map_err(|error| match guard(&self.guard).refused.take() {
                        Some(address) => OciImportError::NonPublicDestination {
                            url: redact(&url).to_string(),
                            address: address.to_string(),
                        },
                        None => OciImportError::Registry(format!("GET {}: {error}", redact(&url))),
                    })?;
            // The headers are in: the body is held to its own deadline.
            guard(&self.guard).deadline = Some(deadline);
            let status = response.status().as_u16();
            match status {
                200 => {
                    return Ok(Deadlined {
                        body: response.into_body().into_reader(),
                        deadline,
                    });
                }
                401 if to_registry && !challenged => {
                    challenged = true;
                    let challenge = response
                        .headers()
                        .get("www-authenticate")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    self.token = Some(self.anonymous_token(&challenge)?);
                }
                301 | 302 | 303 | 307 | 308 => {
                    redirects += 1;
                    if redirects > self.limits.max_redirects {
                        return Err(OciImportError::TooManyRedirects {
                            limit: self.limits.max_redirects,
                        });
                    }
                    let location = response
                        .headers()
                        .get("location")
                        .and_then(|value| value.to_str().ok())
                        .ok_or_else(|| {
                            OciImportError::Registry(format!(
                                "GET {}: a redirect without a Location",
                                redact(&url)
                            ))
                        })?;
                    url = resolve(&uri, location)?;
                    to_registry = false;
                }
                401 => {
                    return Err(OciImportError::Registry(format!(
                        "GET {}: HTTP 401: the registry refused an anonymous pull (credentials are not supported)",
                        redact(&url)
                    )));
                }
                status => {
                    return Err(OciImportError::Registry(format!(
                        "GET {}: HTTP {status}",
                        redact(&url)
                    )));
                }
            }
        }
    }

    /// Answer a bearer challenge with an anonymous token for this
    /// repository's pull scope.
    fn anonymous_token(&mut self, challenge: &str) -> Result<String, OciImportError> {
        let refuse = |why: &str| {
            OciImportError::Registry(format!(
                "the registry's authentication challenge {why} (only anonymous bearer tokens are supported)"
            ))
        };
        let (scheme, parameters) = challenge.split_once(' ').unwrap_or((challenge, ""));
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(refuse(&format!("is {scheme:?}, not Bearer")));
        }
        let parameters = challenge_parameters(parameters);
        let realm = parameters
            .iter()
            .find(|(name, _)| name == "realm")
            .map(|(_, value)| value.as_str())
            .ok_or_else(|| refuse("names no realm"))?;
        let mut url = format!(
            "{realm}{}scope={}",
            if realm.contains('?') { '&' } else { '?' },
            percent_encode(&format!("repository:{}:pull", self.repository))
        );
        if let Some((_, service)) = parameters.iter().find(|(name, _)| name == "service") {
            url.push_str(&format!("&service={}", percent_encode(service)));
        }
        let body = self.get(url, Some("application/json"), MAX_TOKEN_BYTES, false)?;
        let mut bytes = Vec::new();
        body.take(MAX_TOKEN_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| OciImportError::Registry(format!("read a token reply: {error}")))?;
        if bytes.len() as u64 > MAX_TOKEN_BYTES {
            return Err(OciImportError::Registry(format!(
                "the token reply is larger than {MAX_TOKEN_BYTES} bytes"
            )));
        }
        let reply: TokenReply = serde_json::from_slice(&bytes)
            .map_err(|_| OciImportError::Registry("the token reply is not a token".into()))?;
        reply
            .token
            .or(reply.access_token)
            .filter(|token| !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()))
            .ok_or_else(|| OciImportError::Registry("the token reply holds no usable token".into()))
    }

    /// An index or manifest, read whole, counted, and verified by the
    /// layout import's own check before it is returned.
    fn document(
        &mut self,
        digest: &str,
        size: Option<u64>,
        accept: &str,
    ) -> Result<Vec<u8>, OciImportError> {
        // An index entry's digest is the registry's to choose: well formed
        // before it is part of a URL.
        sha256_hex(digest)?;
        let url = format!("{}/v2/{}/manifests/{digest}", self.origin, self.repository);
        let body = self.get(url, Some(accept), MAX_DOCUMENT_BYTES, true)?;
        let budget = self
            .limits
            .max_download_bytes
            .saturating_sub(self.downloaded);
        let bound = budget.min(MAX_DOCUMENT_BYTES);
        let mut bytes = Vec::new();
        body.take(bound + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| OciImportError::Registry(format!("read {digest}: {error}")))?;
        let read = bytes.len() as u64;
        if read > budget {
            return Err(OciImportError::DownloadTooLarge {
                limit: self.limits.max_download_bytes,
            });
        }
        if read > MAX_DOCUMENT_BYTES {
            return Err(OciImportError::Unsupported(format!(
                "document {digest} is larger than {MAX_DOCUMENT_BYTES} bytes"
            )));
        }
        self.downloaded += read;
        check_blob(digest, size, read, &Sha256::digest(&bytes))?;
        Ok(bytes)
    }

    /// Stream a blob into the staged layout, at most one byte past its
    /// descriptor's size, verified before it is kept.
    fn blob(
        &mut self,
        digest: &str,
        size: u64,
        layout: &StagedLayout,
    ) -> Result<(), OciImportError> {
        let destination = layout.path(digest)?;
        let partial = destination.with_extension("partial");
        let url = format!("{}/v2/{}/blobs/{digest}", self.origin, self.repository);
        let mut reader = self
            .get(url, None, size, true)?
            .take(size.saturating_add(1));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .map_err(io_error("stage an image blob"))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 16];
        let mut read_total = 0u64;
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| OciImportError::Registry(format!("read {digest}: {error}")))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(io_error("stage an image blob"))?;
            read_total += read as u64;
        }
        // Counted when its size was reserved, before any blob was requested.
        check_blob(digest, Some(size), read_total, &hasher.finalize())?;
        fs::rename(&partial, &destination).map_err(io_error("stage an image blob"))
    }
}

/// A response body that fails once its deadline passes. ureq's own timeouts
/// cannot be that deadline: its body timeout restarts at every read, and a
/// phase deadline that has passed becomes a one-second wait per read.
struct Deadlined {
    body: ureq::BodyReader<'static>,
    deadline: Instant,
}

impl Read for Deadlined {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.body.read(buffer)?;
        // How far past the deadline: zero until it passes.
        if Instant::now().saturating_duration_since(self.deadline) > Duration::ZERO {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the response did not arrive within its deadline",
            ));
        }
        Ok(read)
    }
}

/// `auth-param`s of a challenge: `name="quoted, value"` or `name=token`,
/// comma separated. Names are lowercased; malformed pairs are skipped.
fn challenge_parameters(parameters: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = parameters.trim_start();
    while let Some((name, after)) = rest.split_once('=') {
        let name = name.trim().to_ascii_lowercase();
        let (value, after) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((value, after)) => (value.to_string(), after),
                None => break,
            }
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim().to_string(), &after[end..])
        };
        out.push((name, value));
        rest = after.trim_start().trim_start_matches(',').trim_start();
    }
    out
}

/// Percent-encode everything but RFC 3986 unreserved characters.
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// A redirect's `Location`, absolute or origin-relative, as an absolute URL.
fn resolve(base: &Uri, location: &str) -> Result<String, OciImportError> {
    if location.contains("://") {
        return Ok(location.to_string());
    }
    match (
        location.strip_prefix('/'),
        base.scheme_str(),
        base.authority(),
    ) {
        (Some(path), Some(scheme), Some(authority)) if !path.starts_with('/') => {
            Ok(format!("{scheme}://{authority}/{path}"))
        }
        _ => Err(OciImportError::Registry(format!(
            "unsupported redirect location {:?}",
            redact(location)
        ))),
    }
}

/// A URL without its query or fragment: what an error may name.
fn redact(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "c567b231f65614688711a75f31518a064258a51d442117a3e771a3a8fdba78ef";

    fn target(reference: &str) -> Result<Target, OciImportError> {
        Target::parse(reference)
    }

    fn pinned(registry: &str, repository: &str) -> Target {
        Target {
            registry: registry.into(),
            repository: repository.into(),
            digest: format!("sha256:{HEX}"),
        }
    }

    #[test]
    fn docker_hub_names_are_normalized() {
        for (reference, repository) in [
            ("hello-world", "library/hello-world"),
            ("docker.io/hello-world", "library/hello-world"),
            ("index.docker.io/library/hello-world", "library/hello-world"),
            ("registry-1.docker.io/hello-world", "library/hello-world"),
            ("cgr/static", "cgr/static"),
            ("docker.io/cgr/static", "cgr/static"),
        ] {
            assert_eq!(
                target(&format!("{reference}@sha256:{HEX}")).unwrap(),
                pinned(DOCKER_HUB, repository),
                "{reference}"
            );
        }
    }

    #[test]
    fn a_first_component_with_a_dot_a_port_or_localhost_is_the_registry() {
        for (reference, registry, repository) in [
            ("ghcr.io/owner/image", "ghcr.io", "owner/image"),
            ("127.0.0.1:5000/a/b", "127.0.0.1:5000", "a/b"),
            ("localhost/a", "localhost", "a"),
            ("registry:5000/a", "registry:5000", "a"),
            ("[::1]:5000/a", "[::1]:5000", "a"),
        ] {
            assert_eq!(
                target(&format!("{reference}@sha256:{HEX}")).unwrap(),
                pinned(registry, repository),
                "{reference}"
            );
        }
    }

    #[test]
    fn tags_and_bare_digests_are_refused() {
        for reference in [
            "alpine:3.20".to_string(),
            "alpine".to_string(),
            "ghcr.io/a/b:v1".to_string(),
            format!("ghcr.io/a/b:v1@sha256:{HEX}"),
            format!("b:v1@sha256:{HEX}"),
        ] {
            assert!(
                matches!(target(&reference), Err(OciImportError::MutableReference(_))),
                "{reference}"
            );
        }
        assert!(matches!(
            target(&format!("sha256:{HEX}")),
            Err(OciImportError::MalformedReference(_))
        ));
        assert!(matches!(
            target("ghcr.io/a@sha256:ABC"),
            Err(OciImportError::MalformedDigest(_))
        ));
    }

    #[test]
    fn malformed_names_are_refused() {
        for name in [
            "ghcr.io/Owner/image",
            "ghcr.io/a//b",
            "ghcr.io/",
            "ghcr.io/a/-b",
            "ghcr.io/a/b-",
            "ghcr.io/a/b..c",
            "ghcr.io/a/b___c",
            "ghcr.io/a/b._c",
            "ghcr.io/a/b_.c",
            "ghcr.io/a/b-.c",
            "ghcr.io/a/b.-c",
            "ghcr.io/a/b-_c",
            "ghcr.io/a/b_-c",
            "ghcr.io/a/b c",
            "ghcr.io/a/b%c",
            "gh_cr.io/a",
            "host:/a",
            "host:123456/a",
            "host:12a/a",
            "",
        ] {
            assert!(
                matches!(
                    target(&format!("{name}@sha256:{HEX}")),
                    Err(OciImportError::MalformedReference(_))
                ),
                "{name:?}"
            );
        }
        let long = format!("ghcr.io/{}@sha256:{HEX}", "a".repeat(256));
        assert!(matches!(
            target(&long),
            Err(OciImportError::MalformedReference(_))
        ));
        let longest = format!("ghcr.io/{}@sha256:{HEX}", "a".repeat(255));
        assert!(target(&longest).is_ok());
        for name in [
            "ghcr.io/a/b.c",
            "ghcr.io/a/b_c",
            "ghcr.io/a/b__c",
            "ghcr.io/a/b--c",
            "ghcr.io/a/b-c",
            "host:1/a",
            "host:65535/a",
            "host-1.example/a",
        ] {
            assert!(target(&format!("{name}@sha256:{HEX}")).is_ok(), "{name}");
        }
    }

    #[test]
    fn the_plain_http_opt_in_admits_loopback_addresses_only() {
        assert_eq!(plain_http_authority(None).unwrap(), None);
        for loopback in ["127.0.0.1:5000", "127.1.2.3:80", "[::1]:5000"] {
            assert_eq!(
                plain_http_authority(Some(loopback)).unwrap().as_deref(),
                Some(loopback)
            );
        }
        for other in [
            "localhost:5000",
            "10.0.0.1:5000",
            "[::2]:5000",
            "127.0.0.1",
            "ghcr.io:443",
        ] {
            assert!(
                matches!(
                    plain_http_authority(Some(other)),
                    Err(OciImportError::InsecureTransport(_))
                ),
                "{other}"
            );
        }
    }

    fn client<'a>(plain: Option<&str>, limits: &'a RegistryLimits) -> Client<'a> {
        Client::new(
            &pinned("127.0.0.1:5000", "a"),
            plain.map(str::to_string),
            limits,
        )
    }

    #[test]
    fn the_scheme_follows_the_opt_in() {
        let limits = RegistryLimits::default();
        assert_eq!(client(None, &limits).origin, "https://127.0.0.1:5000");
        assert_eq!(
            client(Some("127.0.0.1:5000"), &limits).origin,
            "http://127.0.0.1:5000"
        );
        assert_eq!(
            client(Some("127.0.0.1:5001"), &limits).origin,
            "https://127.0.0.1:5000"
        );
    }

    #[test]
    fn only_https_and_the_opted_in_plain_authority_are_admitted() {
        let limits = RegistryLimits::default();
        let strict = client(None, &limits);
        assert!(strict.admit("https://ghcr.io/v2/").is_ok());
        for refused in ["http://127.0.0.1:5000/v2/", "ftp://ghcr.io/x", "/relative"] {
            assert!(
                matches!(
                    strict.admit(refused),
                    Err(OciImportError::InsecureTransport(_))
                ),
                "{refused}"
            );
        }
        assert!(matches!(
            strict.admit("not a url"),
            Err(OciImportError::Registry(_))
        ));
        // The refusal names what was refused.
        let plain = strict
            .admit("http://127.0.0.1:5000/v2/?q=secret")
            .unwrap_err();
        assert!(
            plain
                .to_string()
                .contains("plain HTTP to http://127.0.0.1:5000/v2/ "),
            "{plain}"
        );
        assert!(!plain.to_string().contains("secret"), "{plain}");
        let other = strict.admit("ftp://ghcr.io/x").unwrap_err();
        assert!(other.to_string().contains("only HTTPS URLs"), "{other}");
        let local = client(Some("127.0.0.1:5000"), &limits);
        assert!(local.admit("http://127.0.0.1:5000/v2/").is_ok());
        assert!(local.admit("https://cdn.example/x").is_ok());
        assert!(matches!(
            local.admit("http://127.0.0.1:5001/v2/"),
            Err(OciImportError::InsecureTransport(_))
        ));
    }

    #[test]
    fn the_body_deadline_grows_with_the_size_at_the_minimum_rate() {
        let limits = RegistryLimits {
            response_timeout: Duration::from_secs(7),
            min_bytes_per_second: 100,
            ..RegistryLimits::default()
        };
        let client = client(None, &limits);
        assert_eq!(client.body_timeout(0), Duration::from_secs(7));
        assert_eq!(client.body_timeout(1_000), Duration::from_secs(17));
        assert_eq!(client.body_timeout(1_099), Duration::from_secs(17));
        let zero = RegistryLimits {
            min_bytes_per_second: 0,
            ..limits
        };
        assert_eq!(
            super::Client::new(&pinned("h.io", "a"), None, &zero).body_timeout(5),
            Duration::from_secs(12)
        );
    }

    #[test]
    fn reservations_are_held_to_the_download_limit() {
        let limits = RegistryLimits {
            max_download_bytes: 100,
            ..RegistryLimits::default()
        };
        let mut client = client(None, &limits);
        client.downloaded = 40;
        assert!(client.reserve(60).is_ok());
        assert!(matches!(
            client.reserve(61),
            Err(OciImportError::DownloadTooLarge { limit: 100 })
        ));
        assert!(matches!(
            client.reserve(u64::MAX),
            Err(OciImportError::DownloadTooLarge { limit: 100 })
        ));
    }

    #[test]
    fn challenge_parameters_are_parsed() {
        assert_eq!(
            challenge_parameters(
                r#"realm="https://ghcr.io/token",service="ghcr.io",scope="repository:a/b:pull,push""#
            ),
            vec![
                ("realm".into(), "https://ghcr.io/token".into()),
                ("service".into(), "ghcr.io".into()),
                ("scope".into(), "repository:a/b:pull,push".into()),
            ]
        );
        assert_eq!(
            challenge_parameters(
                r#" Realm=https://auth.docker.io/token , service="registry.docker.io""#
            ),
            vec![
                ("realm".into(), "https://auth.docker.io/token".into()),
                ("service".into(), "registry.docker.io".into()),
            ]
        );
        assert_eq!(
            challenge_parameters(r#"realm="unterminated"#),
            Vec::<(String, String)>::new()
        );
        assert_eq!(challenge_parameters(""), Vec::<(String, String)>::new());
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(
            percent_encode("repository:library/hello-world:pull"),
            "repository%3Alibrary%2Fhello-world%3Apull"
        );
        assert_eq!(percent_encode("a-b.c_d~e Z9"), "a-b.c_d~e%20Z9");
    }

    #[test]
    fn redirect_locations_resolve_against_the_request() {
        let base: Uri = "https://ghcr.io/v2/a/blobs/x?q=1".parse().unwrap();
        assert_eq!(
            resolve(&base, "https://cdn.example/b?sig=s").unwrap(),
            "https://cdn.example/b?sig=s"
        );
        assert_eq!(resolve(&base, "/c/d").unwrap(), "https://ghcr.io/c/d");
        for unsupported in ["relative/path", "//cdn.example/b", ""] {
            assert!(
                matches!(
                    resolve(&base, unsupported),
                    Err(OciImportError::Registry(_))
                ),
                "{unsupported:?}"
            );
        }
    }

    #[test]
    fn only_public_addresses_are_public() {
        let non_public = [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.8",
            "198.18.0.1",
            "198.19.255.255",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "64:ff9b::a00:1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::a00:1",
            "64:ff9b:1:2::c0a8:101",
            "::a00:1",
            "::7f00:1",
        ];
        for address in non_public {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "11.0.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "126.255.255.255",
            "128.0.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.0.1.1",
            "192.169.0.1",
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.255",
            "2606:4700:4700::1111",
            "2001:db9::1",
            "2002::1",
            "fbff::1",
            "fe00::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "64:ff9c::a00:1",
            "64:ff9b:1::808:808",
            "64:ff9b:2::a00:1",
            "::808:808",
        ];
        for address in public {
            assert!(is_public(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn an_endpoint_defaults_its_port_by_scheme() {
        let endpoint_of = |url: &str| endpoint(&url.parse::<Uri>().unwrap());
        assert_eq!(
            endpoint_of("https://GHCR.io/v2/"),
            Some(("ghcr.io".into(), 443))
        );
        assert_eq!(
            endpoint_of("http://127.0.0.1/x"),
            Some(("127.0.0.1".into(), 80))
        );
        assert_eq!(
            endpoint_of("https://127.0.0.1:5000/"),
            Some(("127.0.0.1".into(), 5000))
        );
        assert_eq!(endpoint_of("ftp://h.io/"), None);
    }

    #[test]
    fn the_registry_and_the_opt_in_are_the_admitted_endpoints() {
        let limits = RegistryLimits::default();
        let client = Client::new(&pinned("ghcr.io", "a"), None, &limits);
        assert_eq!(
            guard(&client.guard).admitted,
            [("ghcr.io".to_string(), 443)]
        );
        let client = Client::new(
            &pinned("127.0.0.1:5000", "a"),
            Some("127.0.0.1:5000".into()),
            &limits,
        );
        assert_eq!(
            guard(&client.guard).admitted,
            [
                ("127.0.0.1".to_string(), 5000),
                ("127.0.0.1".to_string(), 5000)
            ]
        );
        assert_eq!(
            client.header_timeout(),
            limits.connect_timeout * 2 + limits.response_timeout
        );
    }

    /// A transport that always makes progress: one byte per wait, whatever
    /// the timeout, as a server trickling bytes inside ureq's one-second
    /// per-read fallback looks to ureq. `timeout_after` makes it report a
    /// timeout instead once it has been waited on that often.
    #[derive(Debug)]
    struct Trickle {
        buffers: ureq::unversioned::transport::LazyBuffers,
        waits: usize,
        timeout_after: Option<usize>,
        last_timeout: Option<Duration>,
    }

    impl Trickle {
        fn new(timeout_after: Option<usize>) -> Self {
            Self {
                buffers: ureq::unversioned::transport::LazyBuffers::new(1 << 16, 1 << 10),
                waits: 0,
                timeout_after,
                last_timeout: None,
            }
        }
    }

    impl Transport for Trickle {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(&mut self, _: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
            self.last_timeout = Some(*timeout.after);
            Ok(())
        }

        fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
            self.last_timeout = Some(*timeout.after);
            self.waits += 1;
            if self.timeout_after.is_some_and(|after| self.waits > after) {
                // A real transport times out once its wait has elapsed.
                let until = Instant::now() + *timeout.after;
                while Instant::now() < until {
                    std::thread::yield_now();
                }
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            self.buffers.input_append_buf()[0] = b'a';
            self.buffers.input_appended(1);
            self.buffers.input_consume(1);
            Ok(true)
        }

        fn is_open(&mut self) -> bool {
            self.waits % 2 == 0
        }

        fn is_tls(&self) -> bool {
            self.timeout_after.is_some()
        }
    }

    fn one_second() -> NextTimeout {
        NextTimeout {
            after: ureq_time::Duration::from_secs(1),
            reason: ureq::Timeout::RecvResponse,
        }
    }

    fn deadline_over(inner: Trickle, deadline: Option<Instant>) -> Deadline<Trickle> {
        Deadline {
            inner,
            guard: Arc::new(Mutex::new(Guard {
                deadline,
                ..Guard::default()
            })),
        }
    }

    fn is_past_deadline(error: &ureq::Error) -> bool {
        matches!(error, ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut && io.to_string() == PAST_DEADLINE)
    }

    /// The deterministic form of the header-trickle bound: a transport that
    /// always makes progress is cut at the deadline, not by its own pace.
    #[test]
    fn a_transport_that_always_progresses_ends_at_the_deadline() {
        let started = Instant::now();
        let mut transport = deadline_over(
            Trickle::new(None),
            Some(started + Duration::from_millis(50)),
        );
        let error = loop {
            match transport.await_input(one_second()) {
                Ok(progress) => assert!(progress),
                Err(error) => break error,
            }
            assert!(started.elapsed() < Duration::from_secs(2), "never ended");
        };
        assert!(is_past_deadline(&error), "{error}");
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(transport.inner.waits > 1);
        // A write past the deadline is refused the same way.
        let error = transport.transmit_output(0, one_second()).unwrap_err();
        assert!(is_past_deadline(&error), "{error}");

        // Without a deadline the same transport is not cut.
        let mut unbounded = deadline_over(Trickle::new(None), None);
        for _ in 0..40 {
            assert!(unbounded.await_input(one_second()).unwrap());
        }
        assert_eq!(unbounded.inner.last_timeout, Some(Duration::from_secs(1)));
    }

    #[test]
    fn a_wait_is_shortened_to_the_deadline_and_its_timeout_named_as_the_deadline() {
        // Far deadline: the wait keeps ureq's own timeout, and a timeout the
        // inner transport reports stays ureq's.
        let mut far = deadline_over(
            Trickle::new(Some(0)),
            Some(Instant::now() + Duration::from_secs(60)),
        );
        let error = far.await_input(one_second()).unwrap_err();
        assert!(matches!(error, ureq::Error::Timeout(_)), "{error}");
        assert_eq!(far.inner.last_timeout, Some(Duration::from_secs(1)));
        far.transmit_output(0, one_second()).unwrap();
        assert_eq!(far.inner.last_timeout, Some(Duration::from_secs(1)));

        // Near deadline: the wait is at most what remains, and the inner
        // timeout is the deadline's.
        let mut near = deadline_over(
            Trickle::new(Some(0)),
            Some(Instant::now() + Duration::from_millis(100)),
        );
        let error = near.await_input(one_second()).unwrap_err();
        assert!(is_past_deadline(&error), "{error}");
        let waited = near.inner.last_timeout.unwrap();
        assert!(waited <= Duration::from_millis(100), "{waited:?}");
        // Past the deadline, a write is refused before the inner transport.
        let error = near.transmit_output(0, one_second()).unwrap_err();
        assert!(is_past_deadline(&error), "{error}");

        // The wrapper reports the inner transport's state as it is.
        let mut wrapped = deadline_over(Trickle::new(None), None);
        assert!(wrapped.is_open() && !wrapped.is_tls());
        wrapped.await_input(one_second()).unwrap();
        assert!(!wrapped.is_open());
        assert!(near.is_tls());
    }

    /// Review of b1c18d (N1): the registry and the opt-in are admitted
    /// whatever they resolve to, but a registry that has resolved to public
    /// addresses may not move its name onto a non-public one afterwards.
    #[test]
    fn the_guard_admits_public_addresses_and_its_own_endpoints_without_rebinding() {
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        let (public, private, loopback) = (ip("1.1.1.1"), ip("10.0.0.1"), ip("127.0.0.1"));
        let registry = ("r.example".to_string(), 443);
        let cdn = ("cdn.example".to_string(), 443);
        let mut guard = Guard {
            admitted: vec![registry.clone()],
            ..Guard::default()
        };
        // Not admitted: public addresses only, the first other one named.
        assert_eq!(guard.admit(Some(cdn.clone()), [public]), Ok(()));
        assert_eq!(
            guard.admit(Some(cdn.clone()), [public, private]),
            Err(private)
        );
        assert_eq!(
            guard.admit(Some(cdn.clone()), [loopback, private]),
            Err(loopback)
        );
        assert_eq!(guard.admit(None, [private]), Err(private));
        assert_eq!(guard.admit(None, [public]), Ok(()));
        assert_eq!(guard.admit(Some(cdn), []), Ok(()));
        // Another port of the registry's host is another endpoint.
        assert_eq!(
            guard.admit(Some(("r.example".to_string(), 8443)), [private]),
            Err(private)
        );
        assert!(guard.seen_public.is_empty());
        // A registry on a private network stays admitted.
        for _ in 0..2 {
            assert_eq!(guard.admit(Some(registry.clone()), [private]), Ok(()));
        }
        assert!(guard.seen_public.is_empty());
        // Once it has resolved to public addresses only, it may not move.
        for _ in 0..2 {
            assert_eq!(guard.admit(Some(registry.clone()), [public]), Ok(()));
        }
        assert_eq!(guard.seen_public, [registry.clone()]);
        assert_eq!(
            guard.admit(Some(registry.clone()), [public, loopback]),
            Err(loopback)
        );
        assert_eq!(guard.admit(Some(registry.clone()), [private]), Err(private));
        assert_eq!(guard.admit(Some(registry), [public]), Ok(()));
    }

    /// A network stand-in whose connections answer every request with
    /// response headers that never end: one byte per millisecond, each well
    /// inside any wait ureq asks for, so ureq's own timeouts never fire.
    #[derive(Debug)]
    struct EndlessHeaders;

    #[derive(Debug)]
    struct EndlessHeaderStream {
        buffers: ureq::unversioned::transport::LazyBuffers,
        sent: usize,
    }

    impl Connector<()> for EndlessHeaders {
        type Out = EndlessHeaderStream;

        fn connect(
            &self,
            _: &ConnectionDetails,
            _: Option<()>,
        ) -> Result<Option<Self::Out>, ureq::Error> {
            Ok(Some(EndlessHeaderStream {
                buffers: ureq::unversioned::transport::LazyBuffers::new(1 << 16, 1 << 16),
                sent: 0,
            }))
        }
    }

    impl Transport for EndlessHeaderStream {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(&mut self, _: usize, _: NextTimeout) -> Result<(), ureq::Error> {
            Ok(())
        }

        fn await_input(&mut self, _: NextTimeout) -> Result<bool, ureq::Error> {
            const START: &[u8] = b"HTTP/1.1 200 OK\r\nX-Trickle: ";
            let until = Instant::now() + Duration::from_millis(1);
            while Instant::now() < until {
                std::thread::yield_now();
            }
            let byte = START.get(self.sent).copied().unwrap_or(b'a');
            self.sent += 1;
            self.buffers.input_append_buf()[0] = byte;
            self.buffers.input_appended(1);
            Ok(true)
        }

        fn is_open(&mut self) -> bool {
            true
        }
    }

    /// Review of b1c18d (N5), the deterministic form of the header-trickle
    /// bound through the whole connector chain and a real ureq call: with
    /// the deadline connector in the chain the request ends at the header
    /// deadline; without it, ureq would read on for a minute, until its
    /// 64 KiB header limit.
    #[test]
    fn a_header_trickle_through_the_connector_chain_ends_at_the_header_deadline() {
        let limits = RegistryLimits {
            connect_timeout: Duration::from_millis(100),
            response_timeout: Duration::from_millis(100),
            ..RegistryLimits::default()
        };
        let authority = "127.0.0.1:9";
        let mut client = Client::over(
            &pinned(authority, "a"),
            Some(authority.to_string()),
            &limits,
            EndlessHeaders,
        );
        let started = Instant::now();
        let error = match client.get(format!("http://{authority}/v2/"), None, 0, true) {
            Ok(_) => panic!("headers that never end are no response"),
            Err(error) => error,
        };
        let elapsed = started.elapsed();
        assert!(
            matches!(&error, OciImportError::Registry(detail) if detail.contains(PAST_DEADLINE)),
            "{error}"
        );
        assert!(
            elapsed >= client.header_timeout() && elapsed < Duration::from_secs(5),
            "{elapsed:?}"
        );
    }

    #[test]
    fn redaction_drops_queries_and_fragments() {
        assert_eq!(
            redact("https://cdn.example/b?X-Signature=s"),
            "https://cdn.example/b"
        );
        assert_eq!(redact("https://cdn.example/b#f"), "https://cdn.example/b");
        assert_eq!(redact("https://cdn.example/b"), "https://cdn.example/b");
    }
}
