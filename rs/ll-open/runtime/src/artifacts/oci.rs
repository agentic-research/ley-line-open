//! OCI image layout → authenticated CAS rootfs (bead `ley-line-open-7736ab`).
//!
//! [`import_oci_layout`] reads an [OCI image layout] — what `skopeo copy
//! … oci:DIR`, `docker buildx build --output type=oci`, `crane`, `buildah
//! push … oci:DIR` and `podman save --format oci-dir` write — and publishes
//! the image's root filesystem into a CAS root as the content-addressed
//! rootfs `backends::libkrun::plan::DirectoryRootfsResolver` and the native
//! tier already verify and consume:
//!
//! ```text
//! <cas-root>/<blake3 hex>/rootfs.manifest.json   {"version":1,"files":[{"path","mode","blake3"},…]}
//! <cas-root>/<blake3 hex>/<every file it lists>
//! ```
//!
//! The rootfs identity is the existing one: BLAKE3 over the exact bytes of
//! `rootfs.manifest.json` (σ_bytes, ADR-0047). The importer adds no identity
//! construction, and the image's SHA-256 digests (OCI's domain) are never
//! compared with it; they are reported only as provenance.
//!
//! The contract, each clause tested in `tests/oci_rootfs.rs`:
//!
//! - **Pinned input only.** The reference is `sha256:<hex>` (or
//!   `name@sha256:<hex>`, where the name is a label). A tag is refused; the
//!   layout's `index.json`, whose tags are mutable, is never read.
//! - **Every byte authenticated.** Each blob is read from
//!   `blobs/sha256/<hex>` into private staging and must hash to its digest
//!   and match its descriptor's size before it is parsed. Each layer's
//!   uncompressed stream must hash to the config's `diff_id`.
//! - **Platform.** An index selects the one entry for the requested
//!   platform (`arm64` without a variant is `arm64/v8`); none or several is
//!   an error. The image config must agree with the requested platform.
//! - **Layer semantics.** Layers apply in order to an in-memory tree; a
//!   layer's whiteouts (`.wh.<name>`) and opaque whiteouts (`.wh..wh..opq`)
//!   remove lower-layer entries only, never ones the same layer adds.
//! - **What rootfs manifest v1 can express.** Regular files with their
//!   permission bits. Directories are written `0755` and only where a file
//!   needs them: neither a directory's mode, an empty directory, ownership,
//!   timestamps nor extended attributes is in the manifest, so none is
//!   carried, and two imports with the same identity are the same tree on
//!   disk. Setuid, setgid and sticky bits are cleared (and counted): the tree
//!   is written by an unprivileged host user, and a host file carrying them
//!   is a hazard the manifest has no reason to admit. A file its owner cannot
//!   read, a device node, a FIFO, a sparse file or an aufs `.wh..wh.` entry
//!   is refused by name. A hard link becomes a second regular file with its
//!   target's content and mode.
//! - **Symbolic links.** The manifest admits none, so [`SymlinkPolicy`]
//!   decides: `Refuse` (the default) names the first link; `Resolve` replaces
//!   each link with a copy of the regular file or directory it resolves to
//!   inside the image, absolute targets meaning the image's root, never the
//!   host's. A dangling link, a loop, a link whose `..` climbs above the
//!   image root, and a directory link that contains itself are refused. A
//!   layer entry whose path passes through a link (`link/passwd` after
//!   `link -> /etc`) is refused under either policy.
//! - **No traversal.** Absolute paths, `..` components, entries under a
//!   regular file, and `rootfs.manifest.json` at the image root are refused.
//!   Nothing is written to disk until the finished tree is, beneath private
//!   staging inside the CAS root, so no hostile entry reaches the host.
//! - **Deterministic identity.** The manifest lists files in path order with
//!   fixed field order, so the same image content gives the same rootfs
//!   digest whatever the layers' archive order, the host, or the run.
//! - **Atomic, verified publication.** The staged tree is verified against
//!   its own manifest, then renamed to `<cas-root>/<digest>`. An existing
//!   entry is reused only if it verifies, and never overwritten.
//!
//! No Docker, Buildah or Podman daemon, and no krunvm, is involved: those
//! tools are optional producers of the layout. The isolation boundary is
//! the backend (libkrun or native nono), not the image format.
//!
//! [OCI image layout]: https://github.com/opencontainers/image-spec/blob/v1.1.0/image-layout.md

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::rootfs_tree::{FileNode, Final, Node, PathError, ResolveError, Tree, join};
use crate::DigestRef;
use crate::backends::libkrun::plan::{DirectoryRootfsResolver, RootfsResolver, verify_manifest};

