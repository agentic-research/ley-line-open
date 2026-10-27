# leyline-sign

Signing for the Σ substrate: the at-rest `Head` root signer/verifier, plus CMS
primitives and a gpgsm-compatible binary for jj commit signing.

## Signed Σ `Head` (workstreams S1 / S2 / S3)

- **`Ed25519RootSigner`** (`root_signer`, feature `root-signer`) — the one
  concrete `RootSigner` over Σ roots. Signs the canonical head digest from
  `leyline_core::head_digest`, which binds `(generation, rootHash, parentHash)`
  — never `rootHash` alone, so a signature cannot be replayed at another
  generation or grafted onto a forked chain. **S1.**
- **`verify_head`** — verify-on-load. Returns a three-way verdict
  (`Valid` / `Invalid` / `Unsigned`): a present-but-invalid signature is refused
  under every policy; an absent one is left to caller policy so existing
  unsigned arenas keep working. **S2.**
- **`kid`** (`canonical_kid`, `is_canonical_kid_shape`; not feature-gated, so a
  wasm verifier can use it) — the substrate-canonical key identifier
  `lowercasehex(SHA-256(canonical SPKI)[:16])` ratified in signet ADR-012,
  gated against a pinned cross-language vector. `kid` selects a key; it never
  confers authority (`verify_head` still checks the signature against every
  trusted key — parity, not lookup). **S3.**

The signing/verification wiring in the CLI is opt-in via `LEYLINE_HEAD_SIGNING_KEY`
(sign) and `LEYLINE_HEAD_TRUSTED_KEYS` (verify), off by default.

## Ephemeral certificate verification (`cert_chain`)

- **`verify_cert_chain` / `verify_cert_path`** (wasm exports
  `leyline_verify_cert_chain`, `leyline_verify_cert_path`) — verify an
  Ed25519 end-entity certificate, directly under a trust-anchor public key
  or through Issuing CA intermediates, and return its claims as `CertClaims`
  (JSON over the FFI). Paths enforce issuer/subject name chaining,
  BasicConstraints, KeyUsage, pathLenConstraint and one epoch across the
  path.
- **Two profiles on `1.3.6.1.4.1.99999.1.*`.** cloister's Interlace profile
  and the notme / signet profile give `.1.5` and `.1.6` different meanings.
  A certificate carrying any of `.1.1`, `.1.2`, `.1.3`, `.1.8`, `.1.9` is
  read as the Signet profile (`profile = Signet`, JSON `"pr":"signet"`),
  and its `.1.5` / `.1.6` are never reported as `peer_fp` / `scope`.
  Everything else is the Interlace profile, whose claims are unchanged. See
  the `cert_chain` module documentation (bead `ley-line-open-9ac66d`).
- **Binding an anchor to its profile.** The profile is read from bytes the
  issuer signed. A caller that knows which profile an anchor issues passes
  it (`verify_cert_path_expecting`, or `expected_profile` on
  `leyline_verify_cert_path`: `0` either, `1` Interlace, `2` Signet), so a
  trusted issuer cannot present the other profile's claims.

## CMS + jj commit signing

- **Certificate** — Ed25519 self-signed X.509 certificate generation.
- **Signature** — CMS (RFC 5652) `SignedData` creation and verification.
- **`leyline-sign` binary** — drop-in replacement for gpgsm. Accepts `--sign`/`--verify` on stdin/stdout, compatible with jj's signing interface.

## Usage with jj

```bash
jj config set --user signing.backend "gpg"
jj config set --user signing.backends.gpg.program "leyline-sign"
jj config set --user signing.sign-all true
```
