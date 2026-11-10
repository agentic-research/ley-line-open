//! `leyline rootfs import-oci` (bead `ley-line-open-7736ab`).
//!
//! The entry point an orchestrator (Cloister) calls to turn a digest-pinned
//! OCI image layout into the content-addressed rootfs an execution catalog
//! names. It runs offline and in-process: no registry, no container daemon,
//! no krunvm. The contract is `leyline_runtime::artifacts::oci`'s; this
//! command fixes the platform to the host's (libkrun does not emulate) and
//! prints the published rootfs as one JSON document on stdout:
//!
//! ```json
//! {"rootfs":{"algorithm":"blake3-256","value":"<hex>"},
//!  "image":{"reference":"sha256:…","manifest":"sha256:…","config":"sha256:…",
//!           "platform":{"os":"linux","architecture":"arm64"}},
//!  "files":10,"specialModeBitsCleared":1,"symlinksResolved":2,
//!  "specialEntriesDropped":[{"layer":0,"path":"dev/console","kind":"char"}],
//!  "linksDroppedKernelMount":[{"path":"etc/mtab","target":"/proc/mounts"}],
//!  "linksDroppedDangling":[{"path":"usr/share/zoneinfo/localtime","target":"/etc/localtime"}],
//!  "unreadableFilesDropped":[{"path":"etc/shadow","mode":0}],
//!  "imageConfig":{"entrypoint":["/bin/tool"],"cmd":[],"env":[],"workingDir":"/","user":""}}
//! ```
//!
//! The four `…Dropped` lists name what the import did not write; the runtime
//! README's "Container images" section states each rule and what, if
//! anything, a guest can observe of the difference. Each list is empty when
//! nothing was dropped.

use std::path::Path;

use anyhow::{Context, Result};
use leyline_runtime::artifacts::oci::{
    DEFAULT_MAX_ROOTFS_BYTES, ImportedRootfs, OciImport, Platform, SymlinkPolicy, import_oci_layout,
};
use leyline_runtime::cas_volume::{CasVolume, ProvisionedCas, default_state_dir};

use crate::{RootfsCommands, SymlinkMode};

/// Import `image` from `layout` into `cas_root` for this host's platform.
pub fn import_oci(
    layout: &Path,
    image: &str,
    cas_root: &Path,
    symlinks: SymlinkMode,
) -> Result<ImportedRootfs> {
    let request = OciImport {
        layout,
        reference: image,
        platform: Platform::host()?,
        symlinks: match symlinks {
            SymlinkMode::Refuse => SymlinkPolicy::Refuse,
            SymlinkMode::Resolve => SymlinkPolicy::Resolve,
        },
        max_rootfs_bytes: DEFAULT_MAX_ROOTFS_BYTES,
    };
    import_oci_layout(&request, cas_root)
        .with_context(|| format!("import {image} from {}", layout.display()))
}

pub fn run(command: &RootfsCommands) -> Result<()> {
    match command {
        RootfsCommands::ImportOci {
            layout,
            image,
            cas_root,
            symlinks,
        } => {
            let imported = import_oci(layout, image, cas_root, *symlinks)?;
            println!("{}", serde_json::to_string(&imported)?);
            Ok(())
        }
        RootfsCommands::ProvisionCas {
            state_dir,
            size_cap_gib,
        } => {
            let provisioned = provision_cas(state_dir.as_deref(), *size_cap_gib)?;
            println!("{}", serde_json::to_string(&provisioned)?);
            Ok(())
        }
    }
}

