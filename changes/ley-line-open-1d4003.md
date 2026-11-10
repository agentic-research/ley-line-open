Section: Fixed

- **A control block has one daemon** (bead `ley-line-open-1d4003`). Two
  daemons on one `--control` with different `--arena`s were both admitted:
  each took the control lease (`<ctrl>.lock`) shared, the second published
  into the arena the block names while holding only its own arena's lock,
  and its socket bind replaced the first daemon's `<ctrl>.sock`. Every
  daemon now also holds `<ctrl>.daemon.lock` exclusively for its lifetime,
  and a second daemon on the block is refused ("another daemon serves the
  control block …") before it touches the block or the socket. The lease
  stays shared, so a long-lived reader holding `<ctrl>.lock` `LOCK_SH`
  (mache's mapper) still runs beside the daemon, and `--reset-arena` still
  refuses while any reader or daemon holds it.
