Section: Fixed

- **`--reset-arena` recovers a control block written by another VERSION**
  (bead `ley-line-open-feabf4`). After the VERSION 2 → 3 change, a daemon on
  a v2 `.ctrl` was refused even with `--reset-arena`, so mache's auto-start
  failed until `~/.mache/default.ctrl` was moved aside by hand. A normal
  start still refuses the mismatch, and the message now names the recovery.
  With `--reset-arena`, the daemon reinitialises the block at the current
  VERSION and logs the old one, but only when no other process holds it:
  every running daemon now holds `<ctrl>.lock` shared for its lifetime, the
  reset needs it exclusively (and keeps it), the `.ctrl` itself must be
  unlocked, and no daemon may still hold the lock of the arena the block
  names. Any holder refuses the reset, by name. The new block replaces the
  old one by rename, so a process still mapping the old block keeps its old
  bytes rather than misreading the new seqlock. The same lease refuses a
  `--reset-arena` against a control block a live daemon serves.
