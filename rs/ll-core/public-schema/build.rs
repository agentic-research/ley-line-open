fn main() {
    capnpc::CompilerCommand::new()
        // Resolve `using Go = import "/go.capnp";` via the vendored
        // capnp/go.capnp (inert for capnpc-rust; consumed by capnpc-go).
        // See clients/go/leyline-schema/README.md.
        .import_path("capnp")
        .file("capnp/daemon.capnp")
        .run()
        .expect("capnp compile daemon.capnp");
}
