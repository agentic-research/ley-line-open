Section: Fixed

- **`tools/guest_tools.sh` builds again, and a rotted pin no longer breaks
  it silently** (bead `ley-line-open-ae6b99`). Alpine deleted
  `libexpat-2.8.5-r0`; the pin moves to `libexpat-2.10.0-r0` (sha256
  `99144fe0…`). Every pin is now fetched by `tools/fetch_pinned.sh`, which
  tries, in order, this machine's content-addressed store
  (`~/.cache/leyline/pins/<sha256>`, filled with each pin the script has
  verified), Alpine, and LLO's content-addressed mirror (the
  `guest-tools-pins` release, assets named by SHA-256). Every source is
  checked against the same pin. When none has the bytes, the script exits
  naming the pin and what each source answered, instead of curl's bare
  `404`. The pins live in `tools/guest_tools_pins.sh`;
  `tools/guest_tools_mirror.sh upload` adds a pin to the mirror and
  `task guest-tools:mirror:check` confirms they are all there.
  `task guest-tools:fixture-test`, run by `task ci`, proves the fallback and
  the failure message without the network.
