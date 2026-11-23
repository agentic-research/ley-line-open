Section: Fixed

- **`leyline daemon --lifeline-fd`: a teardown write cannot meet a dead launcher's pipe** (bead `ley-line-open-f6b525`).
  When the launcher's lifeline has ended, the daemon's output relay now
  finishes onto `/dev/null` whether or not a write had already found the
  launcher's reader gone. Before, a reader that died with nothing written
  through the relay since (or a forward still in flight) left `finish` handing
  the dead pipe back, and a later teardown write failed with EPIPE, turning
  exit 0 into 101. On any stop, a descriptor also finishes onto `/dev/null`
  when a forward already found its reader gone or when the original reports
  its reader hung up (POLLHUP on macOS, POLLERR on Linux), so a SIGTERM whose
  launcher also died is covered. Otherwise the original goes back to a
  reader that is still there. A reader that dies after `finish` checks is not
  seen. `/dev/null` is opened when the relay is installed, so finishing has
  no failure path that falls back to the original.
