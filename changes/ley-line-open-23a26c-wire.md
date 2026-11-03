Section: Added

- **The propose wire gains the refusal reason `aliases_existing` and
  `ProposalRefusal.other`** (bead `ley-line-open-23a26c`). A daemon with a
  checkout refuses a create that its file system folds onto another entry.
  `refusal.reason` is `aliases_existing` (Cap'n Proto `RefusalReason`
  ordinal 17), `refusal.path` names the created path, and the new
  `refusal.other` names the entry it aliases. `other` is also set for
  `path_conflict`, where it names the file the created path is under. Propose
  clients (mache's write path among them) should map the new reason. A
  client that does not know it still reads `ok: false`: an unknown ordinal
  or reason string is a refusal, never a success
  (`clients/go/leyline-schema/daemon/aliases_existing_test.go`). A JSON
  decoder that rejects unknown fields fails on `other`, so it must allow it.
