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
//!     `dev`, `proc`, `sys` or `tmp` ([`ImportedRootfs::links_dropped_kernel_mount`]),
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
//! - **No traversal.** An absolute entry name or hard-link target is read
//!   from the image root (`/nix/store/x` is `nix/store/x`, as containerd
//!   reads it). `..` components, in an absolute name too, entries under a
//!   regular file, and `rootfs.manifest.json` at the image root are refused.
//!   No image path reaches the host filesystem until the finished tree is
//!   written beneath owner-only staging inside the CAS root; until then,
//!   extracted file contents are stored only under their BLAKE3 names.
//! - **Name collisions.** Before any layer is read, the CAS root's case and
//!   normalization sensitivity is probed once
//!   ([`crate::cas_volume::probe_sensitivity`]) and reported
//!   ([`ImportedRootfs::cas_root_filesystem`]). Two paths of the finished
//!   tree that the CAS root would store as one name refuse the import
//!   ([`OciImportError::PathCollision`], naming both) before any image path
//!   is written; they are never merged. `crate::cas_volume` provisions a
//!   case-sensitive CAS root.
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
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use leyline_core::ContentAddressed;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::rootfs_manifest::{
    FileSpec, MANIFEST_FILE_MODE, ROOT_DIRECTORY_MODE, ROOTFS_MANIFEST, RootfsSpec,
};
use super::rootfs_tree::{
    FileNode, Final, KERNEL_MOUNTED, Node, PathError, ResolveError, Resolved, Tree, join,
    under_kernel_mount,
};
use crate::DigestRef;
use crate::backends::libkrun::plan::{DirectoryRootfsResolver, RootfsResolver, verify_manifest};
use crate::cas_volume::{FilesystemSensitivity, probe_sensitivity};

/// The default bound on what one import extracts and writes: 16 GiB.
pub const DEFAULT_MAX_ROOTFS_BYTES: u64 = 16 << 30;

/// Index, manifest and config documents larger than this are refused (the
/// image spec's own guidance for manifests is 4 MiB).
pub(super) const MAX_DOCUMENT_BYTES: u64 = 4 << 20;

/// Bound on the entries a `Resolve` expansion may produce.
const MAX_RESOLVED_ENTRIES: usize = 1 << 20;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymlinkPolicy {
    /// Keep each link as a link: rootfs manifest v2 authenticates its target
    /// bytes, which are never followed at import, and the per-run copy
    /// recreates it (bead `ley-line-open-4f08ea`). `etc/mtab ->
    /// /proc/mounts` stays a link the guest's kernel follows, and a link
    /// whose target appears only at run time works once it does.
    #[default]
    Preserve,
    /// Refuse the import, naming the first link in path order.
    Refuse,
    /// Replace each link with a copy of its in-image target. A link into
    /// `dev`, `proc`, `sys` or `tmp`, or one whose target the image does not hold,
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
    /// A regular file (or, under `Resolve`, a link that becomes one) or a
    /// kept symbolic link where the guest init mounts a kernel filesystem
    /// (beads `ley-line-open-2aa5d3`, `ley-line-open-4f08ea`).
    #[error(
        "{path} is not a directory, but the guest init mounts a filesystem over /{path} and mounts only over a real directory: the image would import and never boot"
    )]
    KernelMountPointNotDirectory { path: String },
    #[error("symbolic link {path} cannot be resolved inside the image: {reason}")]
    SymlinkUnresolvable { path: String, reason: String },
    /// Two paths of the finished rootfs that the CAS root's filesystem
    /// compares as one name (bead `ley-line-open-ba86d3`). Detected in the
    /// in-memory tree before any of it is written; never merged.
    #[error(
        "image paths {first} and {second} differ only by {kind}, and the CAS root {} is {kind}-insensitive, so writing both would merge them into one: the image is not at fault. {}",
        .cas_root.display(), .kind.remedy()
    )]
    PathCollision {
        /// The first of the two in path order, relative to the image root.
        first: String,
        second: String,
        kind: NameCollision,
        cas_root: PathBuf,
    },
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
    /// A token realm or redirect that would reach a loopback, private,
    /// link-local or otherwise non-public address (bead
    /// `ley-line-open-b1c18d`). Refused before connecting.
    #[error(
        "{url} resolves to the non-public address {address}: a token realm or redirect may reach only public addresses, the registry itself, or the loopback registry the insecure opt-in names"
    )]
    NonPublicDestination { url: String, address: String },
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

