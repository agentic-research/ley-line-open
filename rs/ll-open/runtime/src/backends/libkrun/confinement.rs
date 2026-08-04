use std::path::{Path, PathBuf};

use nono::{AccessMode, CapabilitySet, Sandbox};

use crate::ExecutionError;
use crate::confinement::{ConfinementManifest, Dimensions, FsGrant};

use super::plan::KrunConfig;

/// Host resources required after the worker irreversibly drops ambient
/// authority. Runtime files are read-only; device nodes require read/write
/// access for the virtualization API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VmmHostResources {
    pub runtime_files: Vec<PathBuf>,
    pub devices: Vec<PathBuf>,
}

pub fn build_capabilities(
    config: &KrunConfig,
    resources: &VmmHostResources,
) -> Result<CapabilitySet, ExecutionError> {
    build_process_capabilities(
        &config.rootfs.canonical_path,
        &resources.runtime_files,
        &resources.devices,
    )
}

/// The `confinement/v1` manifest this backend compiles for one worker.
///
/// The common fail-closed policy for a native or VMM worker: the rootfs is
/// the only read/write tree, runtime libraries are read-only, device paths
/// are explicitly read/write, and no network capability is granted. Keeping
/// it independent of libkrun stops a native nono backend from silently
/// widening authority as that backend is built out.
///
/// ADR-0035 §1: the applied `CapabilitySet` and the declared
/// `confinementDigest` must be projections of one object. This is that
/// object. `build_process_capabilities` derives the CapabilitySet *from* it
/// rather than beside it, so a policy change that skipped the manifest would
/// not compile.
///
/// The trailing slash on the rootfs is load-bearing: `confinement/v1` §2
/// distinguishes a directory subtree from a single file by it, and that
/// distinction is exactly nono's `allow_path` vs `allow_file`. Encoding it in
/// the path rather than in a separate flag keeps the manifest self-describing
/// — a reader of the JSON can tell which grant a path is.
/// The path the attested manifest names for the run's writable root.
///
/// Symbolic, and deliberately not the host's realization. The materialized
/// rootfs lives at `<run_root>/rootfs` where `run_root` is a per-run temporary
/// directory, so naming it here would put a fresh random component into every
/// digest — and a digest nobody can predict is a digest no issuer can commit
/// to. `RunGrant.confinementDigest` would then have no satisfying value: the
/// drift check could only ever reject.
///
/// Not a loss of coverage. What matters about the rootfs is its CONTENT, and
/// that is attested independently — `ResolvedRootfs.digest` is verified against
/// the manifest by `verify_ephemeral_rootfs` before anything runs, and reaches
/// the receipt as `inputRoots`. The host path is an implementation detail of
/// where those verified bytes were placed.
///
/// `confinement/v1`'s own pinned vector shows the intended shape — stable,
/// meaningful paths like `/var/lib/bundle-X/`, not tempdirs.
pub const ATTESTED_RUN_ROOTFS: &str = "/run/rootfs/";

pub fn confinement_manifest(
    runtime_files: &[PathBuf],
    devices: &[PathBuf],
) -> Result<ConfinementManifest, ExecutionError> {
    // Fallible now that §2 refuses relative and traversing paths at the point
    // the grant is made. A caller handing us a relative `--runtime-file` gets
    // an error naming the path, instead of a manifest that digests cleanly and
    // describes a grant the spec forbids.
    let mut manifest =
        ConfinementManifest::new().with_fs_grant(FsGrant::read_write(ATTESTED_RUN_ROOTFS))?;
    for path in runtime_files {
        manifest = manifest.with_fs_grant(FsGrant::read_only(path.display().to_string()))?;
    }
    for path in devices {
        manifest = manifest.with_fs_grant(FsGrant::read_write(path.display().to_string()))?;
    }
    // No `network` block at all. §3: an omitted block means no egress, which
    // is what `block_network()` enforces — so declaring an empty allow-list
    // would say the same thing twice, in two places that could disagree.
    Ok(manifest)
}

pub fn build_process_capabilities(
    rootfs: &Path,
    runtime_files: &[PathBuf],
    devices: &[PathBuf],
) -> Result<CapabilitySet, ExecutionError> {
    capabilities_from_manifest(&confinement_manifest(runtime_files, devices)?, rootfs)
}

