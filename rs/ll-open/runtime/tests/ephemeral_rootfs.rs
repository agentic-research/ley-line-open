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
    let fixture = TempDir::new().expect("fixture");
    let source = rootfs_fixture(fixture.path());
    let destination = fixture.path().join("run-root");
    fs::create_dir(&destination).expect("run root");

    let materialized =
        materialize_ephemeral_rootfs(&source, &destination).expect("materialize rootfs");

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
    assert_eq!(
        fs::metadata(materialized.canonical_path.join("usr/bin"))
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o7777,
        0o750
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

#[test]
fn copied_bytes_are_reverified_against_the_requested_identity() {
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
