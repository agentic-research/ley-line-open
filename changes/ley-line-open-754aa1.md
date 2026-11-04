Section: Added

- **`leyline-cache`: the restriction-addressed review cache, restored with a
  lawful key** (bead `ley-line-open-754aa1`, ADR-0031 amendment). The
  restriction cache removed from `leyline-sheaf` in `ley-line-open-afa952`
  (#521) is reverted and moved to a new crate, `rs/ll-open/cache`, that
  depends on `leyline-core` and `blake3` only. Its key is now a
  `PartitionSpec` `RowSet` fold over the review's input-closure rows: each
  row is a `derive_key("leyline/restriction/v1")` address over
  length-prefixed fields, the row kind is the entry's framing, the family
  and resolution rule are the scheme
  (`leyline/restriction/call-target/token-eq/v1`,
  `leyline/restriction/public-api/token-eq/v1`), and `parserId` is the
  params. The old key joined fields with U+001F and rows with a newline,
  both of which POSIX paths can hold, and bound no `parserId` (finding
  `80f037` F5); a proptest property shows distinct row sets get distinct
  keys under the new construction and is falsified on a test-only copy of
  the old one. `FactSubstrate::from_rows` takes the `parserId` the rows were
  extracted under, and restriction keys and `whole_object_hash` return a
  `Hash`. `leyline_cache::strategy` adds one `ReviewCache` under pluggable
  `KeyStrategy`s (`NoCache`, `WholeInput`, `RestrictionKey`), the interface
  the daemon wiring (`ley-line-open-69305e`) calls. `task cache:ablation`
  replays recent git history through those arms and through `SheafCache` in
  heuristic and δ⁰ modes; on 60 commits the restriction key hits 94.0% of
  10,464 lookups with 0 false skips, the whole-input key 0%, and
  `SheafCache` 68.1% with 154 false skips. A closure-mutation test drops
  each closure row in turn and shows the cache going unsound, which is the
  evidence the closure is complete. This reverts the restriction-cache
  entry of the `afa952` fragment.
