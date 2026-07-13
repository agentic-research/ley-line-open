//! `verify_confinement_digest` — compute the BLAKE3-256
//! `confinementDigest` per `confinement/v1` README §7 and assert it
//! matches the pin in `CONFINEMENT_DIGESTS.blake3`.
//!
//! ## Why (bead `ley-line-open-193170`)
//!
//! `VECTORS.sha256` is the SHA-256 content-integrity pin used across
//! the schema-spec tree — "have the bytes on disk drifted from the
//! bytes shipped in the tag?" That is a **content-integrity** check.
//!
//! `confinement/v1` README §7 also names a BLAKE3-256 digest — the
//! `confinementDigest` a substrate runner computes at bundle-start and
//! cross-checks against the workload identity's cert extension. That
//! is an **identity-commitment** check, and its digest is BLAKE3-256
//! (Σ substrate discipline; matches `leyline_core::substrate::
//! ContentAddressed for [u8]`).
//!
//! Both digests are load-bearing on distinct concerns; conflating them
//! (as the spec initially did — see the pre-fix wording of §6/§8) hides
//! the cross-impl conformance property this test exists to prove.
//!
//! ## Cross-impl conformance
//!
//! Cloister computed `d9b5b7270bb6e5ec068aec92798dd76b0f71d1fe2640b3a09833b7742d51c617`
//! for `test-vectors/manifest-canonical.json` via the shared substrate
//! hash (`leyline-cas-ffi`). This test computes the same value via the
//! `blake3` crate directly. Byte-identical results prove
//! substrate-Σ = direct-blake3 for canonical manifest bytes.
//!
//! Behavior:
//! - Walks every `<name>/v<n>/CONFINEMENT_DIGESTS.blake3` file listed
//!   in `CONFINEMENT_DIGEST_FILES`.
//! - Each line uses the `sha256sum`-style shape (`<64-hex>  <path>`),
//!   where the 64-hex is a BLAKE3-256 digest and the path is relative
//!   to the directory containing the pin file. Blank lines and
//!   `#`-comments are ignored.
//! - Re-computes BLAKE3-256 for each referenced file and asserts
//!   equality against the pinned digest.
//!
//! Any drift between the pinned confinementDigest and the bytes on
//! disk fails this test — the same guarantee the Σ substrate would
//! give a runner at bundle-start, encoded so
//! `cargo test -p leyline-schema-spec` enforces it on every workspace
//! test run.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `CONFINEMENT_DIGESTS.blake3` file in the spec tree,
/// expressed relative to `CARGO_MANIFEST_DIR`. Extend when a new
/// capability that names a BLAKE3-256 identity digest ships pins.
const CONFINEMENT_DIGEST_FILES: &[&str] = &["confinement/v1/CONFINEMENT_DIGESTS.blake3"];

#[test]
fn verify_confinement_digest() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0usize;

    for rel in CONFINEMENT_DIGEST_FILES {
        let manifest_path = root.join(rel);
        let manifest_dir = manifest_path.parent().expect("manifest has a parent");
        let contents = fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));

        for (lineno, line) in contents.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (expected_hex, path_str) = split_digest_line(trimmed).unwrap_or_else(|| {
                panic!(
                    "{}:{} — malformed BLAKE3 pin line: {trimmed:?}",
                    manifest_path.display(),
                    lineno + 1
                )
            });
            let vector_path = manifest_dir.join(path_str);
            let actual_hex = blake3_hex(&vector_path);
            assert_eq!(
                actual_hex,
                expected_hex.to_ascii_lowercase(),
                "BLAKE3-256 mismatch for {} pinned by {} — the substrate's \
                 confinementDigest (README §7) has drifted from the canonical \
                 manifest bytes on disk",
                vector_path.display(),
                manifest_path.display()
            );
            checked += 1;
        }
    }

    // Non-zero to catch a future refactor that empties the manifest
    // list or moves the pin files out from under the crate root.
    assert!(
        checked > 0,
        "verify_confinement_digest found no digests to check — did the spec tree move?"
    );
}

/// Split a `<64-hex>  <path>` line into `(hex, path)`. Same forgiving
/// whitespace tolerance as `verify_vectors_sha256`.
fn split_digest_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.splitn(2, char::is_whitespace);
    let hex = parts.next()?.trim();
    let path = parts.next()?.trim();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if path.is_empty() {
        return None;
    }
    Some((hex, path))
}

fn blake3_hex(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    blake3::hash(&bytes).to_hex().to_string()
}