/// Compile a manifest into the `CapabilitySet` nono applies.
///
/// A trailing slash selects `allow_path` (directory subtree); anything else
/// is `allow_file`. nono rejects a directory passed to `allow_file` and a
/// non-directory passed to `allow_path`, so a manifest that mislabels a path
/// fails here rather than granting the wrong shape.
///
/// This compiles confinement/v1 **directly**, deliberately bypassing nono's own
/// `CapabilityManifest` and its `TryFrom<&CapabilityManifest> for CapabilitySet`.
/// The two manifest shapes diverge structurally — `credentials` alone is an array
/// of objects against confinement/v1's scalar `credentialSource`, so no
/// field-for-field mapping exists — and routing through nono's would put a second
/// shape between the object whose digest is attested and the policy actually
/// applied, which is the drift the single manifest exists to prevent. The full
/// field-by-field mapping and the rationale are ADR-0035's second open question
/// (bead `ley-line-open-c17486`).
pub fn capabilities_from_manifest(
    manifest: &ConfinementManifest,
    run_rootfs: &Path,
) -> Result<CapabilitySet, ExecutionError> {
    // Exhaustive by construction: no `..`, so a fifth dimension is a compile
    // error here rather than a clause this function silently ignores. §5 is in
    // this list because it was NOT before — `credential_source` had no accessor
    // at all, so a manifest could declare a vault binding that nothing read and
    // the attested digest still committed to it.
    let Dimensions {
        fs_allow,
        allow_hosts,
        port,
        credential_source,
    } = manifest.dimensions();

    let mut capabilities = CapabilitySet::new();
    for grant in fs_allow {
        let (path, mode) = match grant {
            FsGrant::ReadOnly(path) => (path.as_str(), AccessMode::Read),
            FsGrant::ReadWrite { path } => (path.as_str(), AccessMode::ReadWrite),
        };
        // The one substitution: the attested document names the symbolic root,
        // the applied policy names where those verified bytes actually landed.
        // Everything else is a real host path already and passes through.
        let resolved = if path == ATTESTED_RUN_ROOTFS {
            format!("{}/", run_rootfs.display())
        } else {
            path.to_owned()
        };
        let path = resolved.as_str();
        capabilities = if let Some(directory) = path.strip_suffix('/') {
            capabilities
                .allow_path(Path::new(directory), mode)
                .map_err(nono_error)?
        } else {
            capabilities
                .allow_file(Path::new(path), mode)
                .map_err(nono_error)?
        };
    }

    // §3 egress. nono's CapabilitySet filters by PORT, never by hostname —
    // host-based egress is nono's *manifest* `network.allow_domains`, which
    // rides its proxy path, not the `apply_auto` path this compiles to. So a
    // manifest declaring allowHosts describes something this tier cannot
    // enforce, and the answer is to say so rather than to block everything and
    // let the digest attest a policy that never took effect.
    //
    // The failure direction is closed either way — dropping the grant leaves
    // the sandbox stricter than the manifest. That is exactly why it needs an
    // error: a silent over-restriction still means the attested confinement
    // digest commits to a document whose §3 had no effect, which is the drift
    // ADR-0035 exists to prevent, one dimension over.
    if !allow_hosts.is_empty() {
        return Err(ExecutionError::invalid(format!(
            "confinement/v1 §3 network.allowHosts is not enforceable on this tier: \
             nono filters by port, not by hostname, so the {} declared host(s) \
             would be silently dropped. Route host-scoped egress through the \
             proxy, or omit the dimension.",
            allow_hosts.len()
        )));
    }

    // §5 credential binding. LLO does not route through nono's keystore on the
    // `apply_auto` path, so a declared vault backend has no reader here.
    //
    // This arm is the one the `Dimensions` destructure exists for. Before it,
    // §5 was not merely unenforced — it was unreadable, with no accessor on the
    // manifest at all, so `capabilities_from_manifest` could not have refused it
    // even if someone had thought to. A test file asserting "every dimension is
    // enforced or refused" covered three of four and looked complete.
    if let Some(source) = credential_source {
        return Err(ExecutionError::invalid(format!(
            "confinement/v1 §5 credentialSource {source:?} is not enforceable on \
             this tier: the apply_auto path compiles to a CapabilitySet, which \
             carries no credential binding, so the vault would be declared and \
             never applied. Vend credentials through the proxy, or omit the \
             dimension."
        )));
    }

    // §4 listener. On Linux this is exactly the shape cloister-harness runs
    // today — deny everything, then open one loopback port to the vault-proxy
    // shim. On macOS it cannot be expressed at all; see below.
    if let Some((bind, address)) = port {
        // §4 defaults to 127.0.0.1 and requires anything wider to be declared
        // explicitly. That declaration is precisely what cannot be honoured
        // here: with Landlock filtering on port alone, a grant for 0.0.0.0 and
        // a grant for loopback compile to the identical rule, so accepting the
        // wider one would attest an exposure decision nothing enforces.
        let loopback = matches!(
            address,
            None | Some("127.0.0.1") | Some("localhost") | Some("::1")
        );
        if !loopback {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §4 port.address {:?} is not enforceable on this tier: \
                 nono filters by port, not by bind address, so a non-loopback \
                 listener compiles to the same rule as a loopback one. §4 requires \
                 exposure beyond loopback to be an explicit declaration, and this \
                 tier cannot honour that declaration.",
                address.unwrap_or_default()
            )));
        }
        // Seatbelt cannot filter bind or inbound by port. nono says so in its
        // own source and then emits the blanket rule anyway, because there is
        // nothing narrower to emit:
        //
        //   // Seatbelt cannot filter bind/inbound by port
        //   (allow network-bind)
        //   (allow network-inbound)
        //     — nono-0.71.0/src/sandbox/macos.rs:838-840, NetworkMode::Blocked
        //       with has_localhost_tcp, which is precisely this configuration.
        //
        // Seatbelt scopes the OUTBOUND direction per port. §4 is about the bind
        // direction, and there it is all-or-nothing: declaring one listener
        // would grant the workload every port on every address.
        //
        // This refusal is the §3/§4 rule applied to itself. An earlier version
        // of this function granted the port here and refused only a non-loopback
        // ADDRESS, on the reasoning that "a non-loopback listener compiles to
        // the same rule as a loopback one, so accepting the wider one would
        // attest an exposure decision nothing enforces". That argument is
        // stronger on the port axis on macOS — `bind: 8443` and `bind: anything`
        // compile identically — and it was applied to the address axis on the
        // platform where the port axis is the broken one.
        #[cfg(target_os = "macos")]
        {
            let _ = bind;
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §4 port.bind {bind} is not enforceable on Seatbelt: \
                 macOS cannot filter bind by port, so nono emits an unqualified \
                 `(allow network-bind)` and declaring one listener would grant \
                 every listener on every address. Run the listener tier on Linux, \
                 or omit the dimension."
            )));
        }
        #[cfg(not(target_os = "macos"))]
        {
            // Landlock filters per port (`NetPort::new(port, AccessNet::BindTcp)`),
            // so the grant means what §4 says. Loopback-only is a property of the
            // `block_network()` below rather than of this call — Landlock filters
            // by port, not by destination IP.
            capabilities = capabilities.allow_localhost_port(bind);
        }
    }

    Ok(capabilities.block_network())
}

