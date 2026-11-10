Section: Added

- **The Go schema module refuses a head digest v2 Head by version** (bead
  `ley-line-open-a46711`). `sigma.ClaimOf(head.Head)` reads a Head's claim as
  LLO's `head_claim::claim_of` does and returns `sigma.ErrRetiredDigestV2`
  (`errors.Is`) for a Head carrying `retiredTreeRoot @7`. Before, a Go
  consumer had no refusal and read such a head as unstamped. A shared vector,
  `sigma/v1` `headMessage` (a v3 Head and the same Head with `@7` set, as
  Cap'n Proto messages), is checked by the Rust tests (`RetiredDigestV2` from
  `claim_of`, `stamp_of` and `digest_of`) and by the Go module
  (`task schema:go-sigma-vectors`).
