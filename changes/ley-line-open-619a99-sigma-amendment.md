Section: Added

- **ADR-0047: the Σ amendment** (bead `ley-line-open-619a99`). σ is indexed by
  construction: one value has one address per construction, each construction
  other than plain BLAKE3 over bytes is keyed, and addresses from different
  constructions are never compared. The ADR adds `verify(Ref, Witness)`, four
  edge kinds (Derives, Contains, Attests, Locates), a conformance table of every
  shipped identity, the never-filed axiom falsification suite, and four
  accepted identity migrations (keyed `node_hash` and `run_id`, a parser-free
  Merkle `storeRoot` replacing `treeRoot`, `lang`/`raw_kind` per occurrence,
  blob address unchanged under chunks-primary). Documentation only; no
  identity moves in this release.
