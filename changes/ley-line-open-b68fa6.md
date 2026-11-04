Section: Security

- **A signed `Head` now attests its `treeRoot`, `parserId` and the previous
  head's claim** (bead `ley-line-open-b68fa6`). The head digest was
  `BLAKE3_derive_key("leyline head digest v1", generation ‖ rootHash ‖ parentHash)`,
  so a signed head whose `treeRoot` or `parserId` was rewritten after signing
  verified as `Valid`, although ADR-0040 names `treeRoot` as the root that
  signatures cite. The digest is now v2,
  `BLAKE3_derive_key("leyline head digest v2", generation ‖ rootHash ‖ parentHash ‖ treeRoot ‖ parserId ‖ P)`,
  where `P` is `0x00`, or `0x01 ‖ parentDigest` when the new
  `Head.parentDigest` field names the previous head's v2 digest. `parentHash`
  alone names only the previous run's segments, which a no-op rerun makes
  equal across arenas. The digest is computed over a `HeadClaim` that cannot
  be built without a stamped `treeRoot` and `parserId`: a head without them is
  refused rather than signed, and a signature on one verifies as `Invalid`.
  `leyline verify-head` checks the signature before it compares the stamped
  `treeRoot` with the arena. With `LEYLINE_HEAD_TRUSTED_KEYS` set it refuses an
  unsigned head, so stripping the signature and rewriting the head and arena
  together no longer passes. With no trust set it refuses when
  `LEYLINE_HEAD_REQUIRE_SIGNATURE` is set, and otherwise prints
  `signature=not-checked`; its success line now always names the signature
  verdict. `manifestRoot`, `hierarchyId` and `logicalRoot` do not exist yet and
  are not bound.
