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
//! - **Every byte authenticated.** Each blob must be a regular file at
//!   `blobs/sha256/<hex>`, is read into owner-only staging, and must hash to
//!   its digest and match its descriptor's size before it is parsed. Each
//!   layer's whole uncompressed stream (every gzip member) must hash to the
//!   config's `diff_id`.
//! - **Bounded.** `max_rootfs_bytes` bounds the decompressed bytes read from
//!   all layers together (headers and long-name metadata included) and,
//!   separately, the bytes written. Paths and link targets longer than
//!   `PATH_MAX` (4096 bytes) are refused.
//! - **Platform.** An index selects the one entry for the requested
//!   platform (`arm64` without a variant is `arm64/v8`); none or several is
//!   an error. The image config must agree with the requested platform.
//! - **Layer semantics.** Layers apply in order to an in-memory tree; a
//!   layer's whiteouts (`.wh.<name>`) and opaque whiteouts (`.wh..wh..opq`)
//!   remove lower-layer entries only, never ones the same layer adds.
//! - **What rootfs manifest v1 can express.** Regular files with their
//!   permission bits. Directories are written `0755` and only where a file
//!   needs them (a directory that holds no file, at any depth, is not
//!   written): neither a directory's mode, an empty directory, ownership,
//!   timestamps nor extended attributes is in the manifest, so none is
//!   carried, and two imports with the same identity are the same tree on
//!   disk. The guest init creates its own `/dev`, `/proc` and `/sys` mount
//!   points. Setuid, setgid and sticky bits are cleared (and counted): the
//!   tree is written by an unprivileged host user, and a host file carrying
//!   them is a hazard the manifest has no reason to admit. A directory entry
//!   its owner cannot both read and search (written `0755` it would expose
//!   its subtree), a device node or FIFO outside the kernel-mounted
//!   directories below, a sparse file or an aufs `.wh..wh.` entry is refused
//!   by name. A hard link becomes a second regular file with its target's
//!   content and mode.
//! - **Entries the rootfs cannot carry faithfully are dropped, never
//!   written, and reported** instead of refusing the image. The runtime
//!   README's "Dropped entries" section states, for each rule, what a guest
//!   can still observe of the difference:
//!   - A device node or FIFO strictly beneath `dev/`, `proc/` or `sys/`
//!     ([`ImportedRootfs::special_entries_dropped`]): libkrun's guest init
//!     mounts devtmpfs, proc and sysfs over those directories before the
//!     workload runs (`init.c` `mount_filesystems`; see
//!     `rootfs_tree::KERNEL_MOUNTED` for the line citations). It replaces
//!     whatever a lower layer had at its path.
//!   - A regular file whose owner-read bit is clear (`etc/shadow` at
//!     `0000`, [`ImportedRootfs::unreadable_files_dropped`]): no
//!     unprivileged reader, the guest's virtio-fs server included, can read
//!     it.
//!   - Under `Resolve`, a link whose resolution goes beneath (or ends at)
//!     `dev`, `proc` or `sys` ([`ImportedRootfs::links_dropped_kernel_mount`]),
//!     and a link whose target the image does not hold
//!     ([`ImportedRootfs::links_dropped_dangling`]). See below.
//! - **Symbolic links.** The manifest admits none, so [`SymlinkPolicy`]
//!   decides: `Refuse` (the default) names the first link; `Resolve` replaces
//!   each link with a copy of the regular file or directory it resolves to
//!   inside the image, absolute targets meaning the image's root, never the
//!   host's. A link into a kernel mount is dropped: the guest kernel, not
//!   the image, decides what is there, so no image bytes stand in for it.
//!   A dangling link inside the root is dropped: nothing is written in its
//!   place. A link that reaches `/work` (where a run's workspace is
//!   mounted), a loop, a link whose `..` climbs above the image root (also
//!   after a missing component or a kernel mount point), a target that
//!   continues through a regular file, an empty target, and a directory link
//!   that contains itself are refused. A layer entry whose path passes
//!   through a link (`link/passwd` after `link -> /etc`) is refused under
//!   either policy.
//! - **No traversal.** Absolute paths, `..` components, entries under a
//!   regular file, and `rootfs.manifest.json` at the image root are refused.
//!   No image path reaches the host filesystem until the finished tree is
//!   written beneath owner-only staging inside the CAS root; until then,
//!   extracted file contents are stored only under their BLAKE3 names.
//! - **Deterministic identity.** The manifest lists files in path order with
//!   fixed field order, so the same image content gives the same rootfs
//!   digest whatever the layers' archive order, the host, or the run. A
//!   dropped entry is absent from the manifest: an image's digest is that of
//!   the same image with its dropped entries, and whatever they replaced in
//!   lower layers, removed.
//! - **Atomic, verified publication.** The staged tree is verified against
//!   its own manifest, then renamed to `<cas-root>/<digest>`. An existing
//!   entry is reused only if its files verify and its directories are exactly
//!   the ones this import wrote, all `0755`; it is never overwritten. Files
//!   are not fsynced before the rename: after a host crash an entry may fail
//!   to verify, and an import then refuses it rather than reusing it.
//!
//! No Docker, Buildah or Podman daemon, and no krunvm, is involved: those
//! tools are optional producers of the layout. The isolation boundary is
//! the backend (libkrun or native nono), not the image format.
//!
//! [OCI image layout]: https://github.com/opencontainers/image-spec/blob/v1.1.0/image-layout.md

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use leyline_core::ContentAddressed;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::rootfs_tree::{
    FileNode, Final, Node, PathError, ResolveError, Resolved, Tree, join, under_kernel_mount,
};
use crate::DigestRef;
use crate::backends::libkrun::plan::{DirectoryRootfsResolver, RootfsResolver, verify_manifest};

