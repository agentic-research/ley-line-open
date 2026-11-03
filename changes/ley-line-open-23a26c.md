Section: Fixed

- **A checkout no longer splits from `_tree` through path aliasing** (bead
  `ley-line-open-23a26c`). `_tree` paths are byte-distinct, but the default
  APFS volume folds case and Unicode normalization. Creating `Readme.md`
  beside `README.md`, or NFD `é.txt` beside NFC `é.txt`, was accepted as a
  second store entry for one file on disk: the directory kept the first
  file's bytes, `_tree` held both paths, and `_checkout_conflict` recorded a
  conflict that no edit could clear, across restarts. A daemon (or storefs
  mount) with a checkout now refuses such a create before it commits, with
  the new refusal reason `aliases_existing`, naming the created path
  (`path`) and the entry it aliases (`other`). Folding is probed on the
  checkout's directory, not assumed from the platform, and a case-only
  rename (delete and create in one proposal) still lands. ADR-0043 R3 and
  `docs/TABLE_CONTRACT.md` record the policy. The daemon wire gains the
  `RefusalReason` enumerant `aliasesExisting` and the
  `ProposalRefusal.other` field (also set for `path_conflict`); both are
  additive, the Go bindings are regenerated, and `SCHEMA_VERSION` moves to
  0.21.1.
