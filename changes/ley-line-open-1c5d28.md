Section: Breaking

- **Sheaf stalk hashes are parsed strictly, an omitted hash is an explicit
  "changed", and region hashes have a content-derived contract** (bead
  `ley-line-open-1c5d28`).
  - Breaking: `sheaf_set_topology`, `sheaf_update_topology` and
    `sheaf_invalidate` refuse a `hash` or `boundary_hash` that is not exactly
    64 lowercase hex digits, `""` included, with a typed `WireHashError`,
    and apply none of the request. The old parser zero-padded short input,
    truncated long input and skipped non-hex pairs, so `""`, `"aa"` and
    `"zz"` all became hashes. `boundary_hash` is now required.
  - Fixed: an omitted `hash` (or `null`) marks the region
    `StalkDigest::Changed`. Every boundary it touches fires before the XOR
    pre-filter or δ⁰ is consulted, so the cascade proceeds through it and
    nothing under-evicts. It used to become the all-zero hash, which fired
    the XOR only by accident. mache's current omit-the-hash `Invalidate`
    keeps its eviction behavior; in δ⁰ mode it now also fires the region's
    edges, since its stalk data was not refreshed either.
  - Added: the region-hash contract, `leyline/sheaf-region/v1`: a region's
    hash is the `PartitionSpec` `RowSet` fold over its members' `node_hash`
    values with `parserId` as params, and an edge's `boundary_hash` is the
    XOR of its two regions' hashes. Rust: `leyline_sheaf::region::{
    region_hash, boundary_hash, REGION_SCHEME}`; vectors with full
    preimages: `clients/go/leyline-schema/vectors/sheaf-region-v1.json`.
    Documented in `daemon.capnp` and the sheaf README.
  - `leyline_sheaf::cache::StalkHash` now has one method,
    `digest() -> StalkDigest` (`Hash([u8; 32])` or `Changed`), replacing
    `merkle_root() -> [u8; 32]`; `StalkDigest` implements it, and the
    daemon's `HashStalk` is gone (`SheafCache<StalkDigest, ()>`).
