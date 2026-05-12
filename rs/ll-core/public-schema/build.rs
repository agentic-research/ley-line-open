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
}
