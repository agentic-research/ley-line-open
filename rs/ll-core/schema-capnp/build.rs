// Generates Rust bindings for `schemas/*.capnp` into OUT_DIR.
//
// Requires the `capnp` tool on PATH (documented formally in
// `docs/adr/0014-capnp-as-protocol.md`, bead `ley-line-open-ce8fd1`;
// for now: `brew install capnp` on macOS, package `capnproto` on
// Debian/Ubuntu). Re-runs whenever any schema file changes.
fn main() {
    let mut cmd = capnpc::CompilerCommand::new();
    cmd.src_prefix("schemas");

    for entry in std::fs::read_dir("schemas").expect("read schemas/ dir") {
        let path = entry.expect("schema dir entry").path();
        if path.extension().and_then(|s| s.to_str()) == Some("capnp") {
            println!("cargo:rerun-if-changed={}", path.display());
            cmd.file(&path);
        }
    }

    cmd.run()
        .expect("capnp codegen failed (is `capnp` on PATH?)");
}
