//! `verify_vectors_sha256` — walk every `VECTORS.sha256` file in the spec
//! tree and re-hash the listed vector files, asserting SHA-256 equality.
//!
//! The spec dirs ship two `VECTORS.sha256` files, one per capability that
//! publishes conformance vectors:
//!
//! - `credential-isolation/v1/VECTORS.sha256`
//! - `build-cache/v1/vectors/VECTORS.sha256`
//!
//! Each file uses the standard `sha256sum` line shape (`<hex>  <path>`),
//! where `<path>` is relative to the directory containing the
//! `VECTORS.sha256` file. Blank lines and `#`-comments are ignored so
//! future spec authors can annotate the pin file without breaking the
//! parser.
//!
//! Any drift between a pinned digest and the bytes on disk fails this
//! test — the same guarantee `shasum -a 256 -c VECTORS.sha256` gives
//! shell users, encoded so `cargo test -p leyline-schema-spec` enforces
//! it on every workspace test run.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Every `VECTORS.sha256` file in the spec tree, expressed relative to
/// `CARGO_MANIFEST_DIR`. Extend this list when a new capability's spec
/// starts publishing pinned vectors.
const VECTORS_FILES: &[&str] = &[
    "credential-isolation/v1/VECTORS.sha256",
    "build-cache/v1/vectors/VECTORS.sha256",
    // Bead ley-line-open-a2f94f: kernel-confinement IDL, sibling to
    // credential-isolation/v1. Ships a canonical `ConfinementManifest`
    // JSON vector so two independent implementations reach the same
    // BLAKE3-256 digest under §6's canonical serialization rules.
    "confinement/v1/VECTORS.sha256",
];

#[test]
fn verify_vectors_sha256() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0usize;

    for rel in VECTORS_FILES {
        let manifest_path = root.join(rel);
        let manifest_dir = manifest_path.parent().expect("manifest has a parent");
        let contents = fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));

        for (lineno, line) in contents.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (expected_hex, path_str) = split_sha256_line(trimmed).unwrap_or_else(|| {
                panic!(
                    "{}:{} — malformed sha256sum line: {trimmed:?}",
                    manifest_path.display(),
                    lineno + 1
                )
            });
            let vector_path = manifest_dir.join(path_str);
            let actual_hex = hash_file(&vector_path);
            assert_eq!(
                actual_hex,
                expected_hex.to_ascii_lowercase(),
                "sha256 mismatch for {} pinned by {}",
                vector_path.display(),
                manifest_path.display()
            );
            checked += 1;
        }
    }

    // Non-zero to catch a future refactor that empties the manifest list
    // or moves the manifest files out from under the crate root.
    assert!(
        checked > 0,
        "verify_vectors_sha256 found no vectors to check — did the spec tree move?"
    );
}

/// Split a `sha256sum` line into `(hex, path)`. The canonical shape is
/// `<64-hex>  <path>` (two spaces); we accept any run of whitespace to
/// stay forgiving of hand-edited pin files.
fn split_sha256_line(line: &str) -> Option<(&str, &str)> {
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

fn hash_file(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read vector {}: {e}", path.display()));
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}
