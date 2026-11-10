use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use leyline_runtime::DigestRef;
use leyline_runtime::backends::libkrun::plan::ResolvedRootfs;
use leyline_runtime::backends::libkrun::volume::{
    materialize_ephemeral_rootfs, verify_ephemeral_rootfs,
};
use tempfile::TempDir;

mod common;

fn rootfs_fixture(parent: &Path) -> ResolvedRootfs {
    let content = b"probe-v1";
    let content_digest = blake3::hash(content).to_hex().to_string();
    let manifest = format!(
        "{{\"version\":1,\"files\":[{{\"path\":\"usr/bin/probe\",\"mode\":493,\"blake3\":\"{content_digest}\"}}]}}"
    );
    let digest = blake3::hash(manifest.as_bytes()).to_hex().to_string();
    let rootfs = parent.join("immutable-rootfs");
    let bin = rootfs.join("usr/bin");
    fs::create_dir_all(&bin).expect("rootfs directories");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o750)).expect("bin mode");
    fs::write(bin.join("probe"), content).expect("rootfs executable");
    fs::set_permissions(bin.join("probe"), fs::Permissions::from_mode(0o755))
        .expect("executable mode");
    fs::write(rootfs.join("rootfs.manifest.json"), manifest).expect("rootfs manifest");

    ResolvedRootfs {
        digest: DigestRef {
            algorithm: "blake3-256".into(),
            value: digest,
        },
        canonical_path: rootfs.canonicalize().expect("canonical rootfs"),
    }
}

