Section: Fixed

- **head.capnp describes `unboundFacts` as the producer counts it** (bead
  `ley-line-open-cc2130`). The schema said it counted `fact_edges` rows
  with a NULL `dst` under a "W5" ratchet gate; the producer counts
  `node_refs` rows whose token has no `node_defs` row, recorded as
  `_meta.unbound_facts`, and no such gate exists. The field's text now says
  that, and that the value depends on the extraction epoch (epoch 6 value
  refs raise it). `identity_docs_test` fails if the description names
  `fact_edges` or a W5 gate again, or if the producer's count or
  TABLE_CONTRACT's definition moves.