/// The default bound on what one import extracts and writes: 16 GiB.
pub const DEFAULT_MAX_ROOTFS_BYTES: u64 = 16 << 30;

/// Index, manifest and config documents larger than this are refused (the
/// image spec's own guidance for manifests is 4 MiB).
pub(super) const MAX_DOCUMENT_BYTES: u64 = 4 << 20;

/// Bound on the entries a `Resolve` expansion may produce.
const MAX_RESOLVED_ENTRIES: usize = 1 << 20;

const ROOTFS_MANIFEST: &str = "rootfs.manifest.json";

/// Linux's `PATH_MAX`: the longest archive path or link target accepted.
const MAX_PATH_BYTES: usize = 4096;

pub(super) const INDEX_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];
pub(super) const MANIFEST_TYPES: [&str; 2] = [
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
    /// Replace each link with a copy of its in-image target. A link into
    /// `dev`, `proc` or `sys`, or one whose target the image does not hold,
    /// is dropped and reported instead; one that loops or escapes the image
    /// root is refused.
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
    #[error("malformed registry reference {0:?}: expected [registry/]repository@sha256:<hex>")]
    MalformedReference(String),
    #[error("registry: {0}")]
    Registry(String),
    #[error("insecure transport refused: {0}")]
    InsecureTransport(String),
    #[error("the registry redirected more than {limit} times")]
    TooManyRedirects { limit: u32 },
    #[error("the image exceeds its {limit}-byte download limit")]
    DownloadTooLarge { limit: u64 },
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
    /// Bound on what the layers extract (file bytes plus 512 per archive
    /// entry, across every layer, whiteouts and overwritten files included),
    /// and separately on the bytes of the rootfs written (resolved link
    /// copies included).
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
    /// Links replaced by copies of their targets (`SymlinkPolicy::Resolve`),
    /// counting a copy later dropped as unreadable.
    pub symlinks_resolved: usize,
    // The four lists below are per-import provenance, like `image`: two
    // images can give one rootfs digest with different lists, and an
    // import that reuses a CAS entry reports its own. None is part of the
    // rootfs identity.
    /// Device nodes and FIFOs strictly beneath a directory the guest init
    /// mounts a kernel filesystem over (`dev`, `proc`, `sys`), in layer and
    /// archive order, each as it was dropped: a later entry may still write
    /// a regular file at the same path. None is written.
    pub special_entries_dropped: Vec<DroppedEntry>,
    /// Links (`SymlinkPolicy::Resolve`) whose resolution reaches `dev`,
    /// `proc` or `sys`, which the guest's kernel provides at boot. In path
    /// order; one per path written, so a link inside a copied directory is
    /// listed at each copy.
    pub links_dropped_kernel_mount: Vec<DroppedLink>,
    /// Links (`SymlinkPolicy::Resolve`) whose target is absent from the
    /// image and never leaves its root. In path order, as above.
    pub links_dropped_dangling: Vec<DroppedLink>,
    /// Regular files without their owner-read bit (`etc/shadow` at `0000`),
    /// in path order, one per path that would have been written (a hard
    /// link or a resolved link to one is another). None is written.
    pub unreadable_files_dropped: Vec<DroppedFile>,
    pub image_config: ImageRuntimeConfig,
}