/// The default bound on what one import extracts and writes: 16 GiB.
pub const DEFAULT_MAX_ROOTFS_BYTES: u64 = 16 << 30;

/// Index, manifest and config documents larger than this are refused (the
/// image spec's own guidance for manifests is 4 MiB).
const MAX_DOCUMENT_BYTES: u64 = 4 << 20;

/// Bound on the entries a `Resolve` expansion may produce.
const MAX_RESOLVED_ENTRIES: usize = 1 << 20;

const ROOTFS_MANIFEST: &str = "rootfs.manifest.json";

const INDEX_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];
const MANIFEST_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];
const CONFIG_TYPES: [&str; 2] = [
    "application/vnd.oci.image.config.v1+json",
    "application/vnd.docker.container.image.v1+json",
];
const TAR_LAYER_TYPES: [&str; 3] = [
    "application/vnd.oci.image.layer.v1.tar",
    "application/vnd.oci.image.layer.nondistributable.v1.tar",
    "application/vnd.docker.image.rootfs.diff.tar",
];
const GZIP_LAYER_TYPES: [&str; 3] = [
    "application/vnd.oci.image.layer.v1.tar+gzip",
    "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
    "application/vnd.docker.image.rootfs.diff.tar.gzip",
];

/// What to do with the symbolic links an image's root filesystem contains.
/// Rootfs manifest v1 has no way to express one (bead
/// `ley-line-open-5f8c37` proposes authenticated link entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymlinkPolicy {
    /// Refuse the import, naming the first link in path order.
    #[default]
    Refuse,
    /// Replace each link with a copy of its in-image target.
    Resolve,
}

/// An OCI platform: `os/architecture[/variant]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
    pub fn linux(architecture: &str) -> Self {
        Self {
            os: "linux".into(),
            architecture: architecture.into(),
            variant: None,
        }
    }

    pub fn with_variant(mut self, variant: &str) -> Self {
        self.variant = Some(variant.into());
        self
    }

    /// A Linux guest of this host's architecture: what libkrun, which does
    /// not emulate, can boot here.
    pub fn host() -> Result<Self, OciImportError> {
        match std::env::consts::ARCH {
            "aarch64" => Ok(Self::linux("arm64")),
            "x86_64" => Ok(Self::linux("amd64")),
            other => Err(OciImportError::Unsupported(format!(
                "no OCI architecture is known for host architecture {other}"
            ))),
        }
    }

    /// Whether an image or index entry for `os/architecture/variant` is for
    /// this platform. A platform without a variant accepts any; `arm64`
    /// without a variant is `arm64/v8`, as the image spec defines it.
    fn admits(&self, os: &str, architecture: &str, variant: Option<&str>) -> bool {
        fn normalized<'a>(architecture: &str, variant: Option<&'a str>) -> Option<&'a str> {
            match (architecture, variant) {
                ("arm64", None) => Some("v8"),
                (_, variant) => variant,
            }
        }
        self.os == os
            && self.architecture == architecture
            && self.variant.as_deref().is_none_or(|wanted| {
                normalized(architecture, variant) == normalized(architecture, Some(wanted))
            })
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(variant) = &self.variant {
            write!(f, "/{variant}")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OciImportError {
    #[error(
        "mutable image reference {0:?} refused: pin the image by digest (sha256:<hex> or name@sha256:<hex>)"
    )]
    MutableReference(String),
    #[error("malformed image digest {0:?}: expected sha256:<64 lowercase hex characters>")]
    MalformedDigest(String),
    #[error("blob {digest} refused: {reason}")]
    DigestMismatch { digest: String, reason: String },
    #[error("layer {layer}'s uncompressed content does not hash to its config diff_id")]
    DiffIdMismatch { layer: usize },
    #[error("the image index has no manifest for platform {0}")]
    NoMatchingPlatform(String),
    #[error(
        "the image index has several manifests for platform {0}; name a variant or pin the manifest digest"
    )]
    AmbiguousPlatform(String),
    #[error("the image is for {image}, not the requested platform {requested}")]
    PlatformMismatch { image: String, requested: String },
    #[error("layer {layer} entry {path:?} refused: {reason}")]
    UnsafePath {
        layer: usize,
        path: String,
        reason: String,
    },
    #[error(
        "symbolic link {path} -> {target:?} refused: rootfs manifest v1 cannot express links (import with symlink resolution to copy link targets)"
    )]
    SymlinkRefused { path: String, target: String },
    #[error("symbolic link {path} cannot be resolved inside the image: {reason}")]
    SymlinkUnresolvable { path: String, reason: String },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("malformed OCI image layout: {0}")]
    Malformed(String),
    #[error("the rootfs exceeds its {limit}-byte import limit")]
    TooLarge { limit: u64 },
    #[error("CAS: {0}")]
    Cas(String),
    #[error("{action}: {source}")]
    Io {
        action: &'static str,
        #[source]
        source: io::Error,
    },
}

