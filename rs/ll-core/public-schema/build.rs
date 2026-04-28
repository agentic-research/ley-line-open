fn main() {
    capnpc::CompilerCommand::new()
        .file("capnp/daemon.capnp")
        .run()
        .expect("capnp compile daemon.capnp");
}
