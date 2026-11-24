Section: Breaking

- **The execution service serves a grant's connections: `Backend::start`
  takes the run's connections, and a daemon with a broker advertises
  `cloister/execution/v1/connections`** (bead `ley-line-open-5308b7`,
  ADR-0046 Slice 1 S2, S6, S7, S8). Start `execution-daemon` with
  `--connection-broker <path to leyline-conn-broker>` (an executable file,
  or the daemon refuses to start): a `RunGrant` carrying
  `connectionsDigest`, `connectionsDocument` and the `credentialBrokerRefs`
  marker then gets one broker for its run, and the workload reaches each
  declared loopback target through it (`127.0.0.1:<guestPort>` in a microVM,
  the endpoint path on the native tier; macOS only in this slice). Without
  the flag the daemon advertises no such capability and refuses the grant by
  name, as before. Before anything is spawned the run is checked and refused
  by connection name: a tier that does not deliver connections, a target
  other than `127.0.0.1`, an endpoint without exactly one confinement/v1 §6
  connect grant in the tier's spelling, a bind-capable §6 grant or a §2
  write grant that reaches an endpoint, an endpoint path that exists or
  whose directory is not private, and an endpoint another live run holds. A
  §6 connect grant with no connection stays legal. The broker is spawned,
  must report ready within 5 s with the document's digest, and only then is
  the workload started; any failure after the spawn kills it and unlinks its
  endpoints. `cancel` now reaches a run that is still starting. A broker
  that exits during a run fails the run and cancels its workload. At every
  terminal state the broker is drained first, so the receipt carries
  `connectionsDigest` and one `connections` outcome per declared connection
  (`complete` false only after a broker crash); a receipt for a run without
  connections is unchanged. **Breaking for embedders:** `Backend::start` has
  a third parameter, `RunConnections` (pass `RunConnections::none()`), and
  `RunOutcome` and `RunReceiptData` gained a `connections` field. An
  embedder with no daemon wraps its backend in `Brokered::new(backend,
  BrokerConfig::new(path))`, passes `RunConnections::verified(..)`, probes
  `Backend::connections()` (absent means refuse), and observes the run's end
  through the wrapper. Limits: the broker binary is not yet a release asset
  (`ley-line-open-caa20b`), and its path is a flag until the daemons adopt
  the deployment configuration document (`ley-line-open-52c177`).