fn io_error(action: &'static str) -> impl FnOnce(io::Error) -> OciImportError {
    move |source| OciImportError::Io { action, source }
}

/// One import: which image, for which platform, under which link policy.
#[derive(Debug, Clone)]
pub struct OciImport<'a> {
    /// The OCI image layout directory.
    pub layout: &'a Path,
    /// `sha256:<hex>` or `name@sha256:<hex>` of an image index or manifest
    /// stored in the layout.
    pub reference: &'a str,
    pub platform: Platform,
    pub symlinks: SymlinkPolicy,
    /// Bound on the bytes extracted from the layers, and separately on the
    /// bytes of the rootfs written (resolved link copies included).
    pub max_rootfs_bytes: u64,
}

/// Where a rootfs came from. These are OCI SHA-256 digests: provenance, not
/// the rootfs identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageProvenance {
    /// The digest the import was pinned to (index or manifest).
    pub reference: String,
    /// The image manifest the rootfs was built from.
    pub manifest: String,
    /// That manifest's config.
    pub config: String,
    pub platform: Platform,
}

/// The image config's process defaults, as the (digest-verified) config
/// states them. Advisory: an execution catalog names a guest-relative
/// executable itself; nothing here is part of the rootfs identity.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRuntimeConfig {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: String,
    pub user: String,
}

/// A published rootfs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedRootfs {
    /// The rootfs identity: `blake3-256` over `rootfs.manifest.json`, and the
    /// directory name beneath the CAS root. What an execution catalog's
    /// `rootfs` names.
    pub rootfs: DigestRef,
    pub image: ImageProvenance,
    /// Regular files in the rootfs manifest.
    pub files: usize,
    /// Files whose setuid, setgid or sticky bit was cleared.
    pub special_mode_bits_cleared: usize,
    /// Links replaced by copies of their targets (`SymlinkPolicy::Resolve`).
    pub symlinks_resolved: usize,
    pub image_config: ImageRuntimeConfig,
}

