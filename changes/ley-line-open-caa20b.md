Section: Added

- **The release attaches `leyline-conn-broker-darwin-arm64`** (bead `ley-line-open-caa20b`; ADR-0046 Slice 1).
  A daemon serves a grant's connections only when started with
  `execution-daemon --connection-broker <path>`, and an embedder needs
  `BrokerConfig { binary }`; until now that binary came only from a source
  build. The release's darwin-arm64 leg builds it
  (`task release:conn-broker:target`), stages it with the linker's ad-hoc
  signature and no entitlement, as the `leyline` CLI ships, lists it in
  `SHA256SUMS`, and runs the staged copy's refusals
  (`task release:conn-broker:smoke`): a bad `--lifeline-fd`, a missing
  value, an unknown flag and a descriptor that is not open are each refused
  by name with exit status 2. Only a darwin broker ships, because the broker
  refuses before ready off macOS. The test-only
  `leyline-conn-broker-unconfined` is refused by name by staging, by
  per-target artifact verification and by public release verification, even
  when a manifest and the asset contract both list it.
