fn main() {
    capnpc::CompilerCommand::new()
        // Resolve `using Go = import "/go.capnp";` via the vendored
        // capnp/go.capnp (inert for capnpc-rust; consumed by capnpc-go).
        // See clients/go/leyline-schema/README.md.
        .import_path("capnp")
        // Wire `using Json = import "/capnp/compat/json.capnp";` to the
        // capnp-json crate's annotation IDs so the generated bindings
        // can be consumed by `capnp_json::to_json` / `from_json`. The
        // fileId 0x8ef99297a43a5e34 is capnp-json's published id for
        // its json.capnp; see capnp-json's README.
        .crate_provides("capnp_json", [0x8ef99297a43a5e34])
        .file("capnp/daemon.capnp")
        .run()
        .expect("capnp compile daemon.capnp");

    // execution/v1 lives with its normative spec and conformance vectors.
    // Compile that exact file rather than maintaining a public-schema copy:
    // Rust, JSON Schema, and MCP projections therefore share one IDL input.
    let spec_root = std::path::PathBuf::from(leyline_schema_spec::SPEC_DIR);
    capnpc::CompilerCommand::new()
        .src_prefix(&spec_root)
        .file(spec_root.join("_traits.capnp"))
        .file(spec_root.join("execution/v1/execution.capnp"))
        .run()
        .expect("capnp compile execution/v1/execution.capnp");
}