/// Import the image `request` pins from its OCI layout into `cas_root`, an
/// existing directory, and return the published rootfs.
pub fn import_oci_layout(
    request: &OciImport<'_>,
    cas_root: &Path,
) -> Result<ImportedRootfs, OciImportError> {
    let reference = parse_reference(request.reference)?;
    let layout = Layout::open(request.layout)?;
    let cas_root = cas_root
        .canonicalize()
        .map_err(io_error("open the CAS root"))?;

    let top = layout.document(&reference, None)?;
    let (manifest_digest, manifest_bytes) = match classify(&top)? {
        Document::Index => {
            let index: Index = parse(&top, "image index")?;
            let selected = select_platform(&index, &request.platform)?;
            let bytes = layout.document(&selected.digest, Some(selected.size))?;
            (selected.digest.clone(), bytes)
        }
        Document::Manifest => (reference.clone(), top),
    };
    let manifest: Manifest = parse(&manifest_bytes, "image manifest")?;
    if !MANIFEST_TYPES.contains(&manifest.media_type.as_deref().unwrap_or(MANIFEST_TYPES[0])) {
        return Err(OciImportError::Unsupported(format!(
            "manifest media type {:?}",
            manifest.media_type
        )));
    }
    if !CONFIG_TYPES.contains(&manifest.config.media_type.as_str()) {
        return Err(OciImportError::Unsupported(format!(
            "config media type {:?}: not a container image",
            manifest.config.media_type
        )));
    }
    let config: ImageConfig = parse(
        &layout.document(&manifest.config.digest, Some(manifest.config.size))?,
        "image config",
    )?;
    if !request
        .platform
        .admits(&config.os, &config.architecture, config.variant.as_deref())
    {
        let image = Platform {
            os: config.os,
            architecture: config.architecture,
            variant: config.variant,
        };
        return Err(OciImportError::PlatformMismatch {
            image: image.to_string(),
            requested: request.platform.to_string(),
        });
    }
    if config.rootfs.kind != "layers" {
        return Err(OciImportError::Unsupported(format!(
            "config rootfs type {:?}",
            config.rootfs.kind
        )));
    }
    if config.rootfs.diff_ids.len() != manifest.layers.len() {
        return Err(OciImportError::Malformed(format!(
            "the manifest lists {} layers and the config {} diff_ids",
            manifest.layers.len(),
            config.rootfs.diff_ids.len()
        )));
    }

    let staging = tempfile::Builder::new()
        .prefix(".oci-import-")
        .tempdir_in(&cas_root)
        .map_err(io_error("create import staging in the CAS root"))?;
    let mut store = ContentStore::create(staging.path().join("content"), request.max_rootfs_bytes)?;
    let mut tree = Tree::default();
    for (index, (layer, diff_id)) in manifest
        .layers
        .iter()
        .zip(&config.rootfs.diff_ids)
        .enumerate()
    {
        let compression = Compression::of(&layer.media_type)?;
        let staged = staging.path().join(format!("layer-{index}"));
        layout.stage_layer(layer, &staged)?;
        let changes = read_layer(index, &staged, compression, diff_id, &mut store)?;
        fs::remove_file(&staged).map_err(io_error("remove a staged layer"))?;
        changes.apply(index, &mut tree)?;
    }

    let (finished, symlinks_resolved) = match request.symlinks {
        SymlinkPolicy::Refuse => {
            if let Some((path, target)) = tree.first_symlink() {
                return Err(OciImportError::SymlinkRefused { path, target });
            }
            (tree.into_final(), 0)
        }
        SymlinkPolicy::Resolve => tree
            .resolve_symlinks(MAX_RESOLVED_ENTRIES)
            .map_err(|error| match error {
                ResolveError::Unresolvable(link) => OciImportError::SymlinkUnresolvable {
                    path: link.path,
                    reason: link.reason.into(),
                },
                ResolveError::TooManyNodes => OciImportError::Unsupported(format!(
                    "resolving symbolic links would write more than {MAX_RESOLVED_ENTRIES} entries"
                )),
            })?,
    };

    let staged_root = staging.path().join("rootfs");
    let written = write_rootfs(&staged_root, &finished, &store, request.max_rootfs_bytes)?;
    let rootfs = DigestRef {
        algorithm: "blake3-256".into(),
        value: written.digest.clone(),
    };
    verify_manifest(&staged_root, &rootfs).map_err(|error| {
        OciImportError::Cas(format!(
            "the staged rootfs does not verify against its own manifest: {}",
            error.detail
        ))
    })?;
    publish(&cas_root, &staged_root, &rootfs)?;

    let runtime = config.config.unwrap_or_default();
    Ok(ImportedRootfs {
        rootfs,
        image: ImageProvenance {
            reference,
            manifest: manifest_digest,
            config: manifest.config.digest,
            platform: request.platform.clone(),
        },
        files: written.files,
        special_mode_bits_cleared: written.special_mode_bits_cleared,
        symlinks_resolved,
        image_config: ImageRuntimeConfig {
            entrypoint: runtime.entrypoint.unwrap_or_default(),
            cmd: runtime.cmd.unwrap_or_default(),
            env: runtime.env.unwrap_or_default(),
            working_dir: runtime.working_dir.unwrap_or_default(),
            user: runtime.user.unwrap_or_default(),
        },
    })
}

