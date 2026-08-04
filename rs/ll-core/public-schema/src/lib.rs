// Daemon protocol schema — Cap'n Proto types generated from capnp/daemon.capnp.
//
// This is the public contract between the ley-line daemon and consumers
// (mache, CLI tools, etc.). The .capnp file is the single source of truth.

#[allow(unused, clippy::all)]
pub mod daemon_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/daemon_capnp.rs"));
}

#[doc(hidden)]
#[allow(unused, clippy::all)]
pub mod _traits_capnp {
    include!(concat!(env!("OUT_DIR"), "/_traits_capnp.rs"));
}

// execution/v1 substrate types generated from the normative schema held by
// leyline-schema-spec. Service behavior is implemented above these data types;
// clients must not hand-mirror RunSpec, RunGrant, events, errors, or receipts.
#[allow(unused, clippy::all)]
pub mod execution_capnp {
    include!(concat!(env!("OUT_DIR"), "/execution/v1/execution_capnp.rs"));
}
