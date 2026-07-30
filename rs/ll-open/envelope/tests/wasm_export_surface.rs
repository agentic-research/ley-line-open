//! Export-surface gate for the published wasm32 artifact (bead
//! ley-line-open-be5f86). Mirrors leyline-sign's
//! tests/wasm_export_surface.rs — see that file for the full rationale
//! (real export-section parse, not a strings grep; `#[ignore]` so the
//! workspace run reports it as ignored rather than silently passing
//! without an artifact).
//!
//! For THIS artifact the equality assert carries an extra claim: the
//! set below is verification-only. leyline-sign's `#[no_mangle]`
//! exports (including its SIGNING entry points) would leak into this
//! cdylib if the manifest's `default-features = false` pin on
//! leyline-sign ever slipped — an appearance of `leyline_sign_data`
//! here is a trust-boundary regression, not a cosmetic drift.

use wasmparser::{ExternalKind, Parser, Payload};

#[test]
#[ignore = "needs the wasm32 artifact — run via `task envelope:wasm:verify`"]
fn wasm_export_surface_is_exact() {
    let path = std::env::var("LEYLINE_WASM_ARTIFACT").expect(
        "LEYLINE_WASM_ARTIFACT must name the leyline_envelope.wasm under test \
         (`task envelope:wasm:verify` sets it)",
    );
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!("cannot read wasm artifact {path}: {e} — run `task envelope:wasm:build` first")
    });

    // A valid module, not merely a byte stream containing an export
    // section — a truncated or corrupted artifact must fail here.
    wasmparser::validate(&bytes).expect("artifact must validate as a wasm module");

    let mut funcs = Vec::new();
    let mut memories = Vec::new();
    for payload in Parser::new(0).parse_all(&bytes) {
        if let Payload::ExportSection(section) = payload.expect("malformed wasm payload") {
            for export in section {
                let export = export.expect("malformed export entry");
                match export.kind {
                    ExternalKind::Func => funcs.push(export.name.to_string()),
                    ExternalKind::Memory => memories.push(export.name.to_string()),
                    _ => {}
                }
            }
        }
    }
    funcs.sort();

    // Literal names, deliberately not shared consts with src/ffi.rs — the
    // gate must move only when the exported surface deliberately changes,
    // not track whatever the source currently exports.
    assert_eq!(
        funcs,
        ["envelope_alloc", "envelope_free", "envelope_verify"],
        "exported function surface drifted in {path}"
    );
    // Linear memory must be exported — without it a JS/workerd host cannot
    // reach the buffers `envelope_alloc` hands out.
    assert_eq!(
        memories,
        ["memory"],
        "linear memory export missing in {path}"
    );
}
