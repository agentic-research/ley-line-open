use std::path::{Path, PathBuf};

use nono::{AccessMode, CapabilitySet, Sandbox, UnixSocketMode};

use crate::ExecutionError;
use crate::confinement::{ConfinementManifest, Dimensions, FsGrant, UnixSocketGrant};

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
    authorized: Option<&ConfinementManifest>,
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
    // §4 is the one dimension a grant can originate, and the ONLY source is the
    // authorized document — never the `ExecutionRequest`, which is caller
    // intent. `authorization.rs` has already refused any grant whose carried
    // document does not digest to the `confinementDigest` it names, so what
    // arrives here has an issuer behind it.
    //
    // Taken INTO the manifest rather than applied beside it, which is what
    // keeps ADR-0035 §1 true: the digest the worker attests is computed over
    // this document, so the listener is inside the object the receipt commits
    // to instead of being a grant nothing describes.
    //
    // The digest therefore changes when a listener is declared, and an issuer
    // has to predict it. That is achievable precisely because the other inputs
    // are stable: the rootfs is symbolic (ATTESTED_RUN_ROOTFS), and runtime
    // files and devices come from `LibkrunBackendConfig` — deployment
    // configuration fixed when the backend is constructed, not per-run values.
    // An issuer configured for a deployment can compute this document exactly.
    //
    // §2, §3, §5 and §6 are deliberately NOT merged. Each would either widen
    // what LLO grants itself or is refused by the compiler on one of the two
    // tiers, and merging a dimension whose tier refuses it would turn a clean
    // refusal into a digest mismatch — a worse diagnostic for the same outcome.
    // §4 is the dimension the microVM tier can actually deliver.
    if let Some((bind, address)) = authorized.and_then(|m| m.dimensions().port) {
        manifest = manifest.with_port_bind(bind, address)?;
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
    capabilities_from_manifest(&confinement_manifest(runtime_files, devices, None)?, rootfs)
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
        unix_sockets,
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

    // §6 local channels. Enforceable on Seatbelt, and NOT on Landlock — the
    // exact mirror of §4, which is enforceable on Landlock and not on Seatbelt.
    //
    // I wrote the opposite here first, and it was wrong. Be precise about *why*
    // it is refused, because the imprecise version of this claim expires:
    //
    //   - `AccessNet` is `BindTcp | ConnectTcp` and nothing else, at every ABI
    //     the crate knows (landlock-0.4.5/src/net.rs:45, `from_all` for V4..V7).
    //     No network right ever covers AF_UNIX. That part is durable.
    //   - But pathname-AF_UNIX *is* expressible on Linux as of Landlock ABI 9
    //     (kernel 7.1): `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`, an AccessFs right,
    //     restricts `connect(2)` and `sendmsg(2)`-with-recipient per path.
    //     So "Landlock cannot express §6" is false in general and must not be
    //     written down as if it were a property of the dimension.
    //
    // What is true *here* is narrower and entirely about this stack:
    //   - landlock 0.4.5 tops out at ABI V7 (compat.rs:57-75); it has no
    //     `RESOLVE_UNIX` constant to emit.
    //   - nono targets ABI 5, and reads `unix_socket_capabilities()` in exactly
    //     one enforcement backend — `sandbox/macos.rs:429` — and zero times in
    //     `sandbox/linux.rs`.
    // A §6 grant therefore compiles to nothing on this tier today.
    //
    // So the refusal is not conservatism, it is the §3/§5 rule applied to the
    // dimension I added while fixing that very defect: a clause a tier cannot
    // enforce must be a rejection, never a silent pass-through into an
    // identity-committed digest. Routing a channel through a socket rather than
    // a port does not close the platform gap — it swaps which platform has one.
    #[cfg(target_os = "linux")]
    if !unix_sockets.is_empty() {
        return Err(ExecutionError::invalid(format!(
            "confinement/v1 §6 unixSocket.allow is not enforceable by the \
             Landlock ABI this build targets, so the {} declared socket \
             grant(s) would compile to nothing while the attested digest \
             committed to them. Per-path AF_UNIX mediation exists upstream as \
             LANDLOCK_ACCESS_FS_RESOLVE_UNIX (ABI 9, kernel 7.1), but the \
             landlock crate here stops at ABI 7 and nono targets ABI 5 and \
             consults unix_socket_capabilities() only in its macOS backend. \
             Use §4 port.bind on this tier, which Landlock does filter per port.",
            unix_sockets.len()
        )));
    }

    for grant in unix_sockets {
        let path = grant.path();
        let mode = unix_socket_mode(grant)?;

        // Trailing slash is a directory of sockets, exactly as in §2. The
        // spelling is shared on purpose: a reader of the JSON should not have
        // to learn two conventions for "this names a tree, not a leaf".
        capabilities = if let Some(directory) = path.strip_suffix('/') {
            capabilities
                .allow_unix_socket_dir(Path::new(directory), mode)
                .map_err(nono_error)?
        } else {
            capabilities
                .allow_unix_socket(Path::new(path), mode)
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

/// Resolve one §6 grant to the `UnixSocketMode` it compiles to, or refuse it.
///
/// Split out of `capabilities_from_manifest` because both refusals below are
/// properties of the DIMENSION, not of the tier, while the caller's §6 arm is
/// platform-gated: on Linux it refuses every socket grant for the Landlock ABI
/// reason, so nothing past that arm is reachable there. Inline, that made both
/// conditions dead code on the only platform CI runs — which is exactly what
/// cargo-mutants reported, as surviving mutants that no test driving the public
/// compile path could kill, because on Linux there is no path to drive.
///
/// A free function is the smallest thing that fixes that: the decisions stay
/// where they were, and they become reachable from a test on every platform
/// without either pretending the compile path is portable or relaxing the
/// Linux refusal to make room for a test.
fn unix_socket_mode(grant: &UnixSocketGrant) -> Result<UnixSocketMode, ExecutionError> {
    let path = grant.path();

    // §6 `bind` is serve-without-dial: it grants `bind(2)` and WITHHOLDS
    // `connect(2)`. nono's `UnixSocketMode` has no bind-only member — the
    // pair is `Connect | ConnectBind` — so neither available value carries
    // the mode. `ConnectBind` would add the `connect(2)` the grant exists to
    // withhold, and `Connect` inverts it outright. Widening is the failure
    // this dimension was added while fixing, so refuse.
    //
    // The mode is not unenforceable in general: the hypervisor tier does
    // enforce exactly it, because a `krun_add_vsock_port2` mapping with
    // `listen=true` answers a guest-originated connect with a reset. That is
    // a boundary, not a filter — it enforces by not constructing the dial
    // path at all — which is why it can hold a clause a filter cannot.
    if !grant.permits_connect() {
        return Err(ExecutionError::invalid(format!(
            "confinement/v1 §6 unixSocket.allow mode {:?} for {path:?} is not \
             enforceable through a CapabilitySet: the only modes available are \
             connect and connect-bind, and granting connect-bind would add the \
             connect(2) this mode exists to withhold. Serve-without-dial is a \
             hypervisor-tier construction (a listen=true vsock port mapping), \
             not a sandbox filter.",
            grant.mode()
        )));
    }

    let mode = if grant.permits_bind() {
        UnixSocketMode::ConnectBind
    } else {
        UnixSocketMode::Connect
    };

    // §6 `connect` names an endpoint SOMEONE ELSE owns, and nono requires it
    // to exist: `UnixSocketCapability::new_file` canonicalizes the path, and
    // it only tolerates a missing leaf when the mode permits `bind(2)` — its
    // reasoning being that bind will create the file. That reasoning does not
    // cover the case this dimension exists for, where the peer binds and the
    // grantee only dials.
    //
    // The requirement is nonetheless real and must not be papered over. The
    // two ways to "fix" it here are both widenings: granting the parent
    // directory would cover every socket in it, and passing ConnectBind would
    // add the `bind(2)` a connect grant deliberately withholds. Canonicalization
    // is also load-bearing rather than incidental — resolving the leaf is what
    // stops a symlink planted at the path from redirecting the grant to some
    // other endpoint, which is exactly the §6 redirect hazard.
    //
    // So the endpoint must be bound before confinement is applied. That is an
    // ordering contract (README §6), and the deployment satisfies it naturally:
    // the proxy binds, then the confined child is spawned. What was wrong was
    // letting it surface as `BackendFailed: ... Path does not exist`, which
    // reads as an internal fault rather than as the contract it is. Check it
    // here so the diagnostic names the dimension, the path, and the ordering.
    if !grant.permits_bind()
        && let Some(socket) = path.strip_suffix('/').is_none().then_some(path)
        && !Path::new(socket).exists()
    {
        return Err(ExecutionError::invalid(format!(
            "confinement/v1 §6 unixSocket.allow grants connect(2) on {socket:?}, \
             but nothing is bound there yet. A connect grant names an endpoint \
             owned by another process, and it is resolved — not merely recorded \
             — when confinement is applied, so that a symlink planted at the \
             path cannot redirect the grant. Bind the endpoint before starting \
             the confined workload. This is an ordering requirement, not a \
             malformed manifest: the same document compiles once the peer is up."
        )));
    }

    Ok(mode)
}

/// Apply nono to the current worker process. This is irreversible and must be
/// called only after the worker has loaded libkrun and resolved its rootfs.
pub fn apply(config: &KrunConfig, resources: &VmmHostResources) -> Result<(), ExecutionError> {
    apply_manifest(
        &confinement_manifest(&resources.runtime_files, &resources.devices, None)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A leaf path that exists, standing in for an endpoint a peer has already
    /// bound. The refusal reads `Path::exists`, not "is a socket", so the test
    /// binary itself is a faithful stand-in — and unlike a temp file or a real
    /// `UnixListener` it cannot race, leak, or blow the ~104-byte `sun_path`
    /// limit that `std::env::temp_dir()` risks on macOS.
    fn bound_endpoint() -> String {
        std::env::current_exe()
            .expect("test binary has a path")
            .display()
            .to_string()
    }

    /// A leaf path that does not exist, standing in for an endpoint whose peer
    /// has not bound yet.
    fn unbound_endpoint() -> String {
        let path =
            std::env::temp_dir().join(format!("llo-uds-unbound-{}.sock", std::process::id()));
        assert!(
            !path.exists(),
            "fixture must name a path nothing is bound at"
        );
        path.display().to_string()
    }

    /// `bind` is serve-without-dial, and a `CapabilitySet` has no such member,
    /// so the only correct answer is a refusal. Both widenings are silent
    /// authority changes: `ConnectBind` adds the `connect(2)` the mode exists
    /// to withhold, and `Connect` inverts the mode outright.
    #[test]
    fn bind_mode_is_refused_because_neither_nono_mode_withholds_connect() {
        let error = unix_socket_mode(&UnixSocketGrant::bind("/run/llo/served.sock"))
            .expect_err("bind withholds connect(2), which no UnixSocketMode expresses");
        let message = error.to_string();
        assert!(
            message.contains("§6") && message.contains("bind"),
            "the refusal must name the dimension and the mode: {message}"
        );
    }

    /// The mirror of the above, and the half that catches a refusal widened to
    /// fire on everything: the two modes that legitimately dial must compile.
    #[test]
    fn dialing_modes_compile_to_their_nono_mode() {
        let endpoint = bound_endpoint();

        let connect = unix_socket_mode(&UnixSocketGrant::connect(&endpoint))
            .expect("connect is enforceable and the endpoint is bound");
        assert!(
            matches!(connect, UnixSocketMode::Connect),
            "connect must not silently acquire bind(2)"
        );

        let connect_bind = unix_socket_mode(&UnixSocketGrant::connect_bind(&endpoint))
            .expect("connect-bind is enforceable");
        assert!(
            matches!(connect_bind, UnixSocketMode::ConnectBind),
            "connect-bind must keep the bind(2) it was granted"
        );
    }

    /// The §6 ordering contract: a `connect` grant names an endpoint someone
    /// else owns, and it is resolved when confinement is applied, so the peer
    /// must have bound first. This must surface as a named refusal rather than
    /// as a `BackendFailed: ... Path does not exist` from inside nono.
    #[test]
    fn connect_to_an_unbound_endpoint_names_the_ordering_contract() {
        let error = unix_socket_mode(&UnixSocketGrant::connect(unbound_endpoint()))
            .expect_err("a connect grant cannot resolve an endpoint nothing is bound at");
        let message = error.to_string();
        assert!(
            message.contains("§6") && message.contains("nothing is bound there yet"),
            "the refusal must name the dimension and the ordering: {message}"
        );
    }

    /// The existence requirement is scoped to grants that only dial. A grant
    /// permitting `bind(2)` creates the socket itself, so requiring it to
    /// pre-exist would refuse the very case bind exists to serve — and would
    /// make the ordering contract fire on a manifest that satisfies it.
    #[test]
    fn a_grant_that_binds_need_not_find_the_endpoint_already_there() {
        let mode = unix_socket_mode(&UnixSocketGrant::connect_bind(unbound_endpoint()))
            .expect("connect-bind creates the endpoint; it must not require one");
        assert!(matches!(mode, UnixSocketMode::ConnectBind));
    }

    /// A trailing slash names a tree, not a leaf (§2's spelling, shared on
    /// purpose). The endpoint check applies to leaves only: a directory grant
    /// covers sockets that do not exist yet, which is the point of granting a
    /// directory rather than enumerating its members.
    #[test]
    fn a_directory_grant_is_not_checked_for_a_bound_leaf() {
        let directory = format!("{}/", std::env::temp_dir().join("llo-uds-absent").display());
        let mode = unix_socket_mode(&UnixSocketGrant::connect(directory))
            .expect("a directory grant names a tree, so no leaf is resolved here");
        assert!(matches!(mode, UnixSocketMode::Connect));
    }
}
