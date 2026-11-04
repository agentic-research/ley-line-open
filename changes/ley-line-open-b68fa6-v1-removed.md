Section: Breaking

- **Head digest v1 is removed; heads signed by an earlier release must be
  re-signed** (bead `ley-line-open-b68fa6`). A v1 signature does not verify
  under v2, so with `LEYLINE_HEAD_TRUSTED_KEYS` set a parse refuses to chain
  onto a head signed by v0.23.0 or earlier, and `leyline verify-head` refuses
  it. Delete `${db}.head.capnp` and reparse with `LEYLINE_HEAD_SIGNING_KEY`
  set to write a v2-signed head. Unsigned heads still chain. Rolling back
  fails closed: an older binary with a trust set refuses a v2-signed head,
  because it recomputes the v1 digest. `leyline verify-head` with a trust set
  now refuses an unsigned head, and its success line gains
  `signature=<verified|not-checked (...)>`. In `leyline-core`,
  `head_digest(generation, root, parent)` is now `head_digest(&HeadClaim)` and
  `HEAD_DIGEST_CONTEXT` is `"leyline head digest v2"`; in `leyline-sign`,
  `verify_head(generation, root, parent, signature, kid, trusted)` is now
  `verify_head(Option<&HeadClaim>, signature, kid, trusted)`. `Head` gains the
  additive field `parentDigest @9` (Go bindings regenerated).
