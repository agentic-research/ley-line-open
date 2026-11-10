Section: Fixed

- **A daemon on an explicit `--arena` no longer takes over
  `~/.mache/default.sock`** (bead `ley-line-open-e7459a`). Every daemon used
  to repoint `default.sock` at its own socket and leave it dangling when it
  exited, so mache's `DiscoverOrStart` reached the wrong daemon or a dead
  path. Only the default instance (`~/.mache/default.arena` with
  `~/.mache/default.ctrl`, which is what mache passes) binds that name, and
  on exit it removes the name only while it still holds the socket it bound.
  Any other daemon serves at its own `<ctrl>.sock` and never creates,
  replaces or removes `default.sock`; reach it with `LEYLINE_SOCKET`. An
  explicit `--arena` with the default `--control` is refused. The execution
  daemon follows the same rule for `~/.mache/execution.sock`. The daemon no
  longer publishes discovery symlinks, and `SocketDiscovery::symlink_name` is
  replaced by `instance_name`. A daemon's exit now removes its own socket only
  while the path still names it, so a newer daemon's socket survives an older
  one's exit. The rule is scoped to `$HOME/.mache`: a `default.ctrl` or
  `default.sock` in any other directory is an ordinary daemon. The daemon now
  installs its SIGINT/SIGTERM handlers before it announces itself; a
  supervisor that sent SIGTERM on `daemon ready` used to kill it with no final
  snapshot and its socket left behind.
