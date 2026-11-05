//! Computes `LEYLINE_BUILD_VERSION`, the version every surface of the
//! binary reports (bead `ley-line-open-5139a3`); the rule is in
//! `src/build_version.rs`.
//!
//! The build script reruns when `LEYLINE_RELEASE_VERSION` changes or when
//! git's HEAD, the branch it names, or the index moves (a commit, checkout
//! or stage). An unstaged edit alone does not rerun it, so `.dirty` reflects
//! the tracked tree as of the last such event.

#[path = "src/build_version.rs"]
mod build_version;

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Rerun when the revision or the index moves. Only existing paths are
/// emitted: cargo reruns every build for a path that does not exist.
fn watch_git(dir: &Path) {
    let resolve = |args: &[&str]| git(dir, args).map(|p| dir.join(p));
    let (Some(git_dir), Some(common_dir)) = (
        resolve(&["rev-parse", "--git-dir"]),
        resolve(&["rev-parse", "--git-common-dir"]),
    ) else {
        return;
    };
    let mut watched: Vec<PathBuf> = vec![
        git_dir.join("HEAD"),
        git_dir.join("index"),
        common_dir.join("packed-refs"),
    ];
    if let Some(branch) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        watched.push(common_dir.join(branch));
    }
    for path in watched.into_iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/build_version.rs");
    println!("cargo:rerun-if-env-changed=LEYLINE_RELEASE_VERSION");

    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let crate_version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets it");
    let release = std::env::var("LEYLINE_RELEASE_VERSION")
        .ok()
        .filter(|v| !v.is_empty());

    let revision = if release.is_none() {
        watch_git(&dir);
        git(&dir, &["rev-parse", "--short=12", "HEAD"]).map(|short_sha| build_version::Revision {
            short_sha,
            dirty: git(&dir, &["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty()),
        })
    } else {
        None
    };

    match build_version::build_version(&crate_version, release.as_deref(), revision.as_ref()) {
        Ok(version) => println!("cargo:rustc-env=LEYLINE_BUILD_VERSION={version}"),
        Err(e) => panic!("{e}"),
    }
}
