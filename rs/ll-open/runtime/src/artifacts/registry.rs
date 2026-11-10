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
//!   timeout, the time to a response's headers, and a body deadline of that
//!   time plus the descriptor's size at a minimum transfer rate.
//!
//! [OCI distribution API]: https://github.com/opencontainers/distribution-spec/blob/v1.1.0/spec.md

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use ureq::http::Uri;

use super::oci::{
    DEFAULT_MAX_ROOTFS_BYTES, Document, INDEX_TYPES, ImportedRootfs, Index, MANIFEST_TYPES,
    MAX_DOCUMENT_BYTES, Manifest, OciImport, OciImportError, Platform, SymlinkPolicy, check_blob,
    classify, import_oci_layout, parse, parse_reference, select_platform, sha256_hex,
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
    /// Bound on sending a request and receiving its response headers, and
    /// the fixed part of every body's deadline.
    pub response_timeout: Duration,
    /// The slowest transfer accepted: a body of `n` bytes must arrive within
    /// `response_timeout + n / min_bytes_per_second`.
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
    let top = client.document("manifests", &target.digest, None, &manifest_accept)?;
    layout.write(&target.digest, &top)?;
    let manifest_bytes = match classify(&top)? {
        Document::Index => {
            let index: Index = parse(&top, "image index")?;
            let selected = select_platform(&index, &request.platform)?;
            let bytes = client.document(
                "manifests",
                &selected.digest,
                Some(selected.size),
                &manifest_accept,
            )?;
            layout.write(&selected.digest, &bytes)?;
            bytes
        }
        Document::Manifest => top,
    };
    let manifest: Manifest = parse(&manifest_bytes, "image manifest")?;
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
}

impl<'a> Client<'a> {
    fn new(target: &Target, plain: Option<String>, limits: &'a RegistryLimits) -> Self {
        let scheme = if plain.as_deref() == Some(target.registry.as_str()) {
            "http"
        } else {
            "https"
        };
        let agent = ureq::Agent::config_builder()
            .https_only(plain.is_none())
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .accept_encoding(ureq::config::AutoHeaderValue::None)
            .user_agent(concat!("leyline-runtime/", env!("CARGO_PKG_VERSION")))
            .timeout_resolve(Some(limits.connect_timeout))
            .timeout_connect(Some(limits.connect_timeout))
            .timeout_send_request(Some(limits.response_timeout))
            .timeout_recv_response(Some(limits.response_timeout))
            .build()
            .new_agent();
        Self {
            agent,
            origin: format!("{scheme}://{}", target.registry),
            repository: target.repository.clone(),
            plain,
            limits,
            token: None,
            downloaded: 0,
        }
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
    /// bearer challenge from the registry, and returning the `200` response.
    fn get(
        &mut self,
        url: String,
        accept: Option<&str>,
        body_size: u64,
        registry: bool,
    ) -> Result<ureq::http::Response<ureq::Body>, OciImportError> {
        let mut url = url;
        let mut redirects = 0;
        // The token goes to the registry's own URL and nowhere else: never to
        // a redirect target, never to the token endpoint.
        let mut to_registry = registry;
        let mut challenged = false;
        loop {
            let uri = self.admit(&url)?;
            let mut builder = self.agent.get(&url);
            if let Some(accept) = accept {
                builder = builder.header("Accept", accept);
            }
            if to_registry && let Some(token) = &self.token {
                builder = builder.header("Authorization", format!("Bearer {token}"));
            }
            let response = builder
                .config()
                .timeout_recv_body(Some(self.body_timeout(body_size)))
                .build()
                .call()
                .map_err(|error| {
                    OciImportError::Registry(format!("GET {}: {error}", redact(&url)))
                })?;
            let status = response.status().as_u16();
            match status {
                200 => return Ok(response),
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
        let response = self.get(url, Some("application/json"), MAX_TOKEN_BYTES, false)?;
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_TOKEN_BYTES + 1)
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

    /// A document (`manifests`) or config-sized blob, read whole, counted,
    /// and verified by the layout import's own check before it is returned.
    fn document(
        &mut self,
        kind: &str,
        digest: &str,
        size: Option<u64>,
        accept: &str,
    ) -> Result<Vec<u8>, OciImportError> {
        let url = format!("{}/v2/{}/{kind}/{digest}", self.origin, self.repository);
        let response = self.get(url, Some(accept), MAX_DOCUMENT_BYTES, true)?;
        let budget = self
            .limits
            .max_download_bytes
            .saturating_sub(self.downloaded);
        let bound = budget.min(MAX_DOCUMENT_BYTES);
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(bound + 1)
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
        let response = self.get(url, None, size, true)?;
        let mut reader = response
            .into_body()
            .into_reader()
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
        check_blob(digest, Some(size), read_total, &hasher.finalize())?;
        self.downloaded += read_total;
        fs::rename(&partial, &destination).map_err(io_error("stage an image blob"))
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
    fn redaction_drops_queries_and_fragments() {
        assert_eq!(
            redact("https://cdn.example/b?X-Signature=s"),
            "https://cdn.example/b"
        );
        assert_eq!(redact("https://cdn.example/b#f"), "https://cdn.example/b");
        assert_eq!(redact("https://cdn.example/b"), "https://cdn.example/b");
    }
}
