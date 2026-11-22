Section: Fixed

- **Every child the runtime starts inherits exactly what it names** (bead `ley-line-open-528525`; ADR-0046 Slice 1 S7).
  Native workers, libkrun workers, connection brokers and the `codesign` and
  `hdiutil` runs are now spawned through one helper
  (`leyline_runtime::backends::spawn`). On macOS it is `posix_spawn` with
  `POSIX_SPAWN_CLOEXEC_DEFAULT` and file actions for the standard streams and
  the inherit list. On Linux, where pipes are close-on-exec atomically, the
  child marks every descriptor from 3 up close-on-exec with `close_range`
  before exec, then hands over exactly the named ones; on a kernel without
  `close_range` (before 5.11) it marks the descriptors listed just before the
  spawn instead. Before, a child spawned on one thread at the moment another
  thread had created a lifeline pipe but not yet marked it close-on-exec
  (macOS has no `pipe2`) inherited that run's write end and kept its worker
  or broker alive after its supervisor died. The process group, `SIGPIPE`
  default and empty signal mask are kept. On Linux a spawn forks, and from
  the fork to its exec the new child holds a copy of every descriptor the
  daemon held, another run's lifeline write end among them; the copy closes
  at that exec. A write end closed while children forked before that are
  still between fork and exec is seen at its far end once the last of them
  has exec'd. After every exec, a child holds exactly what it was named, on
  both platforms. In the API, `Spawn::inherit` takes a `BorrowedFd`, so a
  descriptor stays open until the spawn is done with it, and `Spawn::spawn`
  refuses, by name, a program that is no absolute path and an inherit entry
  whose source is a standard stream or another entry's target.
