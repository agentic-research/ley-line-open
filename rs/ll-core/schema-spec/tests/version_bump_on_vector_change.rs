//! `version_bump_on_vector_change` — enforce that every conformance
//! vector carries a `"version"` field matching its containing spec
//! directory (`cloister/<name>/v<n>`).
//!
//! ## Why (bead `ley-line-open-f47f43`)
//!
//! When a vector is edited to change semantic behavior, the containing
//! spec version MUST be bumped (v1 → v2 in a new sibling dir). Silently
//! mutating a v1 vector's expected bytes is a **silent contract break**
//! against every implementation that pinned to the v1 SHA-256 — nothing
//! about the SHA-256 changing tells a downstream reader *why* it
//! changed, and the pinned SHA-256 gate (see `verify_vectors_sha256`)
//! is content-blind.
//!
//! This test surfaces the anti-pattern by pinning the invariant
//! `vector.version == "cloister/<dir>/v<n>"`. A future refactor that
//! forgets to bump the vector's `"version"` string when moving it to
//! a new spec version (say v2) fails here, and a future author who
//! copies a v1 vector into a v2 sibling dir without touching the
//! `"version"` field also fails here.
//!
//! Behavior:
//! - Walks every `cloister-spec/<name>/v<n>/test-vectors/*.json` under
//!   the schema-spec crate root.
//! - Parses each JSON file and reads its top-level `"version"` field.
//! - Asserts that field byte-matches the string
//!   `cloister/<name>/v<n>` derived from the file's path.
//!
//! Non-goals:
//! - This test is **not** a semantic version-drift detector. Changing
//!   a vector's bytes without bumping the version still passes here
//!   as long as `"version"` matches the dir. What it catches is the
//!   *specific* mistake of copy-move-vectors-without-updating-version,
//!   which lives-and-dies in the git diff.
//! - Vectors WITHOUT a top-level `"version"` field are silently
//!   skipped so future spec authors can opt out of this convention
//!   for domain reasons (recorded here as a known blind spot to
//!   monitor).

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn version_bump_on_vector_change() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0usize;
    let mut skipped_no_version = Vec::new();

    for spec_version_dir in enumerate_spec_versions(&root) {
        let expected_version =
            derive_version_string(&root, &spec_version_dir).unwrap_or_else(|| {
                panic!(
                    "could not derive `cloister/<name>/v<n>` from {} — is the tree shape unusual?",
                    spec_version_dir.display()
                )
            });
        let vectors_dir = spec_version_dir.join("test-vectors");
        if !vectors_dir.is_dir() {
            continue;
        }
        for vector_path in enumerate_json_files(&vectors_dir) {
            let bytes = fs::read(&vector_path)
                .unwrap_or_else(|e| panic!("read {}: {e}", vector_path.display()));
            let parsed: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!("parse JSON {}: {e}", vector_path.display());
            });
            let Some(actual) = parsed.get("version").and_then(|v| v.as_str()) else {
                skipped_no_version.push(vector_path.clone());
                continue;
            };
            assert_eq!(
                actual,
                expected_version,
                "vector {} declares `version: {actual:?}` but sits under {} (expected {expected_version:?}). \
                 Either bump the vector's `version` field to match, or move it into a sibling spec-version dir.",
                vector_path.display(),
                spec_version_dir.display()
            );
            checked += 1;
        }
    }

    // Non-zero to catch a future refactor that empties the spec tree
    // or moves the vectors out from under the crate root.
    assert!(
        checked > 0,
        "version_bump_on_vector_change found no vectors to check — did the spec tree move?"
    );

    // Skipped-no-version list is informational, not a failure. If it
    // grows unexpectedly a future author can tighten the invariant to
    // require `version` on every vector. Print it so CI logs it.
    if !skipped_no_version.is_empty() {
        eprintln!(
            "version_bump_on_vector_change: {} vector(s) skipped (no top-level `version` field):",
            skipped_no_version.len(),
        );
        for path in &skipped_no_version {
            eprintln!("  - {}", path.display());
        }
    }
}

/// Enumerate every `<crate_root>/<name>/v<n>/` directory that looks
/// like a spec-version directory. Excludes hidden dirs, `src/`,
/// `tests/`, and `build-cache/` sub-tree shapes that don't fit the
/// `<name>/v<n>/` pattern.
fn enumerate_spec_versions(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name.starts_with('_') || name == "src" || name == "tests" {
            continue;
        }
        // Look for `v<n>` subdirs. Some capabilities may nest deeper
        // (e.g. `build-cache/v1/vectors/`) but the version dir is
        // always the immediate `v<n>` child.
        if let Ok(sub) = fs::read_dir(&path) {
            for sub_entry in sub.flatten() {
                let sub_path = sub_entry.path();
                if !sub_path.is_dir() {
                    continue;
                }
                let Some(sub_name) = sub_path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if is_version_dir(sub_name) {
                    out.push(sub_path);
                }
            }
        }
    }
    out.sort();
    out
}

fn is_version_dir(name: &str) -> bool {
    name.starts_with('v') && name.len() >= 2 && name[1..].chars().all(|c| c.is_ascii_digit())
}

/// Derive the `cloister/<name>/v<n>` string from the spec-version
/// directory's path. Returns None if the tree shape doesn't match.
fn derive_version_string(root: &Path, spec_version_dir: &Path) -> Option<String> {
    let rel = spec_version_dir.strip_prefix(root).ok()?;
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if parts.len() != 2 {
        return None;
    }
    Some(format!("cloister/{}/{}", parts[0], parts[1]))
}

fn enumerate_json_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json") {
            out.push(path);
        }
    }
    out.sort();
    out
}