/// The `sha256:<hex>` a reference pins, or why it pins nothing.
fn parse_reference(reference: &str) -> Result<String, OciImportError> {
    let pinned = reference
        .rsplit_once('@')
        .map_or(reference, |(_, digest)| digest);
    if let Some(hex) = pinned.strip_prefix("sha256:") {
        return if is_lower_hex(hex, 64) {
            Ok(pinned.to_string())
        } else {
            Err(OciImportError::MalformedDigest(reference.to_string()))
        };
    }
    // The image spec's digest grammar: `algorithm:encoded`, the encoding
    // hexadecimal for every registered algorithm. A tag (`alpine:3.20`) has
    // no hexadecimal encoding.
    if let Some((algorithm, encoded)) = pinned.split_once(':')
        && !algorithm.is_empty()
        && algorithm
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && encoded.len() >= 32
        && is_lower_hex(encoded, encoded.len())
    {
        return Err(OciImportError::Unsupported(format!(
            "digest algorithm {algorithm}: only sha256 image digests are verified"
        )));
    }
    Err(OciImportError::MutableReference(reference.to_string()))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

struct Layout {
    blobs: PathBuf,
}

#[derive(Deserialize)]
struct LayoutMarker {
    #[serde(rename = "imageLayoutVersion")]
    image_layout_version: String,
}

impl Layout {
    fn open(root: &Path) -> Result<Self, OciImportError> {
        let marker = fs::read(root.join("oci-layout")).map_err(|error| {
            OciImportError::Malformed(format!(
                "{} has no readable oci-layout file: {error}",
                root.display()
            ))
        })?;
        let marker: LayoutMarker = parse(&marker, "oci-layout")?;
        if marker.image_layout_version != "1.0.0" {
            return Err(OciImportError::Unsupported(format!(
                "image layout version {}",
                marker.image_layout_version
            )));
        }
        Ok(Self {
            blobs: root.join("blobs/sha256"),
        })
    }

    /// The blob file for a descriptor digest, which must be well formed: the
    /// digest is joined to a path, so it is never trusted to be a name.
    fn blob_path(&self, digest: &str) -> Result<PathBuf, OciImportError> {
        match digest.strip_prefix("sha256:") {
            Some(hex) if is_lower_hex(hex, 64) => Ok(self.blobs.join(hex)),
            _ => Err(OciImportError::Unsupported(format!(
                "descriptor digest {digest:?}: only sha256 digests are verified"
            ))),
        }
    }

    /// A JSON document blob, read whole and authenticated.
    fn document(&self, digest: &str, size: Option<u64>) -> Result<Vec<u8>, OciImportError> {
        let file = File::open(self.blob_path(digest)?).map_err(io_error("open an image blob"))?;
        let mut bytes = Vec::new();
        file.take(MAX_DOCUMENT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error("read an image blob"))?;
        if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(OciImportError::Unsupported(format!(
                "document {digest} is larger than {MAX_DOCUMENT_BYTES} bytes"
            )));
        }
        check_blob(digest, size, bytes.len() as u64, &Sha256::digest(&bytes))?;
        Ok(bytes)
    }

    /// Copy a layer blob into private staging, authenticating it on the way:
    /// what is parsed afterwards is the copy, so the layout cannot change it
    /// between verification and use.
    fn stage_layer(&self, layer: &Descriptor, staged: &Path) -> Result<(), OciImportError> {
        let source =
            File::open(self.blob_path(&layer.digest)?).map_err(io_error("open a layer blob"))?;
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staged)
            .map_err(io_error("stage a layer blob"))?;
        let mut reader = source.take(layer.size.saturating_add(1));
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 16];
        let mut size = 0u64;
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(io_error("read a layer blob"))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            destination
                .write_all(&buffer[..read])
                .map_err(io_error("stage a layer blob"))?;
            size += read as u64;
        }
        check_blob(&layer.digest, Some(layer.size), size, &hasher.finalize())
    }
}

fn check_blob(
    digest: &str,
    size: Option<u64>,
    actual_size: u64,
    actual: &[u8],
) -> Result<(), OciImportError> {
    if size.is_some_and(|size| size != actual_size) {
        return Err(OciImportError::DigestMismatch {
            digest: digest.into(),
            reason: format!(
                "its descriptor's size is {}, the blob's at least {actual_size}",
                size.unwrap_or_default()
            ),
        });
    }
    if format!("sha256:{}", lower_hex(actual)) != digest {
        return Err(OciImportError::DigestMismatch {
            digest: digest.into(),
            reason: "its content does not hash to its digest".into(),
        });
    }
    Ok(())
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse<'a, T: Deserialize<'a>>(bytes: &'a [u8], what: &str) -> Result<T, OciImportError> {
    serde_json::from_slice(bytes)
        .map_err(|error| OciImportError::Malformed(format!("invalid {what}: {error}")))
}

#[derive(Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    platform: Option<DescriptorPlatform>,
}

#[derive(Deserialize)]
struct DescriptorPlatform {
    architecture: String,
    os: String,
    #[serde(default)]
    variant: Option<String>,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct ImageConfig {
    architecture: String,
    os: String,
    #[serde(default)]
    variant: Option<String>,
    rootfs: ConfigRootfs,
    #[serde(default)]
    config: Option<ConfigProcess>,
}

#[derive(Deserialize)]
struct ConfigRootfs {
    #[serde(rename = "type")]
    kind: String,
    diff_ids: Vec<String>,
}

#[derive(Deserialize, Default)]
struct ConfigProcess {
    #[serde(rename = "Entrypoint", default)]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd", default)]
    cmd: Option<Vec<String>>,
    #[serde(rename = "Env", default)]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir", default)]
    working_dir: Option<String>,
    #[serde(rename = "User", default)]
    user: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Document {
    Index,
    Manifest,
}

/// Whether the pinned document is an index or a manifest: by its declared
/// `mediaType`, or, when it declares none (the field is optional), by shape.
fn classify(bytes: &[u8]) -> Result<Document, OciImportError> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(rename = "schemaVersion", default)]
        schema_version: Option<u64>,
        #[serde(rename = "mediaType", default)]
        media_type: Option<String>,
        #[serde(default)]
        manifests: Option<serde_json::Value>,
        #[serde(default)]
        layers: Option<serde_json::Value>,
    }
    let probe: Probe = parse(bytes, "pinned document")?;
    let document = match probe.media_type.as_deref() {
        Some(media) if INDEX_TYPES.contains(&media) => Document::Index,
        Some(media) if MANIFEST_TYPES.contains(&media) => Document::Manifest,
        Some(media) => {
            return Err(OciImportError::Unsupported(format!(
                "pinned document media type {media}"
            )));
        }
        None if probe.manifests.is_some() => Document::Index,
        None if probe.layers.is_some() => Document::Manifest,
        None => {
            return Err(OciImportError::Malformed(
                "the pinned document is neither an image index nor an image manifest".into(),
            ));
        }
    };
    if probe.schema_version != Some(2) {
        return Err(OciImportError::Unsupported(format!(
            "schemaVersion {:?}",
            probe.schema_version
        )));
    }
    Ok(document)
}

