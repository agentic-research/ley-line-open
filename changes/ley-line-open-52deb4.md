Section: Added

- **The loopback probe, and a macOS CI job for the connection broker** (bead
  `ley-line-open-52deb4`, ADR-0046 Slice 1 S11a). `tests/broker_loopback_probe.rs`
  measures, for `127.0.0.1`, `127.0.0.2`, `127.255.255.254` and `::1` on each platform,
  four things:
  - whether the address is available (an unavailable row is never a pass);
  - whether a child confined by the broker profile for a grant naming that address
    reaches it on the allowed port;
  - whether the profile for another live port on the same address refuses it (a
    wildcard fails the probe);
  - whether the profile for `127.0.0.1` on the same port reaches it, which shows
    whether the filter scopes by address.

  It asserts a pinned table per platform. Measured on macOS 27.2: `127.0.0.1` and `::1`
  are scoped-allowed with the port control passing; `127.0.0.2` and `127.255.255.254`
  are unavailable, since lo0 carries only `127.0.0.1`. Neither platform's filter scopes
  by address, so the broker's code alone holds the address. Widening the delivered set
  (`broker::DELIVERED_TARGETS`, still `127.0.0.1`) is a one-commit gate flip once a row
  passes on every delivering platform.

  New tasks: `task runtime:broker-macos` runs the probe and every broker suite with
  `LEYLINE_REQUIRE_SANDBOX=1`, locally and in a new `macos-latest` job in `ci.yml`;
  the job and the probe record the macOS version the table was observed on.
  `task runtime:broker-probe` runs the probe on Linux on the host network and again in
  a network namespace with IPv6 disabled, where the run must pass one test and record
  `::1` unavailable; it is a step in the ubuntu job and records the Landlock ABI.
