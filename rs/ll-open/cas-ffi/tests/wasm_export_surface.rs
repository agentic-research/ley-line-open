//! Export-surface gate for the published wasm32 artifact (bead
//! ley-line-open-a2099a). Mirrors leyline-sign's
//! tests/wasm_export_surface.rs — see that file for the full rationale
//! (real export-section parse, not a strings grep; `#[ignore]` so the
//! workspace run reports it as ignored rather than silently passing
//! without an artifact).

use wasmparser::{ExternalKind, Parser, Payload};

#[test]
#[ignore = "needs the wasm32 artifact — run via `task cas-ffi:wasm:verify`"]
fn wasm_export_surface_is_exact() {
    let path = std::env::var("LEYLINE_WASM_ARTIFACT").expect(
        "LEYLINE_WASM_ARTIFACT must name the leyline_cas_ffi.wasm under test \
         (`task cas-ffi:wasm:verify` sets it)",
    );
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!("cannot read wasm artifact {path}: {e} — run `task cas-ffi:wasm:build` first")
    });

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

    // Literal name, deliberately not a shared const with src/ffi.rs — the
    // gate must move only when the exported surface deliberately changes.
    assert_eq!(
        funcs,
        ["leyline_hash_bytes"],
        "exported function surface drifted in {path}"
    );
    // Linear memory must be exported — the caller-provided input/output
    // buffers live in it.
    assert_eq!(
        memories,
        ["memory"],
        "linear memory export missing in {path}"
    );
}
