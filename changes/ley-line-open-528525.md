Section: Fixed

- **Every worker and broker the daemon starts inherits exactly what it names** (bead `ley-line-open-528525`; ADR-0046 Slice 1 S7).
  Native workers, libkrun workers and connection brokers are now spawned
  through one helper (`leyline_runtime::backends::spawn`). On macOS it is
  `posix_spawn` with `POSIX_SPAWN_CLOEXEC_DEFAULT` and file actions for the
  standard streams and the inherit list; on Linux, where pipes are
  close-on-exec atomically, the child marks every unnamed descriptor
  close-on-exec before exec. Before, a worker or broker spawned on one thread
  at the moment another thread had created a lifeline pipe but not yet marked
  it close-on-exec (macOS has no `pipe2`) inherited that run's write end and
  kept its worker or broker alive after its supervisor died. The process
  group, `SIGPIPE` default and empty signal mask are kept. On Linux a spawn
  forks, and from the fork to its exec the new child holds a copy of every
  descriptor the daemon held, another run's lifeline write end among them;
  that copy is close-on-exec and closes at the child's exec, which the spawn
  waits for. A supervisor that dies in that moment is seen by its own child
  once the sibling has exec'd. After every exec, a child holds exactly what
  it was named, on both platforms.
