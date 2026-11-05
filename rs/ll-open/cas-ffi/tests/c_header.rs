//! The checked-in C header (`include/leyline_cas_ffi.h`, cbindgen output)
//! declares every exported function with the exact C signature the Rust
//! definition has (bead `ley-line-open-12e252`).
//!
//! A C translation unit assigns each function to a pointer of the type the
//! Rust `extern "C"` signature implies. A missing prototype, a changed
//! parameter list or a changed return type is a compile error under
//! `-Werror`; `-fsyntax-only` keeps the test free of the staticlib, so it
//! runs wherever a C compiler does. The exported surface itself is gated
//! twice more: `wasm_export_surface.rs` on the wasm artifact, and the FFI
//! unit tests through the C ABI.
//!
//! Regenerate the header with
//! `cbindgen --config cbindgen.toml --crate leyline-cas-ffi --output include/leyline_cas_ffi.h .`
//! from this crate's directory.

use std::process::Command;

const PROBE: &str = r#"
#include "leyline_cas_ffi.h"

int32_t (*const hash_bytes)(const uint8_t *, uintptr_t, uint8_t *, uintptr_t) =
    leyline_hash_bytes;
int32_t (*const derive_key)(const uint8_t *, uintptr_t, const uint8_t *, uintptr_t,
                            uint8_t *, uintptr_t) = leyline_derive_key;
"#;

#[test]
fn header_declares_every_export_with_its_rust_signature() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let header = crate_dir.join("include/leyline_cas_ffi.h");
    assert!(header.is_file(), "missing {}", header.display());

    let tmp = tempfile::TempDir::new().unwrap();
    let probe = tmp.path().join("probe.c");
    std::fs::write(&probe, PROBE).unwrap();
    let out = Command::new("cc")
        .args(["-std=c11", "-fsyntax-only", "-Wall", "-Werror"])
        .arg(format!("-I{}", crate_dir.join("include").display()))
        .arg(&probe)
        .output()
        .expect("a C compiler (`cc`) must be on PATH");
    assert!(
        out.status.success(),
        "the header does not match the exported Rust signatures:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
