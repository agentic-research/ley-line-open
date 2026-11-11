# `RunGrant` — canonical bytes and issuer signature

`RunGrant` is the only signed surface in `cloister/execution/v1`. This file is
its canonical-bytes specification: what an issuer signs, what a verifier
recomputes, and why the two cannot drift.

## What the signature is for

Every other check in `authorize` reads a field the grant chose for itself.
`capabilities`, `limits`, `backendClass`, `confinementDigest`,
`expiresAtUnixMs`, `allowedEgress` and the resolved `workspaces` are the
resolved authority — and without a signature nothing ties any of them to an
issuer. A caller who can reach `llo_execution_start` could otherwise assemble
a grant naming whatever authority it wanted.

`issuerEvidence` does not close this on its own: it is a digest the grant
itself chooses, so it binds the grant to a run (see README.md §Evidence
binding) but not the grant's *fields* to an issuer.

## Covered bytes

```
signing_bytes = canonical(RunGrant with `signature` cleared)
signature     = Ed25519(PAE("application/vnd.cloister.execution.run-grant+capnp",
                            signing_bytes))
```

`canonical(...)` is Cap'n Proto canonicalization — the encoding-independent
form, with trailing zero words truncated in both the data and pointer
sections. `PAE` is DSSE's pre-authentication encoding,
`"DSSEv1" SP LEN(type) SP type SP LEN(body) SP body`, reused here for domain
separation: a `RunGrant` signature can never be replayed as a signature over
any other payload in the substrate, and vice versa.

Two properties make this the signable form:

- **Canonical**, so a signature survives re-framing. Segment layout and
  padding are an encoder's choice, not content — the same reason
  `runSpecDigest` is over the canonical form rather than the received wire
  bytes.
- **Signature-cleared**, so signing and verifying agree without a second
  encoding. An issuer computes these bytes from the grant it is about to
  sign; a verifier recomputes them from the grant it received, signature
  and all, and gets the same answer.

"Cleared" means **set to an empty `GrantSignature` struct**, not omitted: the
`signature` pointer is non-null and points at a zero-size struct, whether or
not the grant as received carried a signature. Canonicalization keeps that
pointer, so `signing_bytes` are 24 bytes longer than the canonical form of
the same grant with `signature` left unset (for the vector's first case, 1056
bytes against 1032), and a signer that omits the field instead produces a
signature every verifier refuses. An earlier revision of this document said
the cleared field canonicalizes to a truncated null pointer; that was wrong,
and the bytes the implementation signs have not changed. In JSON-carrier
terms the grant an issuer canonicalizes carries `"signature": {}`.

List presence is signed. An unset list field canonicalizes to a null pointer;
an explicit zero-length list canonicalizes to a non-null pointer, so the two
are different `signing_bytes`. Where a field's absence carries meaning, the
contract fixes one encoding: `WorkspaceGrant.writeScope` is left unset for "no
scope", and an authorizer refuses the empty list (README, "Workspace write
scope"). `test-vectors/run-grant-signing.json` gives known answers for the
signing bytes and the Ed25519 signature over them, including that pair.

## Fields

```capnp
struct GrantSignature {
  algorithm @0 :Text;   # "ed25519" — the only value v1 defines
  keyId     @1 :Text;   # unauthenticated hint
  value     @2 :Data;   # 64 raw signature bytes
}
```

`keyId` is an **unauthenticated hint**. It is inside the signed bytes of a
*different* grant only, never of this one — it is cleared along with the rest
of `signature` — so a verifier MUST try every key in its trust set and MUST
NOT select a key by `keyId`. This is the same parity-not-lookup rule DSSE's
`keyid` follows and that signet ADR-012 R1 requires.

A verifier MUST reject:

- an absent `signature`, unless the caller *is* the issuer (in-process
  embedding, where no wire crosses a trust boundary);
- an `algorithm` other than `ed25519`;
- a `value` whose length is not 64;
- a signature that verifies under no trusted key.

## Reference implementation

`leyline_runtime::authorization`:

- `grant_signing_bytes(grant_bytes) -> Vec<u8>` — the covered bytes.
- `GRANT_SIGNATURE_PAYLOAD_TYPE` — the PAE payload type above.
- `EvidenceVerifier::verify_grant(&SignedGrant)` — the embedder-owned check.
  The substrate computes `signing_bytes` and hands them to the verifier
  rather than handing over the grant, so a verifier cannot disagree with the
  substrate about what was signed.

An issuer signs with
`leyline_envelope::sign_payload(GRANT_SIGNATURE_PAYLOAD_TYPE, &signing_bytes, signer)`.

## Non-goals

Key distribution and rotation are Signet/NotMe's. This contract says only
which bytes a signature covers and how a verifier checks it; where the trust
set comes from is the embedding authority's decision.
