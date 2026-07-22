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
