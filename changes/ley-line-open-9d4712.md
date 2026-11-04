Section: Breaking

- **`mount` is the NFS host mount alone; FUSE is the new `fuse` feature**
  (bead `ley-line-open-9d4712`). `leyline-cli --features mount` (and
  `leyline-storefs --features mount`) now builds the loopback NFS server
  without fuser: the binary links no libfuse-t or libfuse, needs no
  `PKG_CONFIG_PATH`, and launches on a Mac without fuse-t. `--features fuse`
  (which implies `mount`) adds `--backend fuse` and its libfuse link, and
  `full` includes it, so `task install:full+mount` builds what it did. A
  build without `fuse` refuses `--backend fuse` before making the
  mountpoint, naming the feature. The default backend is unchanged: NFS on
  macOS, FUSE elsewhere — so a Linux `mount`-only build mounts only with
  `--backend nfs`; build Linux mounts with `--features fuse` to keep the
  default working.
