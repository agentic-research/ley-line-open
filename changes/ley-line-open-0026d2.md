Section: Security

- **Under `--tsi-hijack-inet`, a guest UDP socket no longer receives
  datagrams from anyone who finds its host port** (bead
  `ley-line-open-0026d2`, LLO libkrun patch `0009`). libkrun's TSI datagram
  proxy never read the port map. A guest `sendto` made the VMM bind a host
  UDP socket at `0.0.0.0` and an ephemeral port, and it forwarded every
  datagram that arrived there to the guest, from any source on any host
  interface. That held even when the VMM's sandbox had refused to transmit
  the guest's own datagram, which on macOS it always does (a §4 grant opens
  outbound TCP to the declared ports only). Measured on a live guest under a
  §4 grant: datagrams from loopback and from the host's LAN address reached
  the guest. With a port map configured (the worker always configures one),
  that socket now forwards only datagrams from a peer the guest named, a
  `sendto` destination or its `connect` peer; anything else is dropped in
  the VMM. A guest UDP bind to a fixed port was already unpublished, on
  loopback and on the LAN address. `libkrun_guest_listener`'s
  `an_undeclared_guest_udp_port_is_not_reachable_from_the_host` proves both
  in one run, beside a declared TCP listener that answers. The guarantee
  belongs to the `leyline-krun-worker` LLO builds; a stock libkrun does not
  carry the patch.
