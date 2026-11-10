//! A case-sensitive CAS volume, provisioned for real (bead
//! `ley-line-open-ba86d3`).
//!
//! The logic of every host is unit-tested in `src/cas_volume.rs` with a fake
//! disk-image tool. These tests run the real one:
//!
//! - **macOS** creates and attaches a sparsebundle with `/usr/bin/hdiutil`,
//!   so they are opt-in: set `LEYLINE_CAS_VOLUME_TESTS=1`. Each test
//!   provisions its own volume beneath a temporary state directory and
//!   detaches it before the directory (and the image in it) is removed.
//! - **Linux** makes no volume: provisioning probes `<state>/cas` and
//!   succeeds on a case-sensitive filesystem, so it always runs.
//! - **Elsewhere** provisioning is refused by name.
//!
//! The acceptance test also needs `LEYLINE_OCI_ACCEPTANCE_DIR` (the
//! first-party OCI layouts, as in `tests/oci_rootfs.rs`).

// The helpers below serve the macOS tests only.
#![cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]

use std::fs;
use std::path::Path;

use leyline_runtime::artifacts::oci::{
    DEFAULT_MAX_ROOTFS_BYTES, OciImport, Platform, SymlinkPolicy, import_oci_layout,
};
use leyline_runtime::cas_volume::{
    CasVolume, DEFAULT_SIZE_CAP_BYTES, is_mount_point, probe_sensitivity,
};
use tempfile::TempDir;

mod oci_fixture;
use oci_fixture::*;

/// A provisioned volume that is detached before its state directory (and
/// the sparsebundle in it) is removed.
struct Provisioned {
    volume: CasVolume,
    state: TempDir,
}

impl Provisioned {
    fn new() -> Self {
        let state = TempDir::new().expect("state directory");
        let volume = CasVolume::new(state.path(), DEFAULT_SIZE_CAP_BYTES).expect("volume");
        Self { volume, state }
    }
}

