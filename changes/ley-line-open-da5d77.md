Section: Fixed

- **The Taskfile's pinned sccache runs its own server on port 4231, not the default 4226** (bead `ley-line-open-da5d77`).
  A different-version sccache server left on the default port by another build (for example a Homebrew 0.19 server) answers the pinned 0.15 client in a protocol it cannot read, so every Rust build task failed with `error reading compile response from server`. Tasks now set `SCCACHE_SERVER_PORT` to 4231 when the pinned binary is the wrapper, keep 4226 when a system sccache is, and leave a port set by hand alone.
