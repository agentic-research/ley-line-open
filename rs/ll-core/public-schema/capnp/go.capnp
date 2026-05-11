@0xd12a1c51fedd6c88;

# Vendored go.capnp — defines the $Go.* annotations consumed by capnpc-go.
# See rs/ll-core/schema-capnp/schemas/go.capnp for the canonical comment.

annotation package(file) :Text;
annotation import(file) :Text;
annotation doc(struct, field, enum) :Text;
annotation tag(enumerant) :Text;
annotation notag(enumerant) :Void;
annotation customtype(field) :Text;
annotation name(struct, field, union, enum, enumerant, interface, method, param, annotation, const, group) :Text;

$package("gocp");
$import("capnproto.org/go/capnp/v3/std/go");
