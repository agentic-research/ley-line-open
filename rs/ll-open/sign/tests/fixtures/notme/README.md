# Canonical-encoding fixtures (ADR-010 cross-implementation gate)

These files are the shared source of truth for the CABundle canonical
encoding — CBOR canonical form (RFC 8949 §4.2), integer-keyed map
`1=Epoch, 2=Seqno, 3=Keys, 4=KeyID, 5=PrevKeyID, 6=IssuedAt`, signature
field excluded from the signing input.

Three implementations must produce byte-identical output
(divergence silently breaks signature verification across them):

- **notme** (TypeScript): `worker/src/revocation.ts` `bundleCanonical` —
  gated by `worker/src/__tests__/bundle-canonical.test.ts`, which loads
  these files (notme-e9d2b8).
- **signet** (Go): `pkg/revocation/canonical.go` — previously asserted a
  hand-copied hex literal in `canonical_test.go`; should consume these
  files instead.
- **ley-line-open** (Rust): `rs/ll-open/sign` kid derivation — same.

## File format

Per fixture `<name>`:

| File | Contents |
|---|---|
| `<name>.json` | Input bundle: `epoch`, `seqno`, `keys` (map of kid → standard-base64 key bytes), `keyId`, `prevKeyId`, `issuedAt`. No `signature` — it is excluded from the signing input; tests supply a decoy to prove the exclusion. |
| `<name>.expected.hex` | Expected canonical bytes, lowercase hex, one line. |
| `<name>.bin` | The same bytes, raw. |

`cabundle-multikey.json` lists its `keys` in NON-canonical order on
purpose — a consumer can only pass by sorting (RFC 8949 §4.2:
length-then-bytewise, not alphabetical), never by insertion order.

## Adding a fixture

Add the three files, then extend the consuming test in each
implementation. Never edit `.expected.hex`/`.bin` to make a failing
implementation pass — the encoding is the contract; three-way agreement
is the point.
