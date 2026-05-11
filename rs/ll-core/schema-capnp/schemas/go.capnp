@0xd12a1c51fedd6c88;

# Vendored go.capnp — defines the $Go.* annotations consumed by capnpc-go.
# Source: capnproto.org/go/capnp/v3/std/go.capnp (v3.1.0-alpha.2).
#
# This file is referenced by `using Go = import "/go.capnp";` in the sibling
# schema files. It exists in this directory so capnp's `-I` include path
# resolves the root-anchored import for both:
#   - capnpc-rust (build.rs adds this dir as an import path)
#   - capnpc-go (regen.sh in clients/go/leyline-schema/ passes -I)
#
# capnpc-rust IGNORES every $Go.* annotation it sees (annotations are
# extensions; unknown ones are no-ops in the codegen path). So this
# vendored file is inert for the Rust build — it only needs to *resolve*.

annotation package(file) :Text;
# The Go package name for the generated file.

annotation import(file) :Text;
# The Go import path that the generated file is accessible from.

annotation doc(struct, field, enum) :Text;
annotation tag(enumerant) :Text;
annotation notag(enumerant) :Void;
annotation customtype(field) :Text;
annotation name(struct, field, union, enum, enumerant, interface, method, param, annotation, const, group) :Text;

$package("gocp");
$import("capnproto.org/go/capnp/v3/std/go");