/// How two names a filesystem merges differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NameCollision {
    /// Only by letter case (`Pod`, `pod`).
    Case,
    /// Only by Unicode normalization (NFC and NFD `é`).
    Normalization,
}

impl NameCollision {
    fn remedy(self) -> &'static str {
        match self {
            Self::Case => {
                "Import it into a CAS root on a case-sensitive volume: `leyline rootfs provision-cas` makes one on macOS"
            }
            Self::Normalization => {
                "No APFS volume tells these apart, case-sensitive ones included: import it on a filesystem that compares names byte for byte (Linux ext4, xfs or btrfs)"
            }
        }
    }
}

impl fmt::Display for NameCollision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Case => "case",
            Self::Normalization => "normalization",
        })
    }
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
    /// Directories in the rootfs manifest (v2), the root left out, empty
    /// ones included.
    pub directories: usize,
    /// Symbolic links kept as links in the rootfs manifest (v2), each with
    /// its target bytes (`SymlinkPolicy::Preserve`).
    pub symlinks: usize,
    /// Files whose setuid, setgid or sticky bit was cleared, and directories
    /// whose setuid or setgid bit was (a directory keeps its sticky bit).
    pub special_mode_bits_cleared: usize,
    /// Links replaced by copies of their targets (`SymlinkPolicy::Resolve`),
    /// counting a copy later dropped as unreadable.
    pub symlinks_resolved: usize,
    // The four lists below are per-import provenance, like `image`: two
    // images can give one rootfs digest with different lists, and an
    // import that reuses a CAS entry reports its own. None is part of the
    // rootfs identity.
    /// Device nodes and FIFOs strictly beneath a directory the guest init
    /// mounts a kernel filesystem over (`dev`, `proc`, `sys`, `tmp`), in layer and
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
    /// How the CAS root's filesystem compares names, probed once before any
    /// layer is read. Provenance of this import, not of the rootfs: an image
    /// with no two names that compare equal there imports to the same
    /// digest on any filesystem.
    pub cas_root_filesystem: FilesystemSensitivity,
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
    // Once, before any layer is read: how the CAS root compares names
    // decides which image paths would merge there.
    let cas_root_filesystem = probe_sensitivity(&cas_root).map_err(io_error(
        "probe the CAS root's case and normalization sensitivity",
    ))?;

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

    let as_it_is = |tree: Tree| {
        let (tree, directory_modes) = tree.into_final();
        Resolved {
            tree,
            copied: 0,
            kernel_mount: Vec::new(),
            dangling: Vec::new(),
            directory_modes,
        }
    };
    let resolved = match request.symlinks {
        SymlinkPolicy::Preserve => as_it_is(tree),
        SymlinkPolicy::Refuse => {
            if let Some((path, target)) = tree.first_symlink() {
                return Err(OciImportError::SymlinkRefused { path, target });
            }
            as_it_is(tree)
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
    let directory_modes = resolved.directory_modes;
    let (finished, unreadable) = resolved.tree.without_unreadable_files();
    let unreadable_files_dropped = unreadable
        .into_iter()
        .map(|(path, mode)| DroppedFile { path, mode })
        .collect();
    // The guest init mounts a kernel filesystem over each of these and
    // cannot mount over a regular file (mount(2) fails ENOTDIR), so an image
    // with one there would import and then never boot: refused by name now
    // (bead `ley-line-open-2aa5d3`). A link there is refused too: mount(2)
    // follows it, and init's `mount_tmp` refuses one. A device or FIFO
    // there is refused earlier, a link into a kernel mount dropped under
    // `Resolve`.
    if let Final::Dir(top) = &finished
        && let Some(name) = KERNEL_MOUNTED.iter().find(|name| {
            matches!(
                top.get(**name),
                Some(Final::File(_) | Final::Symlink(_))
            )
        })
    {
        return Err(OciImportError::KernelMountPointNotDirectory {
            path: (*name).to_string(),
        });
    }
    // Before any image path is written: a pair the CAS root would merge is
    // refused, never merged. Staging (content and layers only) is removed
    // with `staging` on this return.
    if let Some((first, second, kind)) = first_collision(&finished, cas_root_filesystem) {
        return Err(OciImportError::PathCollision {
            first,
            second,
            kind,
            cas_root,
        });
    }

    let staged_root = staging.path().join("rootfs");
    let written = write_rootfs(
        &staged_root,
        &finished,
        &directory_modes,
        &store,
        request.max_rootfs_bytes,
    )?;
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
        directories: written.directories,
        symlinks: written.symlinks,
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
        cas_root_filesystem,
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
    /// A directory entry and its mode: a directory over an existing one
    /// keeps its children and takes this mode.
    Directory(Vec<String>, u32),
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
                Change::Directory(path, mode) => tree
                    .insert(&path, Node::Dir(Default::default()))
                    .map(|()| tree.set_dir_mode(&path, mode)),
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

/// The components of an archive path (an entry's name or a hard link's
/// target), relative to the image root. `.` and empty components
/// (`./a//b/`) are dropped, so a leading `/` is the image root, as
/// containerd and Docker read an absolute name (Nix `dockerTools` writes
/// `/nix/store/...`; bead `ley-line-open-cffbb5`): that changes how a name
/// is read, never where an entry may land. A `..` component is refused,
/// never normalized, in an absolute name as in a relative one.
fn guest_path(raw: &str) -> Result<Vec<String>, &'static str> {
    // PATH_MAX. Also bounds the tree's depth, which every pass over it
    // recurses through.
    if raw.len() > MAX_PATH_BYTES {
        return Err("paths longer than 4096 bytes are refused");
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
                // Rootfs manifest v2 carries the mode (with owner rwx set;
                // see `manifest_directory_mode`). The root's is fixed.
                let mode = entry
                    .header()
                    .mode()
                    .map_err(io_error("read an entry mode"))?
                    & 0o7777;
                if !path.is_empty() {
                    changes
                        .entries
                        .push((raw, Change::Directory(path, mode)));
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
                            "layer {layer} entry {raw}: {kind:?} entries are not expressible in rootfs manifest v1, and only those beneath dev/, proc/, sys/ or tmp/ (which the guest init mounts kernel filesystems over) are dropped"
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

/// The name a filesystem of `sensitivity` stores `name` under, as far as
/// comparing two names goes: NFD when it ignores normalization, Unicode
/// full case folding when it ignores case. The filesystem's own tables may
/// differ in a corner (a Unicode version); a pair this misses still fails
/// when the second is written, inside staging, before anything is published.
fn folded(name: &str, sensitivity: FilesystemSensitivity) -> String {
    use unicode_normalization::UnicodeNormalization;
    let normalize = |name: &str| -> String {
        if sensitivity.normalization_sensitive {
            name.to_string()
        } else {
            name.nfd().collect()
        }
    };
    let mut key = normalize(name);
    if !sensitivity.case_sensitive {
        // Full Unicode case folding (CaseFolding.txt C+F), as APFS folds:
        // `ß` and `ss`, final `ς` and `σ`, `ſ` and `s` are one name there.
        key = normalize(&unicase::UniCase::new(key).to_folded_case());
    }
    key
}

/// The first two paths of `tree`, in path order, that one directory of a
/// filesystem of `sensitivity` would hold as one name, and how they differ.
/// The root also holds `rootfs.manifest.json`, which the import writes.
fn first_collision(
    tree: &Final,
    sensitivity: FilesystemSensitivity,
) -> Option<(String, String, NameCollision)> {
    use unicode_normalization::UnicodeNormalization;
    fn walk(
        entries: &BTreeMap<String, Final>,
        prefix: &str,
        sensitivity: FilesystemSensitivity,
        extra: Option<&str>,
    ) -> Option<(String, String, NameCollision)> {
        let mut seen: BTreeMap<String, &str> = BTreeMap::new();
        for name in extra.into_iter().chain(entries.keys().map(String::as_str)) {
            if let Some(earlier) = seen.insert(folded(name, sensitivity), name) {
                let kind = if !sensitivity.normalization_sensitive && earlier.nfd().eq(name.nfd()) {
                    NameCollision::Normalization
                } else {
                    NameCollision::Case
                };
                return Some((join(prefix, earlier), join(prefix, name), kind));
            }
        }
        entries.iter().find_map(|(name, node)| match node {
            Final::Dir(children) => walk(children, &join(prefix, name), sensitivity, None),
            Final::File(_) | Final::Symlink(_) => None,
        })
    }
    if sensitivity == FilesystemSensitivity::EXACT {
        return None;
    }
    match tree {
        Final::Dir(entries) => walk(entries, "", sensitivity, Some(ROOTFS_MANIFEST)),
        Final::File(_) | Final::Symlink(_) => None,
    }
}

struct Written {
    digest: String,
    files: usize,
    /// Directories written, the root left out.
    directories: usize,
    /// Symbolic links written as links.
    symlinks: usize,
    special_mode_bits_cleared: usize,
}

/// A directory's mode in rootfs manifest v2, from the mode its archive
/// entry gave it (`0755` for one only a deeper entry implies): permission
/// bits and the sticky bit (`/tmp` and `/var/tmp` are `1777`); setuid and
/// setgid are cleared, and whether they were is returned; owner read, write
/// and search are always set, since the unprivileged host user writes and
/// removes every copy of the tree, and the guest's root ignores owner bits.
fn manifest_directory_mode(mode: Option<u32>) -> (u32, bool) {
    let mode = mode.unwrap_or(0o755);
    let cleared = mode & 0o6000 != 0;
    ((mode & 0o1777) | 0o700, cleared)
}

struct Writer<'a> {
    store: &'a ContentStore,
    limit: u64,
    written: u64,
    /// Each directory's mode from its archive entry, by written path.
    entry_modes: &'a BTreeMap<String, u32>,
    spec: RootfsSpec,
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
                    let (mode, cleared) =
                        manifest_directory_mode(self.entry_modes.get(&relative).copied());
                    self.special_mode_bits_cleared += usize::from(cleared);
                    // Set after its children are written: a mode without
                    // owner write would have refused them.
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                        .map_err(io_error("set a rootfs directory's mode"))?;
                    self.spec.directories.insert(relative, mode);
                }
                Final::File(file) => self.file(&path, relative, file)?,
                Final::Symlink(target) => {
                    if target.is_empty() {
                        return Err(OciImportError::SymlinkUnresolvable {
                            path: relative,
                            reason: "the link's target is empty".into(),
                        });
                    }
                    std::os::unix::fs::symlink(target, &path).map_err(io_error(
                        "write a rootfs link (a case- or normalization-insensitive host filesystem merges image paths that differ only in case)",
                    ))?;
                    self.spec.symlinks.insert(relative, target.clone());
                }
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
        self.spec.files.insert(
            relative,
            FileSpec {
                mode,
                blake3: file.content.to_hex().to_string(),
            },
        );
        Ok(())
    }
}

/// Create a directory owner-only; its manifest mode is set once its
/// children are written.
fn create_directory(path: &Path) -> Result<(), OciImportError> {
    fs::create_dir(path).map_err(io_error(
        "create a rootfs directory (a case- or normalization-insensitive host filesystem merges image paths that differ only in case)",
    ))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(io_error("set a rootfs directory's mode"))
}

/// Write the finished tree and its rootfs manifest v2 at `root`, returning
/// the manifest's BLAKE3. `entry_modes` holds each directory's archive mode
/// by written path.
fn write_rootfs(
    root: &Path,
    finished: &Final,
    entry_modes: &BTreeMap<String, u32>,
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
        entry_modes,
        spec: RootfsSpec::empty_v2(),
        special_mode_bits_cleared: 0,
    };
    writer.directory(root, "", entries)?;
    fs::set_permissions(root, fs::Permissions::from_mode(ROOT_DIRECTORY_MODE))
        .map_err(io_error("set the rootfs root's mode"))?;
    // The canonical bytes: the only ones a reader accepts for this tree.
    let manifest = writer.spec.to_canonical_v2();
    let manifest_path = root.join(ROOTFS_MANIFEST);
    fs::write(&manifest_path, &manifest).map_err(io_error("write the rootfs manifest"))?;
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(MANIFEST_FILE_MODE))
        .map_err(io_error("set the rootfs manifest's mode"))?;
    Ok(Written {
        // σ_bytes over the manifest: the identity `verify_manifest` checks.
        digest: manifest.as_slice().hash().to_string(),
        files: writer.spec.files.len(),
        directories: writer.spec.directories.len() - 1,
        symlinks: writer.spec.symlinks.len(),
        special_mode_bits_cleared: writer.special_mode_bits_cleared,
    })
}

