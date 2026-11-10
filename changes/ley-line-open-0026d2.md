Section: Security

- **Under `--tsi-hijack-inet`, a guest UDP socket no longer gets a host
  port that anyone can send to** (bead `ley-line-open-0026d2`, LLO libkrun
  patch `0009`). libkrun's TSI datagram proxy never read the port map. A
  guest `sendto` made the VMM bind a host UDP socket at `0.0.0.0` and an
  ephemeral port before sending anything, and it forwarded every datagram
  that arrived there to the guest, from any source on any host interface.
  That held even though the VMM's sandbox refused to transmit the guest's
  own datagram, which on macOS it always does (a §4 grant opens outbound
  TCP to the declared ports only). Measured on a live guest under a §4
  grant: datagrams from loopback and from the host's LAN address reached
  the guest. With a port map configured (the worker always configures
  one), the proxy binds nothing for the guest, admits a peer only once the
  host has actually sent to it or connected to it, and forwards only
  datagrams from admitted peers; a destination the guest merely names
  admits nothing. Under the worker's profile the VMM now holds no UDP port
  at all. A guest UDP bind to a fixed port was already unpublished, on
  loopback and on the LAN address. `libkrun_guest_listener`'s
  `an_undeclared_guest_udp_port_is_not_reachable_from_the_host` proves both
  in one run, beside a declared TCP listener that answers. The guarantee
  belongs to the `leyline-krun-worker` LLO builds; a stock libkrun does not
  carry the patch.
