//! Export-surface gate for the published wasm32 artifact (bead
//! ley-line-open-a2099a).
//!
//! The release stages `leyline_sign.wasm` as a platform-independent
//! asset; consumers (cloister's workerd loader, browser verifiers) bind
//! these exports by name, so a rename — or a feature slip that drops one
//! — is a consumer-breaking change no native test can see. This parses
//! the artifact's real export section via wasmparser; a strings grep
//! would match symbol names surviving anywhere in the byte stream of an
//! artifact that exports nothing.
//!
//! `#[ignore]` because the assertion needs a wasm32 artifact on disk,
//! which plain `cargo test --workspace` does not build. `task
//! sign:wasm:verify` runs it explicitly after `task sign:wasm:build`;
//! release.yml and release-dryrun.yml run that pair against the STAGED
//! bytes. The workspace run reports it as ignored — visible, never a
//! silent pass.

use wasmparser::{ExternalKind, Parser, Payload};

#[test]
#[ignore = "needs the wasm32 artifact — run via `task sign:wasm:verify`"]
fn wasm_export_surface_is_exact() {
    let path = std::env::var("LEYLINE_WASM_ARTIFACT").expect(
        "LEYLINE_WASM_ARTIFACT must name the leyline_sign.wasm under test \
         (`task sign:wasm:verify` sets it)",
    );
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!("cannot read wasm artifact {path}: {e} — run `task sign:wasm:build` first")
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
        [
            "leyline_sign_data",
            "leyline_sign_data_without_attributes",
            "leyline_verify",
            "leyline_verify_cert_chain",
            "lsign_alloc",
            "lsign_free",
        ],
        "exported function surface drifted in {path}"
    );
    // Linear memory must be exported — without it a JS/workerd host cannot
    // reach the buffers `lsign_alloc` hands out.
    assert_eq!(
        memories,
        ["memory"],
        "linear memory export missing in {path}"
    );
}
