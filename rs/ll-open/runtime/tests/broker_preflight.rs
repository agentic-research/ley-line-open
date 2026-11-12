//! Endpoint host checks (ADR-0046 S4 table; S11 "Cross-document and endpoint
//! checks"): each refusal by name, in the table's order.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use leyline_runtime::ErrorCode;
use leyline_runtime::broker::preflight::{SUN_PATH_BYTES, check_endpoint};

fn euid() -> u32 {
    // SAFETY: geteuid cannot fail.
    unsafe { libc::geteuid() }
}

fn private_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("ch")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    (dir, real)
}

fn refusal(path: &std::path::Path, uid: u32) -> (ErrorCode, String) {
    let error = check_endpoint("orders-api", path, uid).expect_err("refused");
    assert!(error.detail.contains("`orders-api`"), "{}", error.detail);
    (error.code, error.detail)
}

#[test]
fn a_fresh_path_in_a_private_realpath_directory_passes() {
    let (_dir, real) = private_dir();
    check_endpoint("orders-api", &real.join("orders.api.sock"), euid()).expect("passes");
}

#[test]
fn an_existing_path_is_a_resource_conflict() {
    let (_dir, real) = private_dir();
    let path = real.join("orders.api.sock");
    std::fs::write(&path, b"").unwrap();
    let (code, detail) = refusal(&path, euid());
    assert_eq!(code, ErrorCode::ResourceConflict);
    assert!(detail.contains("endpoint exists"), "{detail}");
    // A dangling symlink at the path exists too.
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("/nonexistent", &path).unwrap();
    assert_eq!(refusal(&path, euid()).0, ErrorCode::ResourceConflict);
}

#[test]
fn a_missing_parent_is_refused() {
    let (_dir, real) = private_dir();
    let (code, detail) = refusal(&real.join("absent/orders.api.sock"), euid());
    assert_eq!(code, ErrorCode::InvalidGrant);
    assert!(detail.contains("endpoint parent missing"), "{detail}");
    let file = real.join("plain");
    std::fs::write(&file, b"").unwrap();
    let (_, detail) = refusal(&file.join("orders.api.sock"), euid());
    assert!(detail.contains("endpoint parent missing"), "{detail}");
}

#[test]
fn a_parent_reached_through_a_symlink_is_refused() {
    let (_dir, real) = private_dir();
    let target = real.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = real.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let (code, detail) = refusal(&link.join("orders.api.sock"), euid());
    assert_eq!(code, ErrorCode::InvalidGrant);
    assert!(detail.contains("through a symlink"), "{detail}");
    // A symlink higher up the path: the parent is not its realpath.
    let nested = target.join("inner");
    std::fs::create_dir(&nested).unwrap();
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (_, detail) = refusal(&link.join("inner/orders.api.sock"), euid());
    assert!(detail.contains("through a symlink"), "{detail}");
}

#[test]
fn a_parent_that_is_not_private_is_refused() {
    let (_dir, real) = private_dir();
    for mode in [0o755, 0o750, 0o701, 0o710, 0o1700, 0o2700] {
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(mode)).unwrap();
        let applied = std::fs::metadata(&real).unwrap().permissions().mode() & 0o7777;
        if applied == 0o700 {
            // macOS drops setgid a non-member sets: nothing to refuse.
            continue;
        }
        let (code, detail) = refusal(&real.join("orders.api.sock"), euid());
        assert_eq!(code, ErrorCode::InvalidGrant, "mode {mode:o}");
        assert!(detail.contains("endpoint parent not private"), "{detail}");
    }
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Owned by another uid.
    let (_, detail) = refusal(&real.join("orders.api.sock"), euid().wrapping_add(1));
    assert!(detail.contains("endpoint parent not private"), "{detail}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_parent_carrying_an_acl_is_refused() {
    let (_dir, real) = private_dir();
    check_endpoint("orders-api", &real.join("orders.api.sock"), euid()).expect("no ACL yet");
    let status = std::process::Command::new("/bin/chmod")
        .args(["+a", "group:everyone allow list,add_file,delete_child"])
        .arg(&real)
        .status()
        .unwrap();
    assert!(status.success());
    let (code, detail) = refusal(&real.join("orders.api.sock"), euid());
    assert_eq!(code, ErrorCode::InvalidGrant);
    assert!(detail.contains("ACL"), "{detail}");
}

#[test]
fn an_ancestor_others_can_rename_in_is_refused() {
    let (_dir, real) = private_dir();
    let open_ancestor = real.join("open");
    std::fs::create_dir(&open_ancestor).unwrap();
    let parent = open_ancestor.join("ch");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&open_ancestor, std::fs::Permissions::from_mode(0o777)).unwrap();
    let (code, detail) = refusal(&parent.join("orders.api.sock"), euid());
    assert_eq!(code, ErrorCode::InvalidGrant);
    assert!(detail.contains("can be renamed by others"), "{detail}");
    // Sticky, as /tmp is: others cannot rename what they do not own.
    std::fs::set_permissions(&open_ancestor, std::fs::Permissions::from_mode(0o1777)).unwrap();
    check_endpoint("orders-api", &parent.join("orders.api.sock"), euid()).expect("sticky");
    // Group-writable without sticky is open too.
    std::fs::set_permissions(&open_ancestor, std::fs::Permissions::from_mode(0o775)).unwrap();
    assert_eq!(
        refusal(&parent.join("orders.api.sock"), euid()).0,
        ErrorCode::InvalidGrant
    );
}

#[test]
fn a_path_longer_than_sun_path_is_refused() {
    let (_dir, real) = private_dir();
    let base = real.to_str().unwrap().len() + 1;
    let fits = real.join("a".repeat(SUN_PATH_BYTES - 1 - base));
    check_endpoint("orders-api", &fits, euid()).expect("one byte short of the NUL fits");
    let over = real.join("a".repeat(SUN_PATH_BYTES - base));
    let (code, detail) = refusal(&over, euid());
    assert_eq!(code, ErrorCode::InvalidGrant);
    assert!(detail.contains("endpoint path too long"), "{detail}");
}
