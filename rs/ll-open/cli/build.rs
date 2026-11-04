//! macOS `fuse` builds: put fuse-t's library directory on the binary's rpath.
//!
//! fuse-t installs `libfuse-t.dylib` with the install name
//! `@rpath/libfuse-t.dylib`. fuser's build script links it through the
//! `pkg-config` crate, which forwards `-L` and `-l` but drops the
//! `-Wl,-rpath,...` that `rs/pkgconfig/fuse.pc` declares, so the binary
//! carried no LC_RPATH and dyld refused to launch it ("Library not loaded:
//! @rpath/libfuse-t.dylib") — for every command, not only mounts. `cargo test`
//! hid it by putting the link search path on DYLD_FALLBACK_LIBRARY_PATH; the
//! release smoke (`tools/cli_smoke.sh --mount`) runs the binary directly and
//! found it.
//!
//! A `mount` build without `fuse` mounts NFS only, links no fuser and needs
//! no pkg-config (bead `ley-line-open-9d4712`).
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    let fuse = std::env::var_os("CARGO_FEATURE_FUSE").is_some();
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    if !(fuse && macos) {
        return;
    }
    let out = Command::new("pkg-config")
        .args(["--variable=libdir", "fuse"])
        .output()
        .expect("run pkg-config to find fuse-t's library directory");
    assert!(
        out.status.success(),
        "pkg-config cannot resolve `fuse` (PKG_CONFIG_PATH must include rs/pkgconfig, \
         as the Taskfile sets it): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let libdir = String::from_utf8(out.stdout).expect("pkg-config libdir is UTF-8");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{}", libdir.trim());
}
