Section: Added

- **The Σ axiom falsification suite and known-answer vectors for every shipped
  construction** (bead `ley-line-open-10d71a`, ADR-0047 step M0). Each axiom
  of the Σ decade is a test named as ADR-0047 §5 names it, in the crate that
  owns the construction: (CA) round trip and tamper refusal over
  `MemBlobStore`, `FsBlobStore`, the chunk pool, blob-manifest reconstruction
  and `source_blobs`; (DET) one checked-in vector per construction in
  `schema-spec/sigma/v1/test-vectors/constructions.json`, with pinned
  preimages, plus a parse on one and on eight rayon threads that names the same
  identities; (IM) a write-sequence property over every address-keyed table and
  a writer SIGKILLed at four deterministic points of a publish; §1.3 Merkle cost
  for `treeRoot` and `node_hash`; and the construction-separation laws. Two
  claims fail on shipped code and stay visible: `treeRoot` is one flat fold (an
  edit refolds 100,000 entries in a 50,000-file store; turned green by M1,
  `ley-line-open-73c6ee`) and `node_hash`/`run_id` put their tag inside a plain
  σ preimage (a file of a leaf's preimage has that leaf's address; M2/M3,
  `ley-line-open-73add7`). Each has an ignored strict test and a measured test
  that runs in CI.
- **`Ref<D>`: construction-indexed addresses in `leyline-core`**
  (`leyline_core::construction`). A `Ref<D>` is `repr(transparent)` over `Hash`,
  minted only by `D`'s fold (`Ref::derive`) or by re-deriving a claimed address
  from a witness (`Ref::verified`); there is no `from_bytes`, no `ZERO` and no
  conversion between constructions, and comparing refs of two constructions
  does not compile. Constructions: `Blob`, `Chunk`, `ArenaImage`, `HeadDigest`
  (core), `Parser`, `TreeRoot`, `AstNode` (cli-lib), `Restriction` (cache).
  `leyline verify-head` and `leyline rebuild` now type the stamps they check.
  Additive: no identity value, schema, projection or wire type changes, and the
  vectors were pinned before the types landed.
