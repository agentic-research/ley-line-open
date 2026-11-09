Section: Breaking

- **The store identity is parser-free: `treeRoot` is removed and `storeRoot`
  replaces it (ADR-0047 M1, bead `ley-line-open-73c6ee`).** `treeRoot` folded
  `parserId` into its params, so a grammar, epoch or projection bump renamed
  an unchanged store. `storeRoot` is a directory Merkle tree
  (`leyline/store-tree/v1`): one keyed `RowSet` fold per directory, params
  empty, children in name-byte order as `(σ(name), 2i, |name|)` and
  `(child, 2i + 1, git mode)`. One edit refolds only the directories on its
  path (240 entries on a 50,000-file store, where the flat fold refolded
  100,000). The parser stays bound beside it, never inside it:
  - **Head digest v3** (`leyline head digest v3`) binds `storeRoot` and
    `parserId` separately. `Head.storeRoot @10` is new; `Head.treeRoot @7` is
    renamed `retiredTreeRoot` and never written. A head that carries it is a
    v2 head and is refused by every reader with the typed
    `HeadFormatError::RetiredDigestV2`: `leyline verify-head` fails, and a
    parse does not chain onto it. Delete the `.head.capnp` and reparse to
    start a v3 chain. A v2 signature does not verify under v3.
  - **Daemon wire, `wire_format_major` 2:** `read_source` and `propose`
    answer `store_root` (`ReadSourceResponse.storeRoot @6`,
    `ProposeResponse.storeRoot @7`); `tree_root` is retired and never sent.
    The `leyline_version` handshake and `compatibility.json` report wire
    major 2, so a major-1 client refuses the daemon instead of reading the
    missing `tree_root` as "no root recorded". The Go `daemon/wire` mirror's
    `TreeRoot` fields are now `StoreRoot`; a consumer that decodes with
    `DisallowUnknownFields` refuses a pre-M1 `tree_root` response, and a
    lenient decoder drops it. The generated Go accessor `head.Head.TreeRoot`
    is now `RetiredTreeRoot`, and `StoreRoot` is new. The `no_tree_root`
    refusal reason is unchanged.
  - **Projection `projection-v11`.** `_meta.derived_from` is the versioned
    value `v2 <storeRoot> <parserId> <projection>`; `leyline rebuild` refuses
    the retired three-field v1 value, and a v10 arena is refused at parse
    open. `leyline verify-head` prints `valid storeRoot=...`, and it and
    `leyline rebuild` refuse a `_tree` whose directory rows are not closed
    under parents (bead `ley-line-open-835aad`), which would otherwise fold
    to the closed manifest's root.
  - **Rust API:** `leyline_cli_lib::tree_root` is `store_root`
    (`StoreLeaf`, `StoreTree`, `StoreRoot`, `store_root_of`, `store_root`);
    `ParseResult`, `Write` and `Accepted` carry `store_root`;
    `leyline-cache`'s `WholeInput.tree_root` is `store_root`.
  - **Grants:** a workspace grant's `graphRoot` is `blake3-256:<storeRoot>`,
    so a parser upgrade no longer invalidates it.
  - **Cross-runtime vectors:** `sigma/v1` replaces its `treeRoot` cases with
    `storeRoot` cases that pin every directory node, and its `headDigest`
    cases move to v3. The Go module gains package `sigma`, which reproduces
    the `PartitionSpec` fold, `storeRoot` and head digest v3 from those
    vectors.
