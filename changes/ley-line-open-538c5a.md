Section: Changed

- **The documentation matches the v0.29.0 code** (bead `ley-line-open-538c5a`).
  `docs/ROADMAP.md` records what shipped in v0.28.0 and v0.29.0, and its
  "Now" is the follow-up gap queue (`ley-line-open-1262af`) and the next
  mache item (`ley-line-open-69305e`). `docs/ARCHITECTURE.md` gains the
  daemon lock (`<stem>.daemon-lock`), the read-only `query` module list, the
  `rootfs` and `--cas-volume` entry points and the microVM worker release
  asset; its ADR table says twelve libkrun patches and that ADR-0046 Q7 is
  implemented. `docs/TECHNICAL_OVERVIEW.md` no longer describes a public
  arena generation and names the execution boundary, the head digest and the
  rootfs digest. `docs/TABLE_CONTRACT.md` states the `node_edge` rule for
  comments as the code has it (the comment's parent writes none; a leaf row
  is shared by kind and text), names `_meta.projection_schema_version`, and
  corrects the `content_chunks` key, the `_checkout_index` columns and the
  Cap'n Proto fixture row. `GETTING-STARTED.md` no longer calls the release
  binary statically linked, names the `find_defs` op, and says which crates
  crates.io holds (0.14.0 of six). `rs/third_party/README.md` gives the
  guest init's size with all twelve patches (89,160 bytes, zig 0.15.2,
  aarch64). The `leyline-cli`, `leyline-cli-lib`, `leyline-schema`,
  `leyline-core` and `leyline-ts` READMEs describe the current subcommands,
  `projection-v13` and extraction epoch 7. `AGENTS.md` lists `task audit`
  and the quiet-host rule for `task release:hypervisor:record`. ADR-0044,
  ADR-0046 and ADR-0047 carry their current status. One specification text
  fix: `confinement.schema.json` cited §9 condition 5 for the `bind`
  refusal, and the README's condition is 6; a consumer that vendors the
  schema file byte for byte re-syncs it. Two code comments are corrected
  (`leyline-schema` `FILE_KEYED_TABLES`, `daemon::mcp`); no behavior moves.
