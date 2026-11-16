Section: Added

- **`leyline daemon --lifeline-fd N`: the daemon shuts down when its launcher dies** (bead `ley-line-open-f6b525`).
  A launcher that runs the daemon in its own process group (mache) had no way
  to stop it after a `SIGKILL`. With the flag, the launcher keeps the only write
  end of a pipe and passes the read end as descriptor N; when the launcher is
  gone, the daemon runs the same graceful shutdown as SIGTERM (unmount, final
  publish, socket cleanup). It works on macOS, which has no parent-death signal.
  A descriptor that is not open for reading, or is not a pipe or socket, and a
  value that is not a descriptor number, refuse startup by name. Without the
  flag nothing changes. Embedders set `DaemonOptions::lifeline_fd`. ADR-0049
  lists the flag as a launch action (CLI-only), not a configuration key.
