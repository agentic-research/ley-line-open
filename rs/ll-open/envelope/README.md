# leyline-envelope

DSSE (Dead Simple Signing Envelope) + in-toto Statement v1 attestation, composed over `leyline-sign`'s root signer. One signing implementation for every runtime, wasm included.

Byte-compatible hoist of rosary's `src/dsse.rs` (bead `ley-line-open-319a08`): identical `(statement, key)` inputs produce identical payload and signature bytes, proven by a pinned vector in the test module. Consumers keep only *policy* — what to attest, when a key is configured — and hand the mechanism a `Statement` plus a signer.

## What's here

- **`Statement` / `UnsignedStatement` / `Subject`** — the in-toto Statement v1 shape.
- **`Envelope`** — the DSSE wire envelope (`payloadType`, base64url `payload`, `signatures[]`).
- **`pae`** — the DSSE Pre-Authentication Encoding function.
- **`sign_payload` / `verify_payload`** — sign/verify over an `Ed25519RootSigner` from `leyline-sign`.
- **`ParseError` / `VerifyError`** — typed failure modes for malformed envelopes and signature mismatches.

## Wire format

```json
{
  "payloadType": "application/vnd.in-toto+json",
  "payload": "<base64url(in-toto Statement JSON)>",
  "signatures": [{ "keyid": "<hint>", "sig": "<base64url(sig)>" }]
}
```

## Used by

- **`leyline-runtime`** — attests execution receipts (`RunReceiptData`) as signed in-toto statements.

## Correctness stance

The byte-compatibility claim with rosary's original `dsse.rs` is falsifiable, not asserted — the pinned golden-vector test is what makes a silent encoding drift between the two implementations impossible to ship unnoticed.
