Section: Added

- **`RunGrant` can carry a run's connections: `connectionsDigest @16`,
  `connectionsDocument @17` and the `credentialBrokerRefs` marker
  `cloister/connections/v1`** (bead `ley-line-open-521deb`, ADR-0046 Slice 1 S3).
  Authorization accepts the three only together. It recomputes the BLAKE3-256 digest
  from the document and refuses a document that is not byte-equal to its canonical
  form. Any other combination, or a present but empty field, is `invalidGrant`
  naming what is missing. `credentialBrokerRefs` now means "the broker integrations
  the run requires"; any entry but exactly the marker keeps the refusal every daemon
  has returned since execution/v1 shipped. Daemons that predate the fields refuse
  such a grant: on its signature when they verify, and on the marker when they do
  not. This daemon has no connection broker yet, so it refuses a grant with
  connections at start with `unsupportedBackend`, before the resolver runs. It does
  not advertise `cloister/execution/v1/connections`; that arrives with the broker
  (`ley-line-open-52b5b3`). `RunReceipt` gains `connectionsDigest @15` and
  `connections @16` (`ConnectionOutcome`). Nothing writes them yet, and a receipt
  from a run without connections is byte-identical to before. New pinned vector:
  `execution/v1/test-vectors/connections-grant.json`, the canonical and signing bytes
  produced by the capnp CLI and matched by `grant_signing_bytes`.
