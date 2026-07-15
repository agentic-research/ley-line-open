# `leyline-net/v1` — generic wire frames (Manifest / ToolCall / ToolResult)

**Status:** Frozen (2026-07-15, bead `ley-line-open-083344`). Vector bytes
are pinned; changes follow the errata / version-bump rules in §Version
policy.
**Audience:** anyone implementing an encoder or decoder for the
leyline-net frame vocabulary — cloister (workerd TS gateway),
cloister-companion (Rust sidecar), rosary (`rsry mcp --ipc-socket`), Go
consumers of `clients/go/leyline-schema/net`, and any future runtime.

**Non-goals:** v1 does NOT specify key exchange (X25519 handshake),
transport negotiation, or the raptorq FEC layer — those are closed
leyline-net concerns. It does NOT re-specify MCP semantics; `ToolResult`
mirrors the MCP `tools/call` result shape so gateways can re-emit it
verbatim.

## What this capability is

The typed frame vocabulary crossing process and host boundaries between
leyline-net peers:

- **`Manifest`** — the unforgeable per-message header: monotonic
  `sequence`, Ed25519 `publicKey` (32B) + `signature` (64B, over
  `sequence LE-8 ‖ contentHash`), and `contentHash` (SHA-256 of the
  AEAD plaintext).
- **`ToolCall`** — request payload: `upstreamId`, `toolName`,
  `argumentsJson` (canonical JSON bytes).
- **`ToolResult`** — response payload: `content` list (union per item:
  `text` / `binary` / `resource`) + `isError`.

The schema source of truth is
[`rs/ll-core/schema-capnp/schemas/net.capnp`](../../../schema-capnp/schemas/net.capnp)
(fileId `0xa25bb2a310446125`). It was lifted verbatim (field-for-field,
ordinal-for-ordinal) from cloister's `wire/cloister.capnp`
(`@0xa1c0157e2a1e0001`, cloister ADR-0005); the fileId is new because
downstream files will import this one and capnp forbids ID sharing, and
a fileId never reaches the wire — the pinned vectors prove the
encodings are byte-identical across both schema files.

## History and canonicalization

The frames were born in cloister (ADR-0005) and vendored by rosary as
`schemas/cloister.capnp`. A leyline-named protocol had its canonical
copy in a cloister subdirectory, with rosary re-vendoring by hand in
lockstep. This spec dir + `net.capnp` make LLO the canonical home:

| Consumer | Before | After (downstream beads) |
|---|---|---|
| cloister `wire/cloister.capnp` | canonical copy | repoints to / re-vendors `net.capnp` (cloister-side bead at PR time) |
| rosary `schemas/cloister.capnp` | vendored from cloister | re-vendors from LLO `net.capnp` (bead `rosary-086973`) |
| cloister Go bindings `clients/go/cloister-schema/wire` | generated from cloister copy | unchanged until cloister repoints |
| LLO Go bindings `clients/go/leyline-schema/net` | — | generated from `net.capnp` |

## Wire summary

Full envelope (network-crossing transports):

```
[manifest length :2 bytes BE]
[manifest bytes  :variable]    -- serialized Manifest
[aead nonce      :12 bytes]    -- ChaCha20-Poly1305 nonce
[aead ciphertext :variable]    -- AEAD(serialized ToolCall | ToolResult),
                               --   authenticated data = manifest bytes
```

Intra-cluster transports (cloister↔companion loopback HTTP, rosary UDS)
exchange plain serialized `ToolCall`/`ToolResult` with no envelope —
see `wire/frames.md` §Envelope for the trust-boundary rationale.

Canonical encoding contract (single segment, unpacked, composite-list
size code 7) and the two circulating byte-forms (reference vs strict
canonical) are specified in `wire/frames.md`.

## Conformance

- `test-vectors/` pins 12 values × 2 byte-forms with BLAKE3 + SHA-256
  digests (`digests.json`) and a `VECTORS.sha256` sha256sum manifest.
- Rust gate: `rs/ll-core/schema-capnp/tests/leyline_net_vectors.rs`
  (BLAKE3 pins hardcoded in test source; encode byte-equality; decode
  field-equality; `capnp eval` cross-schema gates).
- SHA-256 gate: `rs/ll-core/schema-spec/tests/verify_vectors_sha256.rs`.
- Second implementations (the "if only one implementation exists, the
  wire isn't a wire" requirement): cloister's hand-rolled TS codec
  (`cloister/src/wire/`, cross-checked against the same values in
  `cloister/test/wire/cross-check.test.ts`) and cloister's Go bindings.
  The reference vectors here are byte-equal to cloister's committed
  `test/wire/fixtures/canonical.ts` — verified at capture time and
  enforced live by the skip-if-missing cross-repo test.

## Version policy

Per `schema-spec` LAYOUT.md discipline:

- **Errata (in-place):** prose fixes that change no vector byte.
- **Minor (new `v` dir):** additive fields at fresh ordinals, new union
  variants at higher ordinals — old decoders keep working.
- **Major (new `v` dir):** anything that changes any pinned vector byte.

Never rename or renumber an ordinal; retire fields by leaving the
ordinal in place and stopping population (capnp evolution rules, quoted
in the `net.capnp` header).
