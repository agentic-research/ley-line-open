// Generates Rust bindings for `schemas/*.capnp` into OUT_DIR.
//
// Requires the `capnp` tool on PATH (documented formally in
// `docs/adr/0014-capnp-as-protocol.md`, bead `ley-line-open-ce8fd1`;
// for now: `brew install capnp` on macOS, package `capnproto` on
// Debian/Ubuntu). Re-runs whenever any schema file changes.
fn main() {
    let mut cmd = capnpc::CompilerCommand::new();
    cmd.src_prefix("schemas");

    // Resolve `using Go = import "/go.capnp";` in the schema files. The
    // vendored `schemas/go.capnp` defines the inert $Go.* annotations that
    // capnpc-go consumes; capnpc-rust ignores them but still needs the
    // import to resolve. See clients/go/leyline-schema/README.md.
    cmd.import_path("schemas");

    for entry in std::fs::read_dir("schemas").expect("read schemas/ dir") {
        let path = entry.expect("schema dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("capnp") {
            continue;
        }
        // Skip the vendored go.capnp annotations file — it's an include
        // target, not a producer schema. (Compiling it would generate an
        // empty `gocp_capnp.rs` we never use.)
        if path.file_name().and_then(|s| s.to_str()) == Some("go.capnp") {
            println!("cargo:rerun-if-changed={}", path.display());
            continue;
        }
        println!("cargo:rerun-if-changed={}", path.display());
        cmd.file(&path);
    }

    cmd.run()
        .expect("capnp codegen failed (is `capnp` on PATH?)");
}
