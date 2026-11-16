Section: Fixed

- **`leyline daemon --lifeline-fd`: a teardown write cannot meet a dead launcher's pipe** (bead `ley-line-open-f6b525`).
  When the launcher's lifeline has ended, the daemon's output relay now
  finishes onto `/dev/null` whether or not a write had already found the
  launcher's reader gone. Before, a reader that died with nothing written
  through the relay since (or a forward still in flight) left `finish` handing
  the dead pipe back, and a later teardown write failed with EPIPE, turning
  exit 0 into 101. Any other stop still hands the original descriptors back.
