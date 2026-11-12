//! The endpoint registry (ADR-0046 S4; S11 "Registry"): run B whose §6 grant
//! names run A's endpoint is refused in both start orders.

use std::path::{Path, PathBuf};

use leyline_runtime::ErrorCode;
use leyline_runtime::broker::registry::{EndpointRegistry, SocketGrant};

const A_ENDPOINT: &str = "/private/tmp/ch-a/orders.api.sock";

fn a_endpoints() -> Vec<PathBuf> {
    vec![PathBuf::from(A_ENDPOINT)]
}

fn grant(path: &str) -> SocketGrant {
    SocketGrant(path.into())
}

#[test]
fn a_run_whose_grant_names_a_registered_endpoint_is_refused() {
    let registry = EndpointRegistry::new();
    registry
        .register("run-a", &a_endpoints(), &[grant(A_ENDPOINT)])
        .unwrap();
    let error = registry
        .register("run-b", &[], &[grant(A_ENDPOINT)])
        .expect_err("B after A");
    assert_eq!(error.code, ErrorCode::InvalidGrant);
    assert!(error.detail.contains(A_ENDPOINT), "{}", error.detail);
    assert!(!registry.contains("run-b"));
}

#[test]
fn a_run_whose_endpoint_another_runs_grant_already_names_is_refused() {
    // The ordering race: B registers first, then A.
    let registry = EndpointRegistry::new();
    registry
        .register("run-b", &[], &[grant(A_ENDPOINT)])
        .unwrap();
    let error = registry
        .register("run-a", &a_endpoints(), &[grant(A_ENDPOINT)])
        .expect_err("A after B");
    assert_eq!(error.code, ErrorCode::ResourceConflict);
    assert!(
        error.detail.contains("endpoint held by another run"),
        "{}",
        error.detail
    );
}

#[test]
fn a_directory_grant_containing_an_endpoint_counts_in_both_orders() {
    let registry = EndpointRegistry::new();
    registry.register("run-a", &a_endpoints(), &[]).unwrap();
    assert_eq!(
        registry
            .register("run-b", &[], &[grant("/private/tmp/ch-a/")])
            .unwrap_err()
            .code,
        ErrorCode::InvalidGrant
    );
    assert_eq!(
        registry
            .register("run-c", &[], &[grant("/private/tmp/")])
            .unwrap_err()
            .code,
        ErrorCode::InvalidGrant
    );
    let other = EndpointRegistry::new();
    other
        .register("run-b", &[], &[grant("/private/tmp/ch-a/")])
        .unwrap();
    assert_eq!(
        other
            .register("run-a", &a_endpoints(), &[])
            .unwrap_err()
            .code,
        ErrorCode::ResourceConflict
    );
}

#[test]
fn unrelated_paths_and_prefix_lookalikes_do_not_collide() {
    let registry = EndpointRegistry::new();
    registry.register("run-a", &a_endpoints(), &[]).unwrap();
    // The S0 VM-to-VM shared-path join between workload-owned paths still works.
    registry
        .register("run-b", &[], &[grant("/private/tmp/ch-shared/vm.sock")])
        .unwrap();
    registry
        .register("run-c", &[], &[grant("/private/tmp/ch-shared/vm.sock")])
        .unwrap();
    // `/private/tmp/ch-a-other/` is not inside `/private/tmp/ch-a/`.
    registry
        .register("run-d", &[], &[grant("/private/tmp/ch-a-other/")])
        .unwrap();
    // A bare (non-directory) grant on the parent covers nothing beneath it.
    registry
        .register("run-e", &[], &[grant("/private/tmp/ch-a")])
        .unwrap();
}

#[test]
fn two_runs_cannot_own_one_endpoint_and_removal_frees_it() {
    let registry = EndpointRegistry::new();
    registry.register("run-a", &a_endpoints(), &[]).unwrap();
    assert_eq!(
        registry
            .register("run-b", &a_endpoints(), &[])
            .unwrap_err()
            .code,
        ErrorCode::ResourceConflict
    );
    assert_eq!(
        registry.register("run-a", &[], &[]).unwrap_err().code,
        ErrorCode::ResourceConflict,
        "one run registers once"
    );
    assert!(registry.remove("run-a"));
    assert!(!registry.remove("run-a"));
    registry.register("run-b", &a_endpoints(), &[]).unwrap();
}

/// A grant spelled through a symlink names the same node the sandbox will
/// resolve it to, so it is refused in both start orders.
#[test]
fn a_grant_spelled_through_a_symlink_is_refused_in_both_orders() {
    let dir = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(real.join("ch-a")).unwrap();
    std::os::unix::fs::symlink(real.join("ch-a"), real.join("alias")).unwrap();
    let endpoint = vec![real.join("ch-a/orders.api.sock")];
    let spelled = |leaf: &str| grant(real.join("alias").join(leaf).to_str().unwrap());
    let directory = format!("{}/", real.join("alias").display());

    for b_grant in [spelled("orders.api.sock"), grant(&directory)] {
        let registry = EndpointRegistry::new();
        registry.register("run-a", &endpoint, &[]).unwrap();
        assert_eq!(
            registry
                .register("run-b", &[], &[b_grant.clone()])
                .unwrap_err()
                .code,
            ErrorCode::InvalidGrant,
            "B after A: {b_grant:?}"
        );
        let registry = EndpointRegistry::new();
        registry.register("run-b", &[], &[b_grant.clone()]).unwrap();
        assert_eq!(
            registry.register("run-a", &endpoint, &[]).unwrap_err().code,
            ErrorCode::ResourceConflict,
            "A after B: {b_grant:?}"
        );
    }
    // /tmp itself is a symlink on macOS: the same node spelled both ways.
    if std::fs::canonicalize("/tmp").unwrap() != Path::new("/tmp") {
        let registry = EndpointRegistry::new();
        let tmp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let real_endpoint = std::fs::canonicalize(tmp.path()).unwrap().join("x.sock");
        registry.register("run-a", &[real_endpoint], &[]).unwrap();
        let via_tmp = tmp.path().join("x.sock");
        assert_eq!(
            registry
                .register("run-b", &[], &[grant(via_tmp.to_str().unwrap())])
                .unwrap_err()
                .code,
            ErrorCode::InvalidGrant
        );
    }
}

/// macOS volumes are case-insensitive by default: a grant differing only in
/// case names the same node, and is refused in both orders.
#[cfg(target_os = "macos")]
#[test]
fn a_grant_differing_only_in_case_is_refused_in_both_orders() {
    let dir = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(real.join("ch-a")).unwrap();
    let endpoint = vec![real.join("ch-a/orders.api.sock")];
    let upper = grant(real.join("CH-A/ORDERS.API.SOCK").to_str().unwrap());
    let registry = EndpointRegistry::new();
    registry.register("run-a", &endpoint, &[]).unwrap();
    assert_eq!(
        registry
            .register("run-b", &[], &[upper.clone()])
            .unwrap_err()
            .code,
        ErrorCode::InvalidGrant
    );
    let registry = EndpointRegistry::new();
    registry.register("run-b", &[], &[upper]).unwrap();
    assert_eq!(
        registry.register("run-a", &endpoint, &[]).unwrap_err().code,
        ErrorCode::ResourceConflict
    );
}

#[test]
fn a_root_directory_grant_covers_everything() {
    let registry = EndpointRegistry::new();
    registry.register("run-a", &a_endpoints(), &[]).unwrap();
    assert_eq!(
        registry
            .register("run-b", &[], &[grant("/")])
            .unwrap_err()
            .code,
        ErrorCode::InvalidGrant
    );
}