fn select_platform<'a>(
    index: &'a Index,
    platform: &Platform,
) -> Result<&'a Descriptor, OciImportError> {
    let mut candidates = index.manifests.iter().filter(|descriptor| {
        descriptor.platform.as_ref().is_some_and(|candidate| {
            platform.admits(
                &candidate.os,
                &candidate.architecture,
                candidate.variant.as_deref(),
            )
        })
    });
    let selected = candidates
        .next()
        .ok_or_else(|| OciImportError::NoMatchingPlatform(platform.to_string()))?;
    if candidates.next().is_some() {
        return Err(OciImportError::AmbiguousPlatform(platform.to_string()));
    }
    if !MANIFEST_TYPES.contains(&selected.media_type.as_str()) {
        return Err(OciImportError::Unsupported(format!(
            "index entry {} has media type {}: nested indexes are not followed",
            selected.digest, selected.media_type
        )));
    }
    Ok(selected)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
}

impl Compression {
    fn of(media_type: &str) -> Result<Self, OciImportError> {
        if TAR_LAYER_TYPES.contains(&media_type) {
            Ok(Self::None)
        } else if GZIP_LAYER_TYPES.contains(&media_type) {
            Ok(Self::Gzip)
        } else {
            Err(OciImportError::Unsupported(format!(
                "layer media type {media_type}"
            )))
        }
    }
}

/// Regular-file contents extracted from the layers, keyed by BLAKE3.
struct ContentStore {
    dir: PathBuf,
    next: u64,
    extracted: u64,
    limit: u64,
}

impl ContentStore {
    fn create(dir: PathBuf, limit: u64) -> Result<Self, OciImportError> {
        fs::create_dir(&dir).map_err(io_error("create the import content store"))?;
        Ok(Self {
            dir,
            next: 0,
            extracted: 0,
            limit,
        })
    }

    fn path(&self, content: &blake3::Hash) -> PathBuf {
        self.dir.join(content.to_hex().as_str())
    }

    fn put(&mut self, reader: &mut impl Read) -> Result<(blake3::Hash, u64), OciImportError> {
        let temporary = self.dir.join(format!("partial-{}", self.next));
        self.next += 1;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(io_error("extract a layer file"))?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0; 1 << 16];
        let mut size = 0u64;
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(io_error("read a layer file"))?;
            if read == 0 {
                break;
            }
            size += read as u64;
            self.extracted += read as u64;
            if self.extracted > self.limit {
                return Err(OciImportError::TooLarge { limit: self.limit });
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(io_error("extract a layer file"))?;
        }
        let content = hasher.finalize();
        fs::rename(&temporary, self.path(&content)).map_err(io_error("store a layer file"))?;
        Ok((content, size))
    }
}

/// A `Read` that hashes everything read through it: a layer's diff_id.
struct DiffIdReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> Read for DiffIdReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

enum Change {
    Node(Vec<String>, Node),
    Hardlink(Vec<String>, Vec<String>),
}

/// One layer, read whole before any of it is applied: its whiteouts must
/// act on the lower layers before its own entries land.
#[derive(Default)]
struct LayerChanges {
    whiteouts: Vec<(String, Vec<String>, String)>,
    opaques: Vec<(String, Vec<String>)>,
    entries: Vec<(String, Change)>,
}

impl LayerChanges {
    fn apply(self, layer: usize, tree: &mut Tree) -> Result<(), OciImportError> {
        let refuse = |raw: String| move |error: PathError| unsafe_path(layer, &raw, error.reason());
        for (raw, parent, name) in self.whiteouts {
            tree.whiteout(&parent, &name).map_err(refuse(raw))?;
        }
        for (raw, dir) in self.opaques {
            tree.opaque(&dir).map_err(refuse(raw))?;
        }
        for (raw, change) in self.entries {
            match change {
                Change::Node(path, node) => tree.insert(&path, node),
                Change::Hardlink(path, target) => tree.hardlink(&path, &target),
            }
            .map_err(refuse(raw))?;
        }
        Ok(())
    }
}

