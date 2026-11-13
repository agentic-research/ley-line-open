//! `leyline rootfs import-oci` (beads `ley-line-open-7736ab`,
//! `ley-line-open-97b636`).
//!
//! The entry point an orchestrator (Cloister) calls to turn a digest-pinned
//! OCI image into the content-addressed rootfs an execution catalog names.
//! The image comes from an OCI image layout (`--layout DIR --image DIGEST`,
//! offline) or is pulled from a registry (`--from-registry
//! [registry/]repository@sha256:<hex>`, HTTPS and anonymous; plain HTTP only
//! to the loopback registry `--insecure-plain-http-registry` names). Either
//! way it runs in-process, with no container daemon and no krunvm. The
//! contracts are `leyline_runtime::artifacts::oci`'s and
//! `leyline_runtime::artifacts::registry`'s: a pull stages the fetched blobs
//! as a layout and imports that, so both sources give the same rootfs and
//! the same report for one digest. This command fixes the platform to the
//! host's (libkrun does not emulate) and prints the published rootfs as one
//! JSON document on stdout:
//!
//! ```json
//! {"rootfs":{"algorithm":"blake3-256","value":"<hex>"},
//!  "image":{"reference":"sha256:…","manifest":"sha256:…","config":"sha256:…",
//!           "platform":{"os":"linux","architecture":"arm64"}},
//!  "files":10,"directories":6,"symlinks":2,"specialModeBitsCleared":1,
//!  "symlinksResolved":0,
//!  "specialEntriesDropped":[{"layer":0,"path":"dev/console","kind":"char"}],
//!  "linksDroppedKernelMount":[],"linksDroppedDangling":[],
//!  "unreadableFilesDropped":[{"path":"etc/shadow","mode":0}],
//!  "imageConfig":{"entrypoint":["/bin/tool"],"cmd":[],"env":[],"workingDir":"/","user":""}}
//! ```
//!
//! The four `…Dropped` lists name what the import did not write; the runtime
//! README's "Container images" section states each rule and what, if
//! anything, a guest can observe of the difference. Each list is empty when
//! nothing was dropped.

use std::path::Path;

use anyhow::{Context, Result, bail};
use leyline_runtime::artifacts::oci::{
    DEFAULT_MAX_ROOTFS_BYTES, ImportedRootfs, OciImport, Platform, SymlinkPolicy, import_oci_layout,
};
use leyline_runtime::artifacts::registry::{RegistryLimits, RegistryPull, pull_oci_image};
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
        symlinks: policy(symlinks),
        max_rootfs_bytes: DEFAULT_MAX_ROOTFS_BYTES,
    };
    import_oci_layout(&request, cas_root)
        .with_context(|| format!("import {image} from {}", layout.display()))
}

/// Pull `reference` from its registry into `cas_root` for this host's
/// platform. `insecure_plain_http` names the one loopback registry that may
/// be reached over plain HTTP.
pub fn pull_oci(
    reference: &str,
    cas_root: &Path,
    symlinks: SymlinkMode,
    insecure_plain_http: Option<&str>,
) -> Result<ImportedRootfs> {
    let request = RegistryPull {
        reference,
        platform: Platform::host()?,
        symlinks: policy(symlinks),
        max_rootfs_bytes: DEFAULT_MAX_ROOTFS_BYTES,
        limits: RegistryLimits::default(),
        insecure_plain_http,
    };
    pull_oci_image(&request, cas_root).with_context(|| format!("pull {reference}"))
}

fn policy(symlinks: SymlinkMode) -> SymlinkPolicy {
    match symlinks {
        SymlinkMode::Preserve => SymlinkPolicy::Preserve,
        SymlinkMode::Refuse => SymlinkPolicy::Refuse,
        SymlinkMode::Resolve => SymlinkPolicy::Resolve,
    }
}