/// A regular file an import dropped because its owner cannot read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DroppedFile {
    /// Where it would have been written, relative to the image root.
    pub path: String,
    /// Its permission bits as the layer gave them, special bits included.
    pub mode: u32,
}

/// What kind of entry a dropped special entry was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecialEntryKind {
    Char,
    Block,
    Fifo,
}

/// A device node or FIFO an import dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DroppedEntry {
    /// The layer whose archive held it, from 0.
    pub layer: usize,
    /// Its path relative to the image root, normalized as the manifest
    /// writes paths (`./dev//console` is `dev/console`).
    pub path: String,
    pub kind: SpecialEntryKind,
}

/// A symbolic link an import dropped instead of copying its target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DroppedLink {
    /// Where the link would have been written, relative to the image root.
    pub path: String,
    /// The link's target, verbatim.
    pub target: String,
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
    admit_manifest(&manifest)?;
    let config: ImageConfig = parse(
        &layout.document(&manifest.config.digest, Some(manifest.config.size))?,
        "image config",
    )?;
    if !request
        .platform
        .admits(&config.os, &config.architecture, config.variant.as_deref())
    {
        return Err(OciImportError::PlatformMismatch {
            image: config.platform().to_string(),
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
    // Owner-only: staged layers hold files a later layer may white out.
    fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))
        .map_err(io_error("make import staging private"))?;
    let mut store = ContentStore::create(staging.path().join("content"))?;
    let budget = Budget::new(request.max_rootfs_bytes);
    let mut tree = Tree::default();
    let mut special_entries_dropped = Vec::new();
    for (index, (layer, diff_id)) in manifest
        .layers
        .iter()
        .zip(&config.rootfs.diff_ids)
        .enumerate()
    {
        let compression = Compression::of(&layer.media_type)?;
        let staged = staging.path().join(format!("layer-{index}"));
        layout.stage_layer(layer, &staged)?;
        let changes = read_layer(index, &staged, compression, diff_id, &mut store, &budget)
            .map_err(|error| budget.explain(error))?;
        fs::remove_file(&staged).map_err(io_error("remove a staged layer"))?;
        special_entries_dropped.extend(changes.apply(index, &mut tree)?);
    }

    let resolved = match request.symlinks {
        SymlinkPolicy::Refuse => {
            if let Some((path, target)) = tree.first_symlink() {
                return Err(OciImportError::SymlinkRefused { path, target });
            }
            Resolved {
                tree: tree.into_final(),
                copied: 0,
                kernel_mount: Vec::new(),
                dangling: Vec::new(),
            }
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
    let dropped_links = |links: Vec<(String, String)>| {
        links
            .into_iter()
            .map(|(path, target)| DroppedLink { path, target })
            .collect()
    };
    let links_dropped_kernel_mount = dropped_links(resolved.kernel_mount);
    let links_dropped_dangling = dropped_links(resolved.dangling);
    let symlinks_resolved = resolved.copied;
    let (finished, unreadable) = resolved.tree.without_unreadable_files();
    let unreadable_files_dropped = unreadable
        .into_iter()
        .map(|(path, mode)| DroppedFile { path, mode })
        .collect();
    // Rootfs manifest v1 lists files only, so a directory that holds none is
    // not part of the identity: write none, and one digest names one tree.
    let finished = finished.without_empty_directories();

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
    publish(&cas_root, &staged_root, &rootfs, &written.directories)?;

    let platform = config.platform();
    let runtime = config.config.unwrap_or_default();
    Ok(ImportedRootfs {
        rootfs,
        image: ImageProvenance {
            reference,
            manifest: manifest_digest,
            config: manifest.config.digest,
            platform,
        },
        files: written.files,
        special_mode_bits_cleared: written.special_mode_bits_cleared,
        symlinks_resolved,
        special_entries_dropped,
        links_dropped_kernel_mount,
        links_dropped_dangling,
        unreadable_files_dropped,
        image_config: ImageRuntimeConfig {
            entrypoint: runtime.entrypoint.unwrap_or_default(),
            cmd: runtime.cmd.unwrap_or_default(),
            env: runtime.env.unwrap_or_default(),
            working_dir: runtime.working_dir.unwrap_or_default(),
            user: runtime.user.unwrap_or_default(),
        },
    })
}

/// What a manifest must be before its config is read or its layers are
/// fetched: an image manifest whose config is a container image config no
/// larger than a document, and whose layers are all of a readable type.
/// A registry pull applies it before requesting any blob.
pub(super) fn admit_manifest(manifest: &Manifest) -> Result<(), OciImportError> {
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
    if manifest.config.size > MAX_DOCUMENT_BYTES {
        return Err(OciImportError::Unsupported(format!(
            "document {} is larger than {MAX_DOCUMENT_BYTES} bytes",
            manifest.config.digest
        )));
    }
    for layer in &manifest.layers {
        Compression::of(&layer.media_type)?;
    }
    Ok(())
}

/// The `sha256:<hex>` a reference pins, or why it pins nothing.
pub(super) fn parse_reference(reference: &str) -> Result<String, OciImportError> {
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

/// The hex of a well-formed descriptor digest. A digest is joined to a path
/// (and, by a registry pull, to a URL), so it is never trusted to be a name.
pub(super) fn sha256_hex(digest: &str) -> Result<&str, OciImportError> {
    match digest.strip_prefix("sha256:") {
        Some(hex) if is_lower_hex(hex, 64) => Ok(hex),
        _ => Err(OciImportError::Unsupported(format!(
            "descriptor digest {digest:?}: only sha256 digests are verified"
        ))),
    }
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
        Ok(self.blobs.join(sha256_hex(digest)?))
    }

    /// Open the blob for `digest`, which must be a regular file: opening a
    /// FIFO planted in the layout would block the import forever.
    fn open_blob(&self, digest: &str) -> Result<File, OciImportError> {
        let path = self.blob_path(digest)?;
        let metadata = fs::metadata(&path).map_err(io_error("open an image blob"))?;
        if !metadata.is_file() {
            return Err(OciImportError::Malformed(format!(
                "blob {digest} is not a regular file"
            )));
        }
        File::open(&path).map_err(io_error("open an image blob"))
    }

    /// A JSON document blob, read whole and authenticated.
    fn document(&self, digest: &str, size: Option<u64>) -> Result<Vec<u8>, OciImportError> {
        let file = self.open_blob(digest)?;
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
        let source = self.open_blob(&layer.digest)?;
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

pub(super) fn check_blob(
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

pub(super) fn parse<'a, T: Deserialize<'a>>(
    bytes: &'a [u8],
    what: &str,
) -> Result<T, OciImportError> {
    serde_json::from_slice(bytes)
        .map_err(|error| OciImportError::Malformed(format!("invalid {what}: {error}")))
}

#[derive(Deserialize)]
pub(super) struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    pub(super) digest: String,
    pub(super) size: u64,
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
pub(super) struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
pub(super) struct Manifest {
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
    pub(super) config: Descriptor,
    pub(super) layers: Vec<Descriptor>,
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

impl ImageConfig {
    /// The platform the (digest-verified) config says the image is for.
    fn platform(&self) -> Platform {
        Platform {
            os: self.os.clone(),
            architecture: self.architecture.clone(),
            variant: self.variant.clone(),
        }
    }
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
pub(super) enum Document {
    Index,
    Manifest,
}

/// Whether the pinned document is an index or a manifest: by its declared
/// `mediaType`, or, when it declares none (the field is optional), by shape.
pub(super) fn classify(bytes: &[u8]) -> Result<Document, OciImportError> {
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

pub(super) fn select_platform<'a>(
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
}

impl ContentStore {
    fn create(dir: PathBuf) -> Result<Self, OciImportError> {
        fs::create_dir(&dir).map_err(io_error("create the import content store"))?;
        Ok(Self { dir })
    }

    fn path(&self, content: &blake3::Hash) -> PathBuf {
        self.dir.join(content.to_hex().as_str())
    }

    /// Store one file's content. Files are extracted one at a time and each
    /// partial file is renamed to its BLAKE3 name (or the import fails), so
    /// one partial name suffices.
    fn put(&mut self, reader: &mut impl Read) -> Result<(blake3::Hash, u64), OciImportError> {
        let temporary = self.dir.join("partial");
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
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(io_error("extract a layer file"))?;
        }
        let content = hasher.finalize();
        fs::rename(&temporary, self.path(&content)).map_err(io_error("store a layer file"))?;
        Ok((content, size))
    }
}

/// The import's bound on decompressed layer bytes: every byte of every
/// layer's tar stream, headers, metadata payloads, whited-out and trailing
/// bytes included, counted where the stream is read.
struct Budget {
    limit: u64,
    remaining: Cell<u64>,
    exceeded: Cell<bool>,
}

impl Budget {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            remaining: Cell::new(limit),
            exceeded: Cell::new(false),
        }
    }

    /// Spend `bytes`, or fail the read that would overspend.
    fn spend(&self, bytes: usize) -> io::Result<()> {
        let remaining = self.remaining.get();
        if bytes as u64 > remaining {
            self.exceeded.set(true);
            return Err(io::Error::other("the import's decompressed-byte limit"));
        }
        self.remaining.set(remaining - bytes as u64);
        Ok(())
    }

    /// An error raised while the budget was exhausted is that exhaustion,
    /// whichever reader (tar's, the content store's) reported it.
    fn explain(&self, error: OciImportError) -> OciImportError {
        if self.exceeded.get() {
            OciImportError::TooLarge { limit: self.limit }
        } else {
            error
        }
    }
}

/// A `Read` that hashes everything read through it — a layer's diff_id —
/// and charges it to the import's budget.
struct DiffIdReader<'a, R> {
    inner: R,
    hasher: Sha256,
    budget: &'a Budget,
}

impl<R: Read> Read for DiffIdReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.budget.spend(read)?;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

enum Change {
    Node(Vec<String>, Node),
    Hardlink(Vec<String>, Vec<String>),
    /// An entry that is not written: it replaces whatever a lower layer (or
    /// an earlier entry of this one) had at its path, and leaves nothing.
    Drop(Vec<String>),
}

/// One layer, read whole before any of it is applied: its whiteouts must
/// act on the lower layers before its own entries land.
#[derive(Default)]
struct LayerChanges {
    whiteouts: Vec<(String, Vec<String>, String)>,
    opaques: Vec<(String, Vec<String>)>,
    entries: Vec<(String, Change)>,
    dropped: Vec<DroppedEntry>,
}

impl LayerChanges {
    /// Apply the layer to `tree`, returning the entries it dropped.
    fn apply(self, layer: usize, tree: &mut Tree) -> Result<Vec<DroppedEntry>, OciImportError> {
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
                Change::Drop(path) => tree.remove(&path),
            }
            .map_err(refuse(raw))?;
        }
        Ok(self.dropped)
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
    // PATH_MAX. Also bounds the tree's depth, which every pass over it
    // recurses through.
    if raw.len() > MAX_PATH_BYTES {
        return Err("paths longer than 4096 bytes are refused");
    }
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
    budget: &Budget,
) -> Result<LayerChanges, OciImportError> {
    let file = BufReader::new(File::open(staged).map_err(io_error("open a staged layer"))?);
    // Every gzip member, as RFC 1952 and other OCI implementations read a
    // layer: the diff_id covers the whole stream, and trailing bytes that
    // are not a member are an error, not ignored.
    let decoded: Box<dyn Read> = match compression {
        Compression::None => Box::new(file),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(file)),
    };
    let mut reader = DiffIdReader {
        inner: decoded,
        hasher: Sha256::new(),
        budget,
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
                // Directory modes are not carried (each is written 0755), so
                // one whose owner cannot both list it and reach into it
                // would expose what it hides.
                let mode = entry
                    .header()
                    .mode()
                    .map_err(io_error("read an entry mode"))?;
                if mode & 0o500 != 0o500 {
                    return Err(unsafe_path(
                        layer,
                        &raw,
                        &format!(
                            "a directory its owner cannot read and search (mode {:04o}) would be written 0755, exposing what it hides",
                            mode & 0o7777
                        ),
                    ));
                }
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
                    if target.len() > MAX_PATH_BYTES {
                        return Err(unsafe_path(
                            layer,
                            &raw,
                            "link targets longer than 4096 bytes are refused",
                        ));
                    }
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
                tar::EntryType::Char | tar::EntryType::Block | tar::EntryType::Fifo => {
                    let kind = match kind {
                        tar::EntryType::Char => SpecialEntryKind::Char,
                        tar::EntryType::Block => SpecialEntryKind::Block,
                        _ => SpecialEntryKind::Fifo,
                    };
                    // Beneath a kernel mount the guest could never see it;
                    // anywhere else it would be a hole in the rootfs.
                    if !under_kernel_mount(&path) {
                        return Err(OciImportError::Unsupported(format!(
                            "layer {layer} entry {raw}: {kind:?} entries are not expressible in rootfs manifest v1, and only those beneath dev/, proc/ or sys/ (which the guest init mounts kernel filesystems over) are dropped"
                        )));
                    }
                    changes.dropped.push(DroppedEntry {
                        layer,
                        path: path.join("/"),
                        kind,
                    });
                    Change::Drop(path)
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
    /// Every directory written, relative, the root as "". All are `0755`.
    directories: BTreeSet<String>,
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
    directories: BTreeSet<String>,
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
                    self.directories.insert(relative.clone());
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
        // `Final::without_unreadable_files` removed every file whose owner
        // cannot read it: each file written here can be verified.
        debug_assert!(
            mode & 0o400 != 0,
            "{relative}: unreadable files are dropped before writing"
        );
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
        directories: BTreeSet::from([String::new()]),
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
        // σ_bytes over the manifest: the identity `verify_manifest` checks.
        digest: manifest.as_slice().hash().to_string(),
        files: writer.files.len(),
        special_mode_bits_cleared: writer.special_mode_bits_cleared,
        directories: writer.directories,
    })
}

/// Every directory under `root`, relative, the root as "", with its mode.
/// Links are not followed (and a verified rootfs holds none).
fn directories_on_disk(root: &Path) -> Result<BTreeMap<String, u32>, OciImportError> {
    fn walk(
        dir: &Path,
        prefix: &str,
        out: &mut BTreeMap<String, u32>,
    ) -> Result<(), OciImportError> {
        for entry in fs::read_dir(dir).map_err(io_error("enumerate a CAS entry"))? {
            let entry = entry.map_err(io_error("enumerate a CAS entry"))?;
            let metadata = entry
                .path()
                .symlink_metadata()
                .map_err(io_error("inspect a CAS entry"))?;
            if metadata.is_dir() {
                let relative = join(prefix, &entry.file_name().to_string_lossy());
                out.insert(relative.clone(), metadata.permissions().mode() & 0o7777);
                walk(&entry.path(), &relative, out)?;
            }
        }
        Ok(())
    }
    let mode = fs::symlink_metadata(root)
        .map_err(io_error("inspect a CAS entry"))?
        .permissions()
        .mode()
        & 0o7777;
    let mut out = BTreeMap::from([(String::new(), mode)]);
    walk(root, "", &mut out)?;
    Ok(out)
}

/// Rename the verified staged tree to `<cas-root>/<digest>`. An entry that
/// is already there is reused only if the CAS resolver verifies its files
/// and its directories are exactly the ones this import wrote, all `0755`:
/// the manifest does not cover directories, and the per-run copy carries
/// them into the guest.
fn publish(
    cas_root: &Path,
    staged: &Path,
    rootfs: &DigestRef,
    directories: &BTreeSet<String>,
) -> Result<(), OciImportError> {
    let destination = cas_root.join(&rootfs.value);
    if destination.symlink_metadata().is_err() && fs::rename(staged, &destination).is_ok() {
        return Ok(());
    }
    let refuse = |detail: String| {
        OciImportError::Cas(format!(
            "{} already exists and does not verify as rootfs {}; refusing to replace it: {detail}",
            destination.display(),
            rootfs.value,
        ))
    };
    let resolved = DirectoryRootfsResolver::new(cas_root)
        .resolve(rootfs)
        .map_err(|error| refuse(error.detail))?;
    let expected: BTreeMap<String, u32> = directories
        .iter()
        .map(|directory| (directory.clone(), 0o755))
        .collect();
    if directories_on_disk(&resolved.canonical_path)? != expected {
        return Err(refuse(
            "its directories or their modes differ from the ones its manifest implies".into(),
        ));
    }
    Ok(())
}
