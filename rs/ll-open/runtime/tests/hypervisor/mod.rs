//! What a test that boots a real libkrun guest needs from its host: the guest
//! kernel (libkrunfw), a hypervisor-entitled VMM binary, and a static Linux
//! guest program. Shared by the `libkrun_guest_*` and
//! `libkrun_virtiofs_backend` test binaries.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// libkrunfw, the one library the linked VMM loads at run time — or `None`,
/// reported as a skip that is NOT a pass.
///
/// The gate is the actual precondition rather than a blanket opt-out: these
/// tests run wherever `LEYLINE_LIBKRUNFW_PATH` names libkrunfw. libkrun itself
/// is linked into the binary under test, so there is no library path to
/// supply for it. `LEYLINE_REQUIRE_HYPERVISOR` turns absence into a hard
/// failure in any environment that claims it can boot a guest: a green tick
/// that tested nothing is worse than a red one, because nobody investigates it.
pub fn libkrunfw_or_skip() -> Option<PathBuf> {
    let Some(libkrunfw) = std::env::var_os("LEYLINE_LIBKRUNFW_PATH") else {
        assert!(
            std::env::var_os("LEYLINE_REQUIRE_HYPERVISOR").is_none(),
            "LEYLINE_REQUIRE_HYPERVISOR is set, so this environment claims a \
             hypervisor, but LEYLINE_LIBKRUNFW_PATH is not set. Refusing to \
             report a pass for a microVM test that did not boot a microVM."
        );
        eprintln!(
            "SKIPPED (NOT a pass): no hypervisor. This test boots a real libkrun \
             guest and needs LEYLINE_LIBKRUNFW_PATH, plus the {} rustup \
             target. Nothing about the microVM backend was verified by this \
             run. Set LEYLINE_REQUIRE_HYPERVISOR to make this absence fail.",
            guest_target()
        );
        return None;
    };
    Some(
        PathBuf::from(libkrunfw)
            .canonicalize()
            .expect("canonical LEYLINE_LIBKRUNFW_PATH"),
    )
}

/// The variable the platform loader searches for libkrunfw, set to the
/// directory holding it.
pub fn library_path_env(libkrunfw: &Path) -> (&'static str, &Path) {
    let dir = libkrunfw.parent().expect("libkrunfw parent");
    if cfg!(target_os = "macos") {
        ("DYLD_LIBRARY_PATH", dir)
    } else {
        ("LD_LIBRARY_PATH", dir)
    }
}

/// The guest is Linux on the host's architecture.
pub fn guest_target() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

/// Compile a one-file Rust program into a static Linux guest executable.
pub fn compile_guest(source: &Path, output: &Path) {
    let status = Command::new("rustc")
        .args(["--edition", "2021", "--target"])
        .arg(guest_target())
        .args([
            "-C",
            "target-feature=+crt-static",
            "-C",
            "linker=rust-lld",
            "-C",
            "strip=symbols",
            "-o",
        ])
        .arg(output)
        .arg(source)
        .status()
        .expect("compile static Linux guest");
    assert!(status.success(), "guest compilation failed: {status}");
}

/// Copy `binary` to `dest` and give the copy the hypervisor entitlement
/// Hypervisor.framework requires (macOS); elsewhere, a plain copy. The copy is
/// what runs: signing the cargo-built original in place would race other test
/// binaries that exec it.
pub fn entitled_copy(binary: &Path, dest: &Path) {
    fs::copy(binary, dest).expect("copy VMM binary for signing");
    if !cfg!(target_os = "macos") {
        return;
    }
    let entitlements = dest.with_extension("entitlements.plist");
    fs::write(
        &entitlements,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>com.apple.security.hypervisor</key><true/>
</dict></plist>
"#,
    )
    .expect("write hypervisor entitlement");
    let status = Command::new("codesign")
        .args(["--force", "--sign", "-", "--entitlements"])
        .arg(&entitlements)
        .arg(dest)
        .status()
        .expect("ad-hoc sign VMM binary");
    assert!(status.success(), "codesign failed: {status}");
}