#[test]
fn guest_writes_are_isolated_from_the_immutable_cas_root() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let source = rootfs_fixture(fixture.path());
    let destination = fixture.path().join("run-root");
    fs::create_dir(&destination).expect("run root");

    let materialized =
        materialize_ephemeral_rootfs(&source, &destination).expect("materialize rootfs");

    assert_eq!(
        materialized.canonical_path,
        destination
            .canonicalize()
            .expect("canonical run directory")
            .join("rootfs")
    );
    assert_eq!(
        fs::metadata(&destination)
            .expect("private run directory")
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    let executable = materialized.canonical_path.join("usr/bin/probe");
    assert_eq!(fs::read(&executable).expect("read copy"), b"probe-v1");
    assert_eq!(
        fs::metadata(&executable)
            .expect("copy metadata")
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    // The CAS entry's `usr/bin` is 0750, but manifest v1 names no directory
    // modes: the guest gets the manifest's implied tree at 0755 (bead
    // `ley-line-open-38ff59`).
    assert_eq!(
        fs::metadata(materialized.canonical_path.join("usr/bin"))
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );

    fs::write(&executable, b"guest-write").expect("write ephemeral rootfs");
    fs::write(
        materialized.canonical_path.join("guest-created"),
        b"scratch",
    )
    .expect("create guest file");

    assert_eq!(
        fs::read(source.canonical_path.join("usr/bin/probe")).expect("read immutable source"),
        b"probe-v1"
    );
    assert!(!source.canonical_path.join("guest-created").exists());
}

/// Every directory of a tree with its mode, rootfs-relative (`""` is the
/// root), and every non-directory's path.
fn tree_shape(root: &Path) -> (Vec<(String, u32)>, Vec<String>) {
    fn walk(root: &Path, dir: &Path, dirs: &mut Vec<(String, u32)>, rest: &mut Vec<String>) {
        let relative = dir
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mode = fs::symlink_metadata(dir).unwrap().permissions().mode() & 0o7777;
        dirs.push((relative, mode));
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                walk(root, &path, dirs, rest);
            } else {
                rest.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let (mut dirs, mut rest) = (Vec::new(), Vec::new());
    walk(root, root, &mut dirs, &mut rest);
    (dirs, rest)
}

/// Bead `ley-line-open-38ff59`: manifest v1 authenticates files only, so a
/// CAS entry's directory set and modes are not part of the rootfs digest. A
/// directory someone widened (world-writable, setgid, sticky) or planted
/// beside the listed files must not reach the guest: the per-run copy holds
/// exactly the parents of the listed files, each at 0755, the root included.
#[test]
fn a_widened_or_planted_directory_never_reaches_the_guest() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let source = rootfs_fixture(fixture.path());
    let cas = &source.canonical_path;
    fs::set_permissions(cas.join("usr"), fs::Permissions::from_mode(0o3777)).unwrap();
    fs::set_permissions(cas.join("usr/bin"), fs::Permissions::from_mode(0o1777)).unwrap();
    for planted in ["etc/cron.d", "opt/x/y", "usr/share"] {
        fs::create_dir_all(cas.join(planted)).unwrap();
        fs::set_permissions(cas.join(planted), fs::Permissions::from_mode(0o777)).unwrap();
    }
    fs::set_permissions(cas, fs::Permissions::from_mode(0o777)).unwrap();
    let destination = fixture.path().join("run-root");
    fs::create_dir(&destination).expect("run root");

    let materialized =
        materialize_ephemeral_rootfs(&source, &destination).expect("materialize rootfs");

    let (dirs, files) = tree_shape(&materialized.canonical_path);
    assert_eq!(
        dirs,
        [
            (String::new(), 0o755),
            ("usr".to_string(), 0o755),
            ("usr/bin".to_string(), 0o755),
        ]
    );
    assert_eq!(files, ["rootfs.manifest.json", "usr/bin/probe"]);
    verify_ephemeral_rootfs(&materialized).expect("the normalized copy verifies");
}

/// One digest names one guest tree: two CAS entries with the same manifest
/// but different directory modes and planted directories give per-run
/// copies of the same shape.
#[test]
fn one_digest_materializes_one_guest_tree() {
    let _serial = common::serial();
    let shape = |widen: bool| {
        let fixture = TempDir::new().expect("fixture");
        let source = rootfs_fixture(fixture.path());
        if widen {
            fs::set_permissions(
                source.canonical_path.join("usr"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::create_dir(source.canonical_path.join("tmp")).unwrap();
        }
        let destination = fixture.path().join("run-root");
        fs::create_dir(&destination).expect("run root");
        let materialized =
            materialize_ephemeral_rootfs(&source, &destination).expect("materialize rootfs");
        (
            source.digest.clone(),
            tree_shape(&materialized.canonical_path),
        )
    };
    let (plain_digest, plain) = shape(false);
    let (widened_digest, widened) = shape(true);
    assert_eq!(plain_digest, widened_digest);
    assert_eq!(plain, widened);
}

/// The per-run copy is re-verified before VM entry with its directories:
/// one planted or widened after materialization is refused by name.
#[test]
fn a_directory_changed_in_the_copy_is_refused_before_vm_entry() {
    let _serial = common::serial();
    for change in ["plant", "widen", "narrow-root"] {
        let fixture = TempDir::new().expect("fixture");
        let source = rootfs_fixture(fixture.path());
        let destination = fixture.path().join("run-root");
        fs::create_dir(&destination).expect("run root");
        let materialized =
            materialize_ephemeral_rootfs(&source, &destination).expect("materialize rootfs");
        let root = &materialized.canonical_path;
        match change {
            "plant" => fs::create_dir(root.join("usr/lib")).unwrap(),
            "widen" => {
                fs::set_permissions(root.join("usr"), fs::Permissions::from_mode(0o777)).unwrap()
            }
            _ => fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap(),
        }
        let error = verify_ephemeral_rootfs(&materialized).expect_err(change);
        assert!(
            error.detail.contains("directory"),
            "{change}: unexpected error: {error:?}"
        );
    }
}

/// A file the manifest does not list, in a directory it does not imply, is
/// still refused (not silently left out), as `DirectoryRootfsResolver`
/// refuses it.
#[test]
fn an_undeclared_file_in_a_planted_directory_is_refused() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let source = rootfs_fixture(fixture.path());
    fs::create_dir_all(source.canonical_path.join("opt/x")).unwrap();
    fs::write(source.canonical_path.join("opt/x/payload"), b"undeclared").unwrap();
    let destination = fixture.path().join("run-root");
    fs::create_dir(&destination).expect("run root");

    let error = materialize_ephemeral_rootfs(&source, &destination)
        .expect_err("an undeclared file must be refused");
    assert!(
        error.detail.contains("opt/x/payload"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn copied_bytes_are_reverified_against_the_requested_identity() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let source = rootfs_fixture(fixture.path());
    fs::write(source.canonical_path.join("usr/bin/probe"), b"tampered")
        .expect("tamper source after resolution");
    let destination = fixture.path().join("run-root");
    fs::create_dir(&destination).expect("run root");

    let error = materialize_ephemeral_rootfs(&source, &destination)
        .expect_err("tampered copy must be rejected");

    assert!(
        error
            .detail
            .contains("rootfs content digest differs from manifest"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn destination_tampering_is_rejected_before_vm_entry() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let source = rootfs_fixture(fixture.path());
    let destination = fixture.path().join("run-root");
    fs::create_dir(&destination).expect("run root");
    let materialized =
        materialize_ephemeral_rootfs(&source, &destination).expect("materialize rootfs");
    fs::write(
        materialized.canonical_path.join("usr/bin/probe"),
        b"tampered-after-copy",
    )
    .expect("tamper copied rootfs");

    let error = verify_ephemeral_rootfs(&materialized)
        .expect_err("destination tampering must be rejected before VM entry");

    assert!(
        error
            .detail
            .contains("rootfs content digest differs from manifest"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn materializer_rejects_symlinks_even_when_given_a_pre_resolved_path() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let source = rootfs_fixture(fixture.path());
    symlink(
        source.canonical_path.join("usr/bin/probe"),
        source.canonical_path.join("usr/bin/probe-link"),
    )
    .expect("symlink fixture");
    let destination = fixture.path().join("run-root");
    fs::create_dir(&destination).expect("run root");

    let error =
        materialize_ephemeral_rootfs(&source, &destination).expect_err("symlink must be rejected");

    assert!(
        error.detail.contains("symbolic links are not supported"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn materializer_requires_an_empty_destination() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let source = rootfs_fixture(fixture.path());
    let destination = fixture.path().join("run-root");
    fs::create_dir(&destination).expect("run root");
    fs::write(destination.join("ambient"), b"must not survive").expect("destination fixture");

    let error = materialize_ephemeral_rootfs(&source, &destination)
        .expect_err("non-empty destination must be rejected");

    assert!(
        error
            .detail
            .contains("ephemeral rootfs destination must be empty"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn materializer_rejects_special_files_without_blocking() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let source = rootfs_fixture(fixture.path());
    let fifo = source.canonical_path.join("guest.fifo");
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed: {status}");
    let destination = fixture.path().join("run-root");
    fs::create_dir(&destination).expect("run root");

    let error = materialize_ephemeral_rootfs(&source, &destination)
        .expect_err("socket must be rejected without blocking");

    assert!(
        error.detail.contains("non-regular filesystem entry"),
        "unexpected error: {error:?}"
    );
}
