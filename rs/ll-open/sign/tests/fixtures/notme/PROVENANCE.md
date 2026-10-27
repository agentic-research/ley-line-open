# notme certificate fixtures

Real certificates minted by notme's own code, used by `tests/notme_chain.rs`
(bead `ley-line-open-9ac66d`). Nothing here is hand-built.

## Source

- Repository: `agentic-research/notme`
- Commit: `e6f0f79f3a28ba0593ee7e689dae9b62e1a9b95e` (branch `goalzero`,
  pushed; the Issuing CA tier is not on `main` at `4d84b70`). The next
  commit, `66ba481`, does not change any minted extension.
- Code: `worker/src/cert-authority.ts`, unmodified: `mintIssuingCaCert`,
  `mintTaskCertPair`, `mintBridgeCertPair`.

## Command

`mint.mts` (in this directory) was run from a scratch directory outside the
notme tree, using notme's installed `tsx` and `@peculiar/x509` read-only.
It writes only into the output directory it is given:

```
<notme>/node_modules/.bin/tsx mint.mts <notme> <out-dir>
```

Node v25.9.0, 2026-10-27. Keys are random per run, so a rerun produces
different bytes; `expected.json` records this run's public values.

## Files

| File | What |
|---|---|
| `root.pub` | Raw 32-byte Ed25519 public key of the root (notme's master key role). |
| `issuing-ca.der` | Issuing CA tier: CA=true, pathlen=0, keyCertSign, signed by the root. Epoch 3, authMethod `passkey`. |
| `task-signing.der` | Task leaf (Ed25519 signing cert), signed by the Issuing CA. Carries `.1.1`–`.1.6`, `.1.8`, `.1.9`, critical BasicConstraints/KeyUsage/SAN. |
| `task-mtls.der` | The P-256 mTLS half of the same task pair (not an Ed25519 leaf). |
| `direct-signing.der` | Root-signed Ed25519 signing cert from `mintBridgeCertPair` (no tier). |
| `expected.json` | Epoch, raw public keys (base64) and peer-binding digests from the mint run. |