fn unsafe_path(layer: usize, raw: &str, reason: &str) -> OciImportError {
    OciImportError::UnsafePath {
        layer,
        path: raw.to_string(),
        reason: reason.to_string(),
    }
}

/// The components of an archive path, relative to the image root. `.` and
/// empty components (`./a//b/`) are dropped; an absolute path or a `..`
/// component is refused, never normalized.
fn guest_path(raw: &str) -> Result<Vec<String>, &'static str> {
    if raw.starts_with('/') {
        return Err("absolute paths are refused");
    }
    let mut components = Vec::new();
    for component in raw.split('/') {
        match component {
            "" | "." => {}
            ".." => return Err("`..` components are refused"),
            name => components.push(name.to_string()),
        }
    }
    Ok(components)
}

fn utf8(layer: usize, bytes: &[u8]) -> Result<String, OciImportError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        unsafe_path(
            layer,
            &String::from_utf8_lossy(bytes),
            "the rootfs manifest names paths as UTF-8 and this one is not",
        )
    })
}

fn read_layer(
    layer: usize,
    staged: &Path,
    compression: Compression,
    diff_id: &str,
    store: &mut ContentStore,
) -> Result<LayerChanges, OciImportError> {
    let file = BufReader::new(File::open(staged).map_err(io_error("open a staged layer"))?);
    let decoded: Box<dyn Read> = match compression {
        Compression::None => Box::new(file),
        Compression::Gzip => Box::new(flate2::read::GzDecoder::new(file)),
    };
    let mut reader = DiffIdReader {
        inner: decoded,
        hasher: Sha256::new(),
    };
    let mut changes = LayerChanges::default();
    {
        let mut archive = tar::Archive::new(&mut reader);
        for entry in archive
            .entries()
            .map_err(io_error("read a layer archive"))?
        {
            let mut entry = entry.map_err(io_error("read a layer archive entry"))?;
            let raw = utf8(layer, &entry.path_bytes())?;
            let path = guest_path(&raw).map_err(|reason| unsafe_path(layer, &raw, reason))?;
            let kind = entry.header().entry_type();
            if kind == tar::EntryType::XGlobalHeader {
                continue;
            }
            if let Some((name, parent)) = path.split_last()
                && let Some(hidden) = name.strip_prefix(".wh.")
            {
                if hidden == ".wh..opq" {
                    changes.opaques.push((raw, parent.to_vec()));
                } else if hidden.is_empty() || hidden.starts_with(".wh.") {
                    return Err(unsafe_path(
                        layer,
                        &raw,
                        "an aufs metadata entry or empty whiteout is not an OCI whiteout",
                    ));
                } else {
                    let (parent, hidden) = (parent.to_vec(), hidden.to_string());
                    changes.whiteouts.push((raw, parent, hidden));
                }
                continue;
            }
            if kind == tar::EntryType::Directory {
                if !path.is_empty() {
                    changes
                        .entries
                        .push((raw, Change::Node(path, Node::Dir(Default::default()))));
                }
                continue;
            }
            if path.is_empty() {
                return Err(unsafe_path(
                    layer,
                    &raw,
                    "only a directory may name the image root",
                ));
            }
            if path == [ROOTFS_MANIFEST] {
                return Err(unsafe_path(
                    layer,
                    &raw,
                    "the image root's rootfs.manifest.json is the rootfs manifest's own name",
                ));
            }
            let change = match kind {
                tar::EntryType::Regular | tar::EntryType::Continuous => {
                    let mode = entry
                        .header()
                        .mode()
                        .map_err(io_error("read an entry mode"))?
                        & 0o7777;
                    let (content, size) = store.put(&mut entry)?;
                    Change::Node(
                        path,
                        Node::File(FileNode {
                            content,
                            size,
                            mode,
                        }),
                    )
                }
                tar::EntryType::Symlink => {
                    let target = match entry.link_name_bytes() {
                        Some(bytes) => utf8(layer, &bytes)?,
                        None => String::new(),
                    };
                    Change::Node(path, Node::Symlink(target))
                }
                tar::EntryType::Link => {
                    let target = match entry.link_name_bytes() {
                        Some(bytes) => utf8(layer, &bytes)?,
                        None => String::new(),
                    };
                    let target = guest_path(&target).map_err(|reason| {
                        unsafe_path(layer, &raw, &format!("hard link target: {reason}"))
                    })?;
                    Change::Hardlink(path, target)
                }
                other => {
                    return Err(OciImportError::Unsupported(format!(
                        "layer {layer} entry {raw}: {other:?} entries are not expressible in rootfs manifest v1"
                    )));
                }
            };
            changes.entries.push((raw, change));
        }
    }
    io::copy(&mut reader, &mut io::sink()).map_err(io_error("read a layer archive"))?;
    if format!("sha256:{}", lower_hex(&reader.hasher.finalize())) != diff_id {
        return Err(OciImportError::DiffIdMismatch { layer });
    }
    Ok(changes)
}