impl Drop for Provisioned {
    fn drop(&mut self) {
        // A detach right after a write can find the volume busy for a
        // moment (Spotlight is off for it, but fseventsd is not).
        for _ in 0..10 {
            match self.volume.detach() {
                Ok(_) => return,
                Err(error) => {
                    eprintln!("detach {}: {error}", self.state.path().display());
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
        panic!(
            "the test volume beneath {} is still attached: detach it with hdiutil",
            self.state.path().display()
        );
    }
}

/// Whether the real macOS provisioning tests run, saying why when not.
fn macos_tests_enabled() -> bool {
    if std::env::var_os("LEYLINE_CAS_VOLUME_TESTS").is_some_and(|value| value == "1") {
        return true;
    }
    eprintln!(
        "SKIPPED (NOT a pass): LEYLINE_CAS_VOLUME_TESTS=1 is unset; these tests create and \
         attach a real sparsebundle with /usr/bin/hdiutil"
    );
    false
}

fn import(layout: &Path, digest: &str, cas_root: &Path) -> String {
    import_oci_layout(
        &OciImport {
            layout,
            reference: digest,
            platform: Platform::linux("arm64"),
            symlinks: SymlinkPolicy::Resolve,
            max_rootfs_bytes: DEFAULT_MAX_ROOTFS_BYTES,
        },
        cas_root,
    )
    .unwrap_or_else(|error| panic!("{digest} must import into {}: {error}", cas_root.display()))
    .rootfs
    .value
}

#[test]
#[cfg(target_os = "macos")]
fn provision_creates_attaches_and_reattaches_a_case_sensitive_volume() {
    if !macos_tests_enabled() {
        return;
    }
    // SAFETY: geteuid has no preconditions.
    assert_ne!(unsafe { libc::geteuid() }, 0, "provisioning needs no root");
    let provisioned = Provisioned::new();
    let state = provisioned.state.path();

    let first = provisioned.volume.provision().expect("provision");
    assert!((first.created, first.attached) == (true, true), "{first:?}");
    assert_eq!(first.cas_root, state.join("cas-volume/mnt/cas"));
    let mountpoint = first.mountpoint.clone().expect("a mountpoint");
    assert!(
        is_mount_point(&mountpoint).unwrap(),
        "the volume is mounted"
    );
    assert!(state.join("cas-volume/cas.sparsebundle").is_dir());
    // Probed independently of provisioning's own probe.
    let filesystem = probe_sensitivity(&first.cas_root).unwrap();
    assert!(filesystem.case_sensitive, "{filesystem:?}");
    assert_eq!(first.filesystem, filesystem);

    let again = provisioned.volume.provision().expect("provision again");
    assert!(
        (again.created, again.attached) == (false, false),
        "{again:?}"
    );
    assert_eq!(again.cas_root, first.cas_root);

    // A colliding image imports into it, both names written.
    let scratch = TempDir::new().unwrap();
    let layout = Layout::create(&scratch.path().join("oci"));
    let digest = layout
        .image(
            "linux",
            "arm64",
            &[Layer::gzip(vec![
                dir("usr", 0o755),
                dir("usr/share", 0o755),
                dir("usr/share/perl", 0o755),
                dir("usr/share/perl/Pod", 0o755),
                file(
                    "usr/share/perl/Pod/Usage.pm",
                    0o644,
                    b"package Pod::Usage;\n",
                ),
                dir("usr/share/perl/pod", 0o755),
                file(
                    "usr/share/perl/pod/perlfunc.pod",
                    0o644,
                    b"=head1 perlfunc\n",
                ),
            ])],
            None,
        )
        .digest;
    let rootfs = import(&scratch.path().join("oci"), &digest, &first.cas_root);
    let tree = first.cas_root.join(&rootfs);
    assert_eq!(
        fs::read(tree.join("usr/share/perl/Pod/Usage.pm")).unwrap(),
        b"package Pod::Usage;\n"
    );
    assert_eq!(
        fs::read(tree.join("usr/share/perl/pod/perlfunc.pod")).unwrap(),
        b"=head1 perlfunc\n"
    );

    // A provider or daemon restart: a new handle re-attaches, never creates.
    assert!(provisioned.volume.detach().expect("detach"));
    assert!(!is_mount_point(&mountpoint).unwrap());
    let restarted = CasVolume::new(state, DEFAULT_SIZE_CAP_BYTES).unwrap();
    let reattached = restarted
        .attach_existing()
        .expect("re-attach")
        .expect("an existing volume");
    assert!(
        (reattached.created, reattached.attached) == (false, true),
        "{reattached:?}"
    );
    assert!(
        tree.join("usr/share/perl/Pod/Usage.pm").is_file(),
        "what was imported is still there"
    );
}

#[test]
#[cfg(target_os = "macos")]
fn first_party_images_import_into_a_provisioned_volume_with_their_known_digests() {
    if !macos_tests_enabled() {
        return;
    }
    let Some(dir) = std::env::var_os("LEYLINE_OCI_ACCEPTANCE_DIR") else {
        eprintln!(
            "SKIPPED (NOT a pass): LEYLINE_OCI_ACCEPTANCE_DIR is unset; the first-party \
             OCI layouts are not in the repository"
        );
        return;
    };
    let provisioned = Provisioned::new();
    let cas_root = provisioned.volume.provision().expect("provision").cas_root;
    // The digests recorded on bead ley-line-open-fe3d26, imported there on
    // a case-insensitive TMPDIR.
    for (name, digest, expected) in [
        (
            "mache",
            "sha256:8620a0dc2d4c2ae7f6cc1f0ee68d6823a8bc9af6a8cf01d332c08738d9203ac7",
            "dd52c667ff1c44e151bd8f05392607b0760b204dfe5d8e66d92fd76857ea4810",
        ),
        (
            "rosary",
            "sha256:38aadea5d94b5f96ea43d9decd3c6f46f97c71a765fc73afd5f44387c21800ad",
            "2982e7d59b5d33d5f1ef26af332e2963f6bbb7596017bc0be0ff0b57b25b6a23",
        ),
    ] {
        let rootfs = import(&Path::new(&dir).join(name), digest, &cas_root);
        eprintln!(
            "{name} {digest}: rootfs blake3-256:{rootfs} on {}",
            cas_root.display()
        );
        assert_eq!(rootfs, expected, "{name}: the known answer");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn linux_provisioning_verifies_the_state_directory_and_makes_no_volume() {
    let state = TempDir::new().expect("state directory");
    let volume = CasVolume::new(state.path(), DEFAULT_SIZE_CAP_BYTES).unwrap();
    let cas_root = state.path().join("cas");
    fs::create_dir(&cas_root).unwrap();
    let case_sensitive = probe_sensitivity(&cas_root).unwrap().case_sensitive;
    match volume.provision() {
        Ok(provisioned) => {
            assert!(case_sensitive);
            assert_eq!(provisioned.cas_root, cas_root);
            assert_eq!((provisioned.mountpoint, provisioned.image), (None, None));
            assert!(!state.path().join("cas-volume").exists());
        }
        Err(error) => {
            assert!(!case_sensitive, "{error}");
            assert!(matches!(
                error,
                leyline_runtime::cas_volume::CasVolumeError::CaseInsensitive { .. }
            ));
        }
    }
}

#[test]
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn other_hosts_refuse_provisioning_by_name() {
    let state = TempDir::new().expect("state directory");
    let error = CasVolume::new(state.path(), DEFAULT_SIZE_CAP_BYTES)
        .unwrap()
        .provision()
        .unwrap_err();
    assert!(
        matches!(
            error,
            leyline_runtime::cas_volume::CasVolumeError::UnsupportedHost { .. }
        ),
        "{error}"
    );
}
