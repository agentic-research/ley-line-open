Section: Security

- **A signed `Head` now attests its `treeRoot` and `parserId`** (bead
  `ley-line-open-b68fa6`). The head digest was
  `BLAKE3_derive_key("leyline head digest v1", generation ‖ rootHash ‖ parentHash)`,
  so a signed head whose `treeRoot` or `parserId` was rewritten after signing
  verified as `Valid`, although ADR-0040 names `treeRoot` as the root that
  signatures cite. The digest is now v2,
  `BLAKE3_derive_key("leyline head digest v2", generation ‖ rootHash ‖ parentHash ‖ treeRoot ‖ parserId)`
  over a `HeadClaim` that cannot be built without a stamped `treeRoot` and
  `parserId`: a head without them is refused rather than signed, and a
  signature on one verifies as `Invalid`. `leyline verify-head` checks the
  signature when `LEYLINE_HEAD_TRUSTED_KEYS` is set, before it compares the
  stamped `treeRoot` with the arena, so an arena and head rewritten together
  no longer pass. `manifestRoot`, `hierarchyId` and `logicalRoot` do not exist
  yet and are not bound.