struct Written {
    digest: String,
    files: usize,
    special_mode_bits_cleared: usize,
}

#[derive(Serialize)]
struct ManifestDocument<'a> {
    version: u32,
    files: &'a [ManifestFile],
}

#[derive(Serialize)]
struct ManifestFile {
    path: String,
    mode: u32,
    blake3: String,
}

struct Writer<'a> {
    store: &'a ContentStore,
    limit: u64,
    written: u64,
    files: Vec<ManifestFile>,
    special_mode_bits_cleared: usize,
}

impl Writer<'_> {
    fn directory(
        &mut self,
        on_disk: &Path,
        prefix: &str,
        entries: &std::collections::BTreeMap<String, Final>,
    ) -> Result<(), OciImportError> {
        for (name, node) in entries {
            let path = on_disk.join(name);
            let relative = join(prefix, name);
            match node {
                Final::Dir(children) => {
                    create_directory(&path)?;
                    self.directory(&path, &relative, children)?;
                }
                Final::File(file) => self.file(&path, relative, file)?,
            }
        }
        Ok(())
    }

    fn file(
        &mut self,
        path: &Path,
        relative: String,
        file: &FileNode,
    ) -> Result<(), OciImportError> {
        let mut mode = file.mode;
        if mode & 0o7000 != 0 {
            mode &= 0o777;
            self.special_mode_bits_cleared += 1;
        }
        if mode & 0o400 == 0 {
            return Err(OciImportError::Unsupported(format!(
                "{relative} has mode {mode:04o}: a file its owner cannot read cannot be verified against the rootfs manifest"
            )));
        }
        self.written += file.size;
        if self.written > self.limit {
            return Err(OciImportError::TooLarge { limit: self.limit });
        }
        let mut source = File::open(self.store.path(&file.content))
            .map_err(io_error("open extracted content"))?;
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(io_error(
                "write a rootfs file (a case- or normalization-insensitive host filesystem merges image paths that differ only in case)",
            ))?;
        io::copy(&mut source, &mut destination).map_err(io_error("write a rootfs file"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(io_error("set a rootfs file's mode"))?;
        self.files.push(ManifestFile {
            path: relative,
            mode,
            blake3: file.content.to_hex().to_string(),
        });
        Ok(())
    }
}

fn create_directory(path: &Path) -> Result<(), OciImportError> {
    fs::create_dir(path).map_err(io_error(
        "create a rootfs directory (a case- or normalization-insensitive host filesystem merges image paths that differ only in case)",
    ))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(io_error("set a rootfs directory's mode"))
}

/// Write the finished tree and its manifest at `root`, returning the
/// manifest's BLAKE3.
fn write_rootfs(
    root: &Path,
    finished: &Final,
    store: &ContentStore,
    limit: u64,
) -> Result<Written, OciImportError> {
    let Final::Dir(entries) = finished else {
        unreachable!("a finished tree is rooted at a directory")
    };
    create_directory(root)?;
    let mut writer = Writer {
        store,
        limit,
        written: 0,
        files: Vec::new(),
        special_mode_bits_cleared: 0,
    };
    writer.directory(root, "", entries)?;
    let manifest = serde_json::to_vec(&ManifestDocument {
        version: 1,
        files: &writer.files,
    })
    .expect("a rootfs manifest serializes");
    let manifest_path = root.join(ROOTFS_MANIFEST);
    fs::write(&manifest_path, &manifest).map_err(io_error("write the rootfs manifest"))?;
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o644))
        .map_err(io_error("set the rootfs manifest's mode"))?;
    Ok(Written {
        digest: blake3::hash(&manifest).to_hex().to_string(),
        files: writer.files.len(),
        special_mode_bits_cleared: writer.special_mode_bits_cleared,
    })
}

/// Rename the verified staged tree to `<cas-root>/<digest>`. An entry that
/// is already there is reused only if the CAS resolver verifies it.
fn publish(cas_root: &Path, staged: &Path, rootfs: &DigestRef) -> Result<(), OciImportError> {
    let destination = cas_root.join(&rootfs.value);
    if destination.symlink_metadata().is_err() && fs::rename(staged, &destination).is_ok() {
        return Ok(());
    }
    DirectoryRootfsResolver::new(cas_root)
        .resolve(rootfs)
        .map(|_| ())
        .map_err(|error| {
            OciImportError::Cas(format!(
                "{} already exists and does not verify as rootfs {}; refusing to replace it: {}",
                destination.display(),
                rootfs.value,
                error.detail
            ))
        })
}
