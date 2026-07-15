# leyline-net/v1 — frame canonical-bytes specification

Bead `ley-line-open-083344`. This is the per-surface canonical-bytes doc
for the leyline-net generic frames. Schema:
`rs/ll-core/schema-capnp/schemas/net.capnp` (`@0xa25bb2a310446125`).

## Structs

Field ordinals are the wire contract. Never renumber; retire by leaving
the ordinal in place.

### `Manifest`

| Ordinal | Field | Type | Constraint |
|---|---|---|---|
| @0 | `sequence` | `UInt64` | monotonic per-`publicKey`; receivers reject `≤ last-seen` |
| @1 | `publicKey` | `Data` | Ed25519 public key, exactly 32 bytes |
| @2 | `signature` | `Data` | Ed25519, exactly 64 bytes, over `sequence (LE 8 bytes) ‖ contentHash (32 bytes)` |
| @3 | `contentHash` | `Data` | SHA-256 of the AEAD **plaintext** (the serialized ToolCall/ToolResult before encryption), 32 bytes |

The signature covers the plaintext hash, NOT the AEAD ciphertext; the
AEAD authenticated-data binding (below) is what ties manifest to
ciphertext.

### `ToolCall`

| Ordinal | Field | Type | Notes |
|---|---|---|---|
| @0 | `upstreamId` | `Text` | logical backend name ("rosary", "mache", "leyline"); receiver-side config, not user-controlled |
| @1 | `toolName` | `Text` | MCP tool name |
| @2 | `argumentsJson` | `Data` | canonical JSON bytes, preserved verbatim so sender-side digests stay valid |

### `ToolResult` / `Content` / `BinaryContent`

| Struct | Ordinal | Field | Type |
|---|---|---|---|
| `ToolResult` | @0 | `content` | `List(Content)` |
| `ToolResult` | @1 | `isError` | `Bool` |
| `Content.body` (union) | @0 | `text` | `Text` |
| `Content.body` (union) | @1 | `binary` | `BinaryContent` |
| `Content.body` (union) | @2 | `resource` | `Data` (opaque, forwarded verbatim) |
| `BinaryContent` | @0 | `data` | `Data` |
| `BinaryContent` | @1 | `mimeType` | `Text` |

## Envelope

Where the transport crosses a real network boundary, the message body is:

```
[manifest length :2 bytes BE]
[manifest bytes  :variable]    -- serialized Manifest
[aead nonce      :12 bytes]    -- ChaCha20-Poly1305 nonce
[aead ciphertext :variable]    -- AEAD(payload)
```

AEAD binding rules:

1. The AEAD **authenticated data is the manifest bytes exactly as they
   appear in the frame** — a man-in-the-middle cannot swap a manifest
   onto a stale ciphertext.
2. The AEAD plaintext is a single serialized `ToolCall` (request) or
   `ToolResult` (response).
3. `Manifest.contentHash` = SHA-256(plaintext); receivers verify after
   decryption as defense-in-depth.
4. Nonce reuse under one key is forbidden (standard ChaCha20-Poly1305
   contract); sequence reuse per pubkey is rejected by receivers.

Intra-cluster amendment (cloister ADR-0005): transports whose trust
boundary is the host — cloister↔companion loopback HTTP, rosary's
`rsry mcp --ipc-socket` UDS — exchange **plain serialized
`ToolCall`/`ToolResult` messages with no Manifest envelope and no
AEAD**. The envelope is for network-crossing paths.

## Canonical encoding contract

Per capnproto.org/encoding.html#canonicalization, consumers require:

- **Single segment** per message (multi-segment is rejected by
  cloister's hand-rolled TS decoder).
- **Unpacked** binary encoding — no packed zero-elision.
- **Composite-list size code 7** for `List(struct)`.
- Stream framing above the capnp layer (the envelope, HTTP body, or
  capnp segment-table stream framing on UDS).

### The two byte-forms (both pinned)

Empirical finding (capnp =0.25.0, recorded on bead
`ley-line-open-083344`): two byte-forms of the same value circulate,
differing only in trailing-zero truncation:

- **reference** (`test-vectors/reference/`): output of plain
  `capnp::serialize::write_message` on a freshly built single-segment
  message, byte-equal to `capnp eval -b` and to cloister's committed
  `test/wire/fixtures/canonical.ts`. Sections keep their declared sizes
  (e.g. a trailing null `argumentsJson` pointer word is present).
- **canonical** (`test-vectors/canonical/`): strict canonical form via
  `set_root_canonical`, which truncates trailing zero words in data and
  pointer sections (9 of the 12 vectors shrink; e.g. `tool-call-empty`
  56B → 48B).

Encoders MAY emit either form (freshly built single-segment messages
are the reference form; canonicalizing writers produce the canonical
form). Decoders MUST accept both — they are value-equal, and the
decode-direction tests enforce this for every vector.
