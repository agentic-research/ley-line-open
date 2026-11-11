# `cloister/connections/v1`: the connection broker's input document

**Status:** Slice 1 (credential-free relay), accepted 2026-11-11 with ADR-0046 "Slice 1"
(S2). Bead `ley-line-open-e9d940`. ADR-0046 D2 as a whole stays Proposed; this spec
fixes the strict subset slice 1 delivers, and every slice-1 document is a valid D2
document, so D3 and D4 extend the schema without changing the digest of any document
slice 1 accepts.

**Audience:** an issuer that emits the document (cloister) and a runner that authorizes
and serves it (LLO's broker). If your implementation canonicalizes every case in
`test-vectors/connections-cases.json` to its pinned bytes and `connectionsDigest`, and
refuses every reject with the pinned connection, field and reason, it is conformant.

## What this capability is

One document per run that declares the network connections the run's workload may make
through the host broker. Each connection joins one host UNIX endpoint, where the broker
listens, to one loopback TCP target, which the broker dials. The broker copies bytes; in
slice 1 it holds no credential, parses no protocol and resolves no name.

```json
{
  "connections": [
    {
      "appProtocol": "art.tcp",
      "endpoint": {
        "unix": "/private/tmp/cloister-ch-7Qm2/orders.api.sock"
      },
      "name": "orders-api",
      "target": {
        "tcp": {
          "host": "127.0.0.1",
          "port": 30080
        }
      }
    }
  ],
  "version": "cloister/connections/v1"
}
```

The issuer chooses every endpoint path (ADR-0046 S4). Cloister's convention is
`<channelDir>/<bundle>.<portName>.sock` in a fresh, realpath'd, mode-0700 channel
directory per run; the runner chooses and derives no path.

## Document map

- `README.md` (this file): the spec.
- `connections.schema.json`: the machine-readable shape (JSON Schema first, as
  confinement/v1, because the digest is taken over canonical JSON). JSON Schema cannot
  state every rule below; where it cannot, the parser is the stricter side and this
  README is normative. Verified by `verify_connections_schema`.
- `test-vectors/connections-canonical.json`: the ADR-0046 S2 example, canonical.
- `test-vectors/connections-multi.json`: three connections in cloister's shape, canonical.
- `test-vectors/connections-cases.json`: accepted inputs with their canonical bytes and
  `connectionsDigest`, and one reject per refusal with the connection, field and reason
  it is refused with. Every `input` is the document's text.
- `CONNECTIONS_DIGESTS.blake3`: BLAKE3-256 `connectionsDigest` pins for the two
  canonical files. Verified by `verify_connections_digest`.
- `VECTORS.sha256`: SHA-256 content-integrity pins ("the bytes on disk have not
  drifted"), a separate concern from the digest. Verified by `verify_vectors_sha256`.
- `ref-impl-py/connections_v1.py`: an independent reference (standard library only,
  with its own reader, canonical writer and BLAKE3; it imports no JSON or hashing
  module). `verify_connections_digest` runs it and requires it to reach every pinned
  digest and refusal the Rust parser (`leyline_schema_spec::connections`) reaches.

## Fields

| field | rule | refused at parse (reason code) |
|---|---|---|
| `version` | exactly `cloister/connections/v1`; checked first | absent (`missing-key`), not a string (`wrong-type`), any other value (`bad-version`) |
| any object | only the keys shown | an undefined key at any level (`unknown-key`); a key twice in one object (`duplicate-key`) |
| `connections` | a non-empty list | not a list (`wrong-type`), empty (`empty-connections`): a bundle with no connections emits no document |
| `name` | `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`, unique | not a label (`bad-name`), a repeat (`duplicate-name`) |
| `appProtocol` | `art.tcp`; required, so no document relies on a default | `art.http`, `art.grpc`, `art.postgres`, `art.mysql`, `art.redis` (`app-protocol-not-delivered`); any other value (`unknown-app-protocol`) |
| `endpoint` | `{"unix": path}` only | `{"tcp": port}` (`endpoint-tcp-not-delivered`) |
| `endpoint.unix` | absolute; at most 103 bytes; no empty component (`//`); no trailing `/`; no `.` or `..` component; unique | `endpoint-not-absolute`, `endpoint-too-long`, `endpoint-empty-component`, `endpoint-trailing-slash`, `endpoint-dot-component`, `duplicate-endpoint` |
| `target` | `{"tcp": {...}}` only | `unix`, `https`, `cloudSql` (`target-not-delivered`) |
| `target.tcp` | `host` and `port` only | `tls` (`tls-not-delivered`) |
| `target.tcp.host` | a loopback IP literal in its one spelling: an IPv4 dotted quad in `127.0.0.0/8` with no leading zero in any octet, or exactly `::1` | a name, including `localhost` (`host-is-name`); an address outside loopback (`non-loopback-target`); loopback spelled another way, such as `127.000.0.1`, `0:0:0:0:0:0:0:1` or `::ffff:127.0.0.1` (`non-canonical-loopback`) |
| `target.tcp.port` | a JSON integer 1–65535, with no sign, fraction or exponent | `30080.0`, `3.008e4`, `"30080"`, `true` (`port-not-integer`); `0`, `65536`, `-1` (`port-out-of-range`) |
| `credentialRef` | absent | present (`credential-ref-present`); D2 rule 4 already refuses it on `art.tcp` |
| every string | printable ASCII, U+0020–U+007E, with no `"` and no `\` | a NUL, a control character, a non-ASCII byte or any escape, even one that decodes to an allowed character (`string-not-printable-ascii`) |
| every value | a JSON value of the type shown; `null` is never a value | `wrong-type` |

Text that is not JSON, or JSON followed by more bytes, is `malformed-json`.

**Every refusal names the connection and the field.** The connection is its `name` when
that is a valid label, otherwise its position as `#<index>` (from 0); a document-level
refusal names no connection. The field is dotted from the connection
(`target.tcp.host`) or from the document (`version`). The reason codes above are stable.

**Which refusal is reported** when a document breaks several rules is fixed, so two
implementations report the same one:

1. syntax (`malformed-json`), over the whole text;
2. the document is an object; `version`'s first occurrence: present, a clean string,
   equal to `cloister/connections/v1`;
3. the document's keys, in input order: a duplicate, then an undefined key; then
   `connections` and `version` present;
4. `connections` is a non-empty list;
5. each connection in list order: it is an object; its keys in input order (a duplicate,
   then `credentialRef`, then an undefined key); `appProtocol`, `endpoint`, `name` and
   `target` present, in that order; then their values in that order, each object's keys
   checked the same way before its values (`endpoint`: `tcp` before an undefined key;
   `target`: `cloudSql`, `https` and `unix` before an undefined key; `target.tcp`: `tls`
   before an undefined key); within `endpoint.unix`, the path rules in the order of the
   table; then whether its name, then its endpoint, repeats an earlier connection's.

**Why loopback IP literals only.** No name is resolved, ever, so there is no resolver to
poison and no answer that changes between check and dial: the dialled address is the
committed bytes. Slice 1 has no TLS client, so the only targets it can serve without a
plaintext network hop are local. The type represents every loopback literal; which of
them a runtime delivers is a runtime check run at start (ADR-0046 S2: slice 1 delivers
`127.0.0.1` and refuses any other loopback target by name, "loopback address not
delivered by this runtime"). Widening that set changes no schema, vector or digest.

**Why 103 bytes.** It is the smallest `sun_path` capacity among the supported platforms
(macOS: 104 including the NUL), so whether a vector is valid does not depend on the host
that checks it.

## Canonical form

1. confinement/v1 §7 rules 1–4, verbatim: UTF-8 without a BOM; keys sorted in ASCII byte
   order at every level; two-space indentation; no trailing whitespace or newline (the
   last byte is the outer `}`); a `null` field is omitted (a slice-1 document has none).
2. `connections` is sorted by `name` in byte order. A one-element list stays a list:
   unlike confinement/v1 §7 rule 5, no earlier document's digest needs the scalar form.
3. An empty `connections` list is refused (a document with no connections does not
   exist).
4. Every string is printable ASCII with no `"` and no `\`, so no JSON escape is ever
   emitted, and serializers that differ on escaping reach the same bytes.
5. `port` is a decimal integer with no sign, fraction, exponent or leading zero.

Python's `json.dumps(doc, sort_keys=True, indent=2)` writes these bytes for a document
that passed the parse rules; the reference in `ref-impl-py/` writes them by hand instead,
so that it does not depend on that library's choices.

A runner that receives a document as authority (ADR-0046 S3, `RunGrant.connectionsDocument`)
requires the received bytes to equal their canonical form and refuses them otherwise; it
never normalizes.

## `connectionsDigest`

`connectionsDigest` is BLAKE3-256 over the canonical bytes, carried as a `DigestRef` with
`algorithm` = `blake3-256` and `value` = 64 lowercase hex characters. It names the
connections document's canonical bytes and nothing else: it is not `confinementDigest`,
a blob hash, or any arena or store root. Both it and `confinementDigest` are plain
BLAKE3 over canonical JSON; the in-document `version` string, which each parser checks
first, keeps the two domains apart (`confinement-version` in the vectors).

## Conformance

```sh
cd rs && cargo test -p leyline-schema-spec --test verify_connections_digest \
  --test verify_connections_schema --test verify_vectors_sha256
python3 -I rs/ll-core/schema-spec/connections/v1/ref-impl-py/connections_v1.py \
  conformance rs/ll-core/schema-spec/connections/v1
```

Cloister reproduces `connectionsDigest` byte for byte from these vectors before it emits
one (ADR-0046 S3, "Consumer surfaces").

## Not in this spec

The cross-document rule against the confinement manifest's §6 grants, the endpoint
checks on the host (path absent, parent private), the endpoint registry, the delivered
loopback set and the tier check are runtime checks at run start (ADR-0046 S2, S4, S6),
not parse rules. Credential references, other `appProtocol` handling, TLS, and
`endpoint {"tcp"}` are D3, D4 and D5 work.