/// Rename the verified staged tree to `<cas-root>/<digest>`. An entry that
/// is already there is reused only if the CAS resolver verifies it, which
/// under rootfs manifest v2 covers every file, directory (with its mode)
/// and link: it is then this tree, and it is never overwritten.
fn publish(cas_root: &Path, staged: &Path, rootfs: &DigestRef) -> Result<(), OciImportError> {
    let destination = cas_root.join(&rootfs.value);
    if destination.symlink_metadata().is_err() && fs::rename(staged, &destination).is_ok() {
        return Ok(());
    }
    // Rootfs manifest v2 names every directory with its mode and every
    // link, so the resolver's verification is exact: an entry that passes
    // it is this tree.
    DirectoryRootfsResolver::new(cas_root)
        .resolve(rootfs)
        .map_err(|error| {
            OciImportError::Cas(format!(
                "{} already exists and does not verify as rootfs {}; refusing to replace it: {}",
                destination.display(),
                rootfs.value,
                error.detail
            ))
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_APFS: FilesystemSensitivity = FilesystemSensitivity {
        case_sensitive: false,
        normalization_sensitive: false,
    };
    const CASE_SENSITIVE_APFS: FilesystemSensitivity = FilesystemSensitivity {
        case_sensitive: true,
        normalization_sensitive: false,
    };
    const CASE_ONLY: FilesystemSensitivity = FilesystemSensitivity {
        case_sensitive: false,
        normalization_sensitive: true,
    };

    fn file() -> Final {
        Final::File(FileNode {
            content: blake3::hash(b""),
            size: 0,
            mode: 0o644,
        })
    }

    /// A tree holding a file at each path.
    fn tree(paths: &[&str]) -> Final {
        fn insert(entries: &mut BTreeMap<String, Final>, components: &[&str]) {
            match components {
                [] => {}
                [name] => {
                    entries.insert(name.to_string(), file());
                }
                [name, rest @ ..] => {
                    let child = entries
                        .entry(name.to_string())
                        .or_insert_with(|| Final::Dir(BTreeMap::new()));
                    let Final::Dir(children) = child else {
                        panic!("{name} is a file")
                    };
                    insert(children, rest);
                }
            }
        }
        let mut root = BTreeMap::new();
        for path in paths {
            insert(&mut root, &path.split('/').collect::<Vec<_>>());
        }
        Final::Dir(root)
    }

    fn collision(
        paths: &[&str],
        sensitivity: FilesystemSensitivity,
    ) -> Option<(String, String, NameCollision)> {
        first_collision(&tree(paths), sensitivity)
    }

    #[test]
    fn names_differing_only_by_case_collide_where_case_is_ignored() {
        let perl = [
            "usr/share/perl/Pod/Usage.pm",
            "usr/share/perl/pod/perlfunc.pod",
        ];
        let expected = Some((
            "usr/share/perl/Pod".to_string(),
            "usr/share/perl/pod".to_string(),
            NameCollision::Case,
        ));
        assert_eq!(collision(&perl, DEFAULT_APFS), expected);
        assert_eq!(collision(&perl, CASE_ONLY), expected);
        assert_eq!(collision(&perl, CASE_SENSITIVE_APFS), None);
        assert_eq!(collision(&perl, FilesystemSensitivity::EXACT), None);
        // Full case folding, as APFS folds (each pair verified to name one
        // file on case-insensitive APFS): final sigma, long s, sharp s and
        // the ff ligature.
        for (a, b) in [
            ("a\u{3c3}", "a\u{3c2}"),
            ("x\u{3a3}", "x\u{3c3}"),
            ("\u{17f}", "s"),
            ("\u{df}", "ss"),
            ("\u{fb00}", "ff"),
        ] {
            assert_eq!(
                collision(&[a, b], DEFAULT_APFS).map(|found| found.2),
                Some(NameCollision::Case),
                "{a:?} and {b:?}"
            );
            assert_eq!(collision(&[a, b], CASE_SENSITIVE_APFS), None);
        }
        // A file and a directory collide as two files do.
        assert_eq!(
            collision(&["lib/Sys", "lib/sys/x"], DEFAULT_APFS),
            Some(("lib/Sys".into(), "lib/sys".into(), NameCollision::Case))
        );
    }

    #[test]
    fn names_differing_only_by_normalization_collide_where_it_is_ignored() {
        let cafe = ["caf\u{e9}", "cafe\u{301}"];
        let expected = Some((
            "cafe\u{301}".to_string(),
            "caf\u{e9}".to_string(),
            NameCollision::Normalization,
        ));
        assert_eq!(collision(&cafe, DEFAULT_APFS), expected);
        assert_eq!(collision(&cafe, CASE_SENSITIVE_APFS), expected);
        assert_eq!(collision(&cafe, CASE_ONLY), None);
        // Case and normalization together fold where both are ignored.
        assert_eq!(
            collision(&["CAF\u{c9}", "cafe\u{301}"], DEFAULT_APFS).map(|found| found.2),
            Some(NameCollision::Case)
        );
        assert_eq!(
            collision(&["CAF\u{c9}", "cafe\u{301}"], CASE_SENSITIVE_APFS),
            None
        );
    }

    #[test]
    fn an_image_file_that_folds_to_the_manifest_name_collides_at_the_root_only() {
        assert_eq!(
            collision(&["Rootfs.Manifest.json"], DEFAULT_APFS),
            Some((
                ROOTFS_MANIFEST.to_string(),
                "Rootfs.Manifest.json".to_string(),
                NameCollision::Case
            ))
        );
        assert_eq!(
            collision(&["Rootfs.Manifest.json"], CASE_SENSITIVE_APFS),
            None
        );
        assert_eq!(collision(&["etc/Rootfs.Manifest.json"], DEFAULT_APFS), None);
    }

    #[test]
    fn distinct_names_never_collide_and_deeper_pairs_are_found() {
        assert_eq!(
            collision(
                &["bin/sh", "bin/bash", "etc/passwd", "etc/group"],
                DEFAULT_APFS
            ),
            None
        );
        assert_eq!(
            collision(&["a/b/c/X", "a/b/c/x", "z/Y", "z/y"], DEFAULT_APFS),
            Some(("a/b/c/X".into(), "a/b/c/x".into(), NameCollision::Case))
        );
    }

    #[test]
    fn the_refusal_names_both_paths_the_cas_root_and_its_insensitivity() {
        let error = OciImportError::PathCollision {
            first: "usr/share/perl/Pod".into(),
            second: "usr/share/perl/pod".into(),
            kind: NameCollision::Case,
            cas_root: PathBuf::from("/cas"),
        };
        let message = error.to_string();
        for needle in [
            "usr/share/perl/Pod",
            "usr/share/perl/pod",
            "the CAS root /cas is case-insensitive",
            "the image is not at fault",
            "leyline rootfs provision-cas",
        ] {
            assert!(
                message.contains(needle),
                "{needle:?} missing from: {message}"
            );
        }
        let message = OciImportError::PathCollision {
            first: "a".into(),
            second: "b".into(),
            kind: NameCollision::Normalization,
            cas_root: PathBuf::from("/cas"),
        }
        .to_string();
        assert!(
            message.contains("is normalization-insensitive"),
            "{message}"
        );
        assert!(message.contains("No APFS volume"), "{message}");
    }
}
