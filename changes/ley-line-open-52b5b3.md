Section: Added

- **The connection broker: the `leyline-conn-broker` binary and
  `leyline_runtime::broker`** (bead `ley-line-open-52b5b3`, ADR-0046 Slice 1 S4, S5).
  One broker per run listens on each declared UNIX endpoint and joins every accepted
  connection to that connection's one committed loopback target. It copies bytes with
  every descriptor a client passes closed on arrival: it reads with `recvmsg(2)`,
  because macOS installs an `SCM_RIGHTS` descriptor even on a plain `read(2)`. It has no parser, no
  resolver and no credential.
  - **Peers:** it accepts only the daemon's uid, and on the microVM tier only the run's
    worker pid. Any other peer is closed and counted as foreign.
  - **Hold and cap:** an `ECONNREFUSED` upstream is held with 5–200 ms backoff for at
    most 10 s, ending early when the client hangs up. The run's cap is 256 simultaneous
    connections.
  - **Bind, then confine:** it binds every endpoint, then confines itself on macOS with
    nono's base profile plus denies (exec, fork, all file reads and writes, the
    `mach-lookup`), so name resolution fails, with outbound TCP only to the declared
    ports. The mDNSResponder socket stays reachable, because nono emits its DNS allow
    after every platform rule (`ley-line-open-fb5c4b`).
    Then it reports ready with the `connectionsDigest` it bound, which the supervisor
    compares with the grant's.
  - **Lifecycle:** it exits the moment its lifeline pipe ends, and drains open
    connections into its records on request.
  - **Supporting pieces:** the endpoint preflight (path absent, parent a private,
    non-symlinked directory owned by the daemon, `sun_path` fits), the endpoint registry
    that refuses whichever of two overlapping runs registers second, the delivered-target
    check (`127.0.0.1` only, others refused as "loopback address not delivered by this
    runtime") and receipt aggregation.
  - **Not wired up yet:** nothing spawns the broker until the service integration
    (`ley-line-open-5308b7`), so grants with connections are still refused at start.
