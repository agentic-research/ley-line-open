Section: Added

- **Go `daemon/wire` types for `read_source` and `propose`** (bead
  `ley-line-open-247489`). `clients/go/leyline-schema/daemon/wire` exports
  `ReadSourceResponse`, `ProposedWrite`, `ProposeResponse`,
  `ProposalRefusal` (with `other`), `StaleRefusal`, `PathHash` and
  `LineRange`, and the `WriteKind` and `RefusalReason` wire spellings as
  constants (`aliases_existing` included), so a Go client decodes the UDS's
  capnp-json without its own mirror. The daemon protocol fixtures decode
  strictly into these types, and a test ties the constants to every
  enumerant of the generated Cap'n Proto enums. A refused proposal carries
  `error` beside `refusal`: decide on `ok` and `refusal`, not on `error`.