pub fn run(command: &RootfsCommands) -> Result<()> {
    match command {
        RootfsCommands::ImportOci {
            layout,
            image,
            from_registry,
            insecure_plain_http_registry,
            cas_root,
            symlinks,
        } => {
            let imported = match (layout, image, from_registry, insecure_plain_http_registry) {
                (Some(layout), Some(image), None, None) => {
                    import_oci(layout, image, cas_root, *symlinks)?
                }
                (None, None, Some(reference), insecure) => {
                    pull_oci(reference, cas_root, *symlinks, insecure.as_deref())?
                }
                _ => bail!(
                    "import-oci takes either --layout DIR --image DIGEST, or --from-registry \
                     REFERENCE [--insecure-plain-http-registry HOST:PORT]"
                ),
            };
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

    fn from_layout(
        layout: PathBuf,
        image: &str,
        cas: &Path,
        symlinks: SymlinkMode,
    ) -> RootfsCommands {
        RootfsCommands::ImportOci {
            layout: Some(layout),
            image: Some(image.into()),
            from_registry: None,
            insecure_plain_http_registry: None,
            cas_root: cas.to_path_buf(),
            symlinks,
        }
    }

    fn from_registry(reference: &str, insecure: Option<&str>, cas: &Path) -> RootfsCommands {
        RootfsCommands::ImportOci {
            layout: None,
            image: None,
            from_registry: Some(reference.into()),
            insecure_plain_http_registry: insecure.map(str::to_string),
            cas_root: cas.to_path_buf(),
            symlinks: SymlinkMode::Resolve,
        }
    }

    fn published(cas: &Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(cas)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    }

    #[test]
    fn run_reports_a_refused_import_as_an_error() {
        let (layout, _) = fixture();
        let cas = tempfile::TempDir::new().unwrap();
        let error = run(&from_layout(
            layout,
            "latest",
            cas.path(),
            SymlinkMode::Resolve,
        ))
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
        run(&from_layout(
            layout,
            &digest,
            cas.path(),
            SymlinkMode::Resolve,
        ))
        .unwrap();
        assert_eq!(published(cas.path()).len(), 1);
    }

    /// `--from-registry` against the runtime's in-process test registry
    /// serving the same fixture: the same rootfs and the same JSON report.
    #[test]
    fn a_registry_pull_reports_what_the_layout_import_reports() {
        let (layout, digest) = fixture();
        let registry = oci_registry::TestRegistry::serve(
            &layout,
            "fixture/multilayer",
            oci_registry::Behavior::default(),
        );
        let reference = registry.reference(&digest);
        let pulled_cas = tempfile::TempDir::new().unwrap();
        let pulled = pull_oci(
            &reference,
            pulled_cas.path(),
            SymlinkMode::Resolve,
            Some(&registry.authority),
        )
        .unwrap();
        let layout_cas = tempfile::TempDir::new().unwrap();
        let imported =
            import_oci(&layout, &digest, layout_cas.path(), SymlinkMode::Resolve).unwrap();
        assert_eq!(
            serde_json::to_string(&pulled).unwrap(),
            serde_json::to_string(&imported).unwrap()
        );
        assert_eq!(
            published(pulled_cas.path()),
            vec![std::ffi::OsString::from(&pulled.rootfs.value)]
        );

        // `run` takes the same path and publishes the same entry.
        let run_cas = tempfile::TempDir::new().unwrap();
        run(&from_registry(
            &reference,
            Some(&registry.authority),
            run_cas.path(),
        ))
        .unwrap();
        assert_eq!(published(run_cas.path()), published(pulled_cas.path()));

        // The link policy reaches the pull.
        let refused = pull_oci(
            &reference,
            run_cas.path(),
            SymlinkMode::Refuse,
            Some(&registry.authority),
        )
        .unwrap_err();
        assert!(
            format!("{refused:#}").contains("bin/abs-alias"),
            "{refused:#}"
        );
    }

    #[test]
    fn a_refused_pull_names_the_reference() {
        let cas = tempfile::TempDir::new().unwrap();
        let error = run(&from_registry("alpine:3.20", None, cas.path())).unwrap_err();
        let shown = format!("{error:#}");
        assert!(shown.contains("pull alpine:3.20"), "{shown}");
        assert!(shown.contains("mutable image reference"), "{shown}");
        assert!(published(cas.path()).is_empty());
    }

    /// `run` refuses a command that names both sources, or neither, as clap
    /// does; an in-process caller can still build one.
    #[test]
    fn run_takes_exactly_one_source() {
        let (layout, digest) = fixture();
        let cas = tempfile::TempDir::new().unwrap();
        let mixed = [
            RootfsCommands::ImportOci {
                layout: Some(layout.clone()),
                image: Some(digest.clone()),
                from_registry: Some(format!("ghcr.io/a@{digest}")),
                insecure_plain_http_registry: None,
                cas_root: cas.path().to_path_buf(),
                symlinks: SymlinkMode::Resolve,
            },
            RootfsCommands::ImportOci {
                layout: Some(layout.clone()),
                image: Some(digest.clone()),
                from_registry: None,
                insecure_plain_http_registry: Some("127.0.0.1:5000".into()),
                cas_root: cas.path().to_path_buf(),
                symlinks: SymlinkMode::Resolve,
            },
            RootfsCommands::ImportOci {
                layout: Some(layout),
                image: None,
                from_registry: None,
                insecure_plain_http_registry: None,
                cas_root: cas.path().to_path_buf(),
                symlinks: SymlinkMode::Resolve,
            },
            RootfsCommands::ImportOci {
                layout: None,
                image: Some(digest.clone()),
                from_registry: Some(format!("ghcr.io/a@{digest}")),
                insecure_plain_http_registry: None,
                cas_root: cas.path().to_path_buf(),
                symlinks: SymlinkMode::Resolve,
            },
        ];
        for command in mixed {
            let error = run(&command).unwrap_err();
            assert!(
                format!("{error:#}").contains("either --layout"),
                "{command:?}: {error:#}"
            );
        }
        assert!(published(cas.path()).is_empty());
    }

    /// The flags a caller scripts against: exactly one source, `--image`
    /// only with `--layout`, the plain-HTTP opt-in only with a pull.
    #[test]
    fn the_import_oci_flags_parse_as_one_source() {
        use clap::Parser;
        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            cmd: crate::Commands,
        }
        let parse = |args: &[&str]| {
            let mut argv = vec!["t", "rootfs", "import-oci", "--cas-root", "/cas"];
            argv.extend_from_slice(args);
            Harness::try_parse_from(argv).map(|harness| harness.cmd)
        };
        match parse(&[
            "--from-registry",
            "ghcr.io/a@sha256:00",
            "--insecure-plain-http-registry",
            "127.0.0.1:5000",
        ])
        .unwrap()
        {
            crate::Commands::Rootfs {
                command:
                    RootfsCommands::ImportOci {
                        layout: None,
                        image: None,
                        from_registry: Some(reference),
                        insecure_plain_http_registry: Some(insecure),
                        ..
                    },
            } => {
                assert_eq!(reference, "ghcr.io/a@sha256:00");
                assert_eq!(insecure, "127.0.0.1:5000");
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&["--layout", "/l", "--image", "sha256:00"]).is_ok());
        for refused in [
            &[][..],
            &["--layout", "/l"][..],
            &["--image", "sha256:00"][..],
            &[
                "--layout",
                "/l",
                "--image",
                "sha256:00",
                "--from-registry",
                "a@sha256:00",
            ][..],
            &["--from-registry", "a@sha256:00", "--image", "sha256:00"][..],
            &[
                "--layout",
                "/l",
                "--image",
                "sha256:00",
                "--insecure-plain-http-registry",
                "127.0.0.1:1",
            ][..],
        ] {
            assert!(parse(refused).is_err(), "{refused:?}");
        }
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
                command: from_layout(layout, "latest", cas.path(), SymlinkMode::Refuse),
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

/// The runtime crate's in-process OCI registry (bead `ley-line-open-97b636`).
#[cfg(test)]
#[path = "../../runtime/tests/oci_registry/mod.rs"]
mod oci_registry;