/// `leyline rootfs provision-cas` (bead `ley-line-open-ba86d3`): a CAS root
/// on a case-sensitive filesystem beneath `state_dir` (or LLO's default
/// state directory), per `leyline_runtime::cas_volume`.
pub fn provision_cas(state_dir: Option<&Path>, size_cap_gib: u64) -> Result<ProvisionedCas> {
    let state_dir = match state_dir {
        Some(state_dir) => state_dir.to_path_buf(),
        None => default_state_dir()?,
    };
    let size_cap = size_cap_gib
        .checked_mul(1 << 30)
        .context("--size-cap-gib is too large")?;
    CasVolume::new(&state_dir, size_cap)?
        .provision()
        .with_context(|| {
            format!(
                "provision a case-sensitive CAS root in {}",
                state_dir.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The runtime crate's checked-in OCI fixture, and the index digest its
    /// `index.json` names (read here only to learn the digest).
    fn fixture() -> (PathBuf, String) {
        let layout =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../runtime/tests/fixtures/oci-multilayer");
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
        let digest = index["manifests"][0]["digest"]
            .as_str()
            .unwrap()
            .to_string();
        (layout, digest)
    }

    #[test]
    fn import_oci_publishes_the_host_platform_rootfs() {
        let (layout, digest) = fixture();
        let cas = tempfile::TempDir::new().unwrap();
        let imported = import_oci(&layout, &digest, cas.path(), SymlinkMode::Resolve).unwrap();
        let host = leyline_runtime::artifacts::oci::Platform::host().unwrap();
        assert_eq!(imported.image.platform, host);
        assert_eq!(imported.image.reference, digest);
        assert_eq!(imported.rootfs.algorithm, "blake3-256");
        assert!(cas.path().join(&imported.rootfs.value).is_dir());
        assert_eq!(
            std::fs::read(cas.path().join(&imported.rootfs.value).join("etc/arch")).unwrap(),
            format!("{}\n", host.architecture).as_bytes()
        );
        // The JSON a caller reads: `rootfs` is the catalog's DigestRef shape.
        let json = serde_json::to_value(&imported).unwrap();
        assert_eq!(json["rootfs"]["algorithm"], "blake3-256");
        assert_eq!(json["rootfs"]["value"], imported.rootfs.value.as_str());
        assert_eq!(json["image"]["manifest"], imported.image.manifest.as_str());
        assert_eq!(json["imageConfig"]["entrypoint"][0], "/bin/tool");
    }

    #[test]
    fn import_oci_refuses_symbolic_links_unless_told_to_resolve_them() {
        let (layout, digest) = fixture();
        let cas = tempfile::TempDir::new().unwrap();
        let error = import_oci(&layout, &digest, cas.path(), SymlinkMode::Refuse).unwrap_err();
        assert!(format!("{error:#}").contains("bin/abs-alias"), "{error:#}");
    }

    #[test]
    fn run_reports_a_refused_import_as_an_error() {
        let (layout, _) = fixture();
        let cas = tempfile::TempDir::new().unwrap();
        let error = run(&RootfsCommands::ImportOci {
            layout,
            image: "latest".into(),
            cas_root: cas.path().to_path_buf(),
            symlinks: SymlinkMode::Resolve,
        })
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("mutable image reference"),
            "{error:#}"
        );
    }

    #[test]
    fn run_publishes_into_the_cas_root() {
        let (layout, digest) = fixture();
        let cas = tempfile::TempDir::new().unwrap();
        run(&RootfsCommands::ImportOci {
            layout,
            image: digest,
            cas_root: cas.path().to_path_buf(),
            symlinks: SymlinkMode::Resolve,
        })
        .unwrap();
        let published: Vec<_> = std::fs::read_dir(cas.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(published.len(), 1);
    }

    /// The `leyline` dispatcher routes `rootfs` here and returns its error.
    #[test]
    fn the_command_dispatcher_runs_rootfs_commands() {
        let (layout, _) = fixture();
        let cas = tempfile::TempDir::new().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let error = runtime
            .block_on(crate::run(crate::Commands::Rootfs {
                command: RootfsCommands::ImportOci {
                    layout,
                    image: "latest".into(),
                    cas_root: cas.path().to_path_buf(),
                    symlinks: SymlinkMode::Refuse,
                },
            }))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("mutable image reference"),
            "{error:#}"
        );
    }

    /// `provision-cas` validates before it touches the host: a cap too
    /// large to express, too small for a volume, or a relative state
    /// directory is refused and nothing is created. (The real provisioning
    /// is `leyline-runtime`'s `tests/cas_volume.rs`.)
    #[test]
    fn provision_cas_refuses_bad_configuration_before_touching_the_host() {
        let state = tempfile::TempDir::new().unwrap();
        for (state_dir, gib, needle) in [
            (state.path(), u64::MAX, "too large"),
            (state.path(), 0, "at least"),
            (Path::new("relative"), 64, "absolute path"),
        ] {
            let error = provision_cas(Some(state_dir), gib).unwrap_err();
            assert!(format!("{error:#}").contains(needle), "{error:#}");
        }
        assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
    }

    /// On Linux the command makes no volume: it verifies `<state>/cas`.
    #[test]
    #[cfg(target_os = "linux")]
    fn provision_cas_on_linux_verifies_the_state_directory() {
        let state = tempfile::TempDir::new().unwrap();
        let provisioned = provision_cas(Some(state.path()), 64).unwrap();
        assert_eq!(provisioned.cas_root, state.path().join("cas"));
        let json = serde_json::to_value(&provisioned).unwrap();
        assert_eq!(json["casRoot"], provisioned.cas_root.to_str().unwrap());
        assert_eq!(json["filesystem"]["caseSensitive"], true);
    }
}
