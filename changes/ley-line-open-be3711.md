Section: Added

- **Declared connections reach an unmodified microVM workload as TCP on the
  guest's loopback, in both directions, and nothing undeclared exists**
  (bead `ley-line-open-be3711`, ADR-0046 Q7, LLO libkrun patches `0010` and
  `0011`). confinement/v1 §6 grants gain an optional `guestPort`, additive
  within v1: `{"path": …, "mode": "connect", "guestPort": N}` lets the
  workload dial `127.0.0.1:N` and reach the peer at the socket, and
  `{"path": …, "mode": "bind", "guestPort": N}` lets the peer at the socket
  reach the workload's listener on `127.0.0.1:N`. The issuer chooses the
  port and `confinementDigest` commits to it; a document without
  `guestPort` digests as before (`test-vectors/manifest-guest-port.json`).
  The worker projects the attested document into the guest init's channel
  list, and the init binds every dial port before the workload starts and
  joins each connection to the grant's vsock port, copying bytes only.
  Refused by name: `guestPort` on the native tiers, on `connect-bind`,
  twice in one document, under `--tsi-hijack-inet`, and off macOS until the
  presentation runs live there. The guest init now also refuses every
  datagram, raw and packet socket in the guest (a seccomp filter every guest
  process inherits), so a guest cannot send UDP under any boundary. A run
  whose declared socket cannot exist (a path longer than `sun_path`, a
  missing directory, an unbound peer) is refused before readiness, naming
  the grant; libkrun used to start the VM and log the failure (bead
  `ley-line-open-bb2143`). A `publicEnvironment` entry containing a double
  quote is refused on the microVM tier: libkrun passes the environment on the
  guest kernel command line, where a quote ends the entry. Rust consumers of
  `leyline-runtime`: `UnixSocketGrant::Connect` is now a struct variant
  carrying `guest_port`; the constructors are unchanged.
