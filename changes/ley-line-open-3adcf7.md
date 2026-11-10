Section: Security

- **`rs/Cargo.lock` carries no RustSec vulnerability, and `task ci` now
  fails on one** (bead `ley-line-open-3adcf7`). Semver-compatible lockfile
  updates clear the 23 vulnerabilities `cargo audit` reported: in the shipped
  binaries, rustls-webpki 0.103.16 and webpki-roots 1.0.11 stop trusting the
  distrusted emSign intermediates (RUSTSEC-2026-0382, -0381) and
  crossbeam-epoch moves to 0.9.21 (RUSTSEC-2026-0204); outside them, h2,
  pulldown-cmark, quinn-proto, rkyv, webpki-root-certs and zstd/zstd-safe/
  zstd-sys move to their fixed releases, and zip moves to 9.4 (bead
  `ley-line-open-643a35`). The unsound anyhow, faster-hex, lru, memmap2, tokio
  and zstd-safe releases and the yanked chacha20, der and async-executor
  releases are replaced too, and imago 0.2.6 drops the unmaintained bincode
  from `leyline-krun-worker`. A new `task audit` runs
  `cargo deny check advisories` over every feature of the workspace with the
  committed `deny.toml`, and `task ci` runs it. Four unmaintained-crate
  notices in not-shipping features remain ignored (paste, bead
  `ley-line-open-531fc5`; the zbus 4 stack, bead `ley-line-open-275db8`).
  `tools/lint_deny_ignores.sh` fails the audit when an ignore lacks a bead or
  an expiry date, is past its expiry, or names a crate that `leyline-cli` or
  `leyline-krun-worker` ships.
