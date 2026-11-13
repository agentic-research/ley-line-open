Section: Added

- **Deployment configuration documents, `config.leyline/v1` (ADR-0049)**
  (bead `ley-line-open-52c177`). `rs/ll-core/schema-spec/deployment-config/v1/`
  specifies one versioned document per daemon kind (`DaemonConfig`,
  `ExecutionDaemonConfig`, `SignHelperConfig`): JSON Schemas with every
  default stated, a TOML adapter onto the same data model, canonical
  resolved bytes and their `configDigest` (BLAKE3-256; a fingerprint of one
  resolved configuration, never an authority), secrets by `{"file": …}`
  reference only (`keychain` and `vault` reserved), and the published list
  of removed environment variables (`leyline-removed-variables.json`).
  Vectors pin every accept case's bytes and digest and every refusal's field
  and reason; an independent standard-library Python reference reaches the
  same bytes. `leyline_schema_spec::deployment_config` is the parser,
  resolver and file rules (no-follow `openat` walk of the realpath'd parent);
  `leyline_runtime::deployment::resolve_execution_deployment` maps an
  execution document into the exact `NativeWorkerConfig` or
  `KrunWorkerConfig` the execution daemon builds, for embedders such as
  Cloister's host runtime, and `check_removed_environment` gives them the
  binaries' removed-variable refusal. The daemons adopt the documents in a
  following change.
