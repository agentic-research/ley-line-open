// Daemon protocol schema — Cap'n Proto types generated from capnp/daemon.capnp.
//
// This is the public contract between the ley-line daemon and consumers
// (mache, CLI tools, etc.). The .capnp file is the single source of truth.

#[allow(unused, clippy::all)]
pub mod daemon_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/daemon_capnp.rs"));
}
