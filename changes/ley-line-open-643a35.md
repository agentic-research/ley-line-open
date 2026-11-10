Section: Security

- **The `engine-witchcraft` feature builds candle 0.11.1 and zip 9.4,
  past zip's extraction advisories** (bead `ley-line-open-643a35`).
  witchcraft pins candle at a git revision (0.10.2) that requires zip 7.2,
  which has RUSTSEC-2026-0367, -0368, -0369 and -0429 (SUID bits kept on
  extraction, symlinks written outside the destination, permissions applied
  through symlinks, and a process abort on a symlink entry). A
  `[patch."https://github.com/huggingface/candle"]` in `rs/Cargo.toml`
  serves witchcraft's candle crates from the crates.io release 0.11.1, which
  requires zip ^9.1.3. No release binary enables the feature. Remove the
  patch when witchcraft pins a candle whose zip is 9.0.5 or later.