/// Apply nono to the current worker process. This is irreversible and must be
/// called only after the worker has loaded libkrun and resolved its rootfs.
pub fn apply(config: &KrunConfig, resources: &VmmHostResources) -> Result<(), ExecutionError> {
    apply_manifest(
        &confinement_manifest(&resources.runtime_files, &resources.devices)?,
        &config.rootfs.canonical_path,
    )
}

/// Apply the policy described by exactly this manifest.
///
/// Takes the manifest rather than the inputs it is built from, so a caller that
/// attests a digest can apply *that* document instead of a second one derived
/// from the same arguments. `apply` above keeps the old shape for callers with
/// no digest to attest; the worker uses this one.
pub fn apply_manifest(
    manifest: &ConfinementManifest,
    run_rootfs: &Path,
) -> Result<(), ExecutionError> {
    let support = Sandbox::support_info();
    if !support.is_supported {
        return Err(ExecutionError::backend(format!(
            "nono sandbox is unavailable on {}: {}",
            support.platform, support.details
        )));
    }
    let capabilities = capabilities_from_manifest(manifest, run_rootfs)?;
    Sandbox::apply_auto(&capabilities)
        .map(|_| ())
        .map_err(nono_error)
}

fn nono_error(error: nono::NonoError) -> ExecutionError {
    ExecutionError::backend(format!("apply nono VMM confinement: {error}"))
}
