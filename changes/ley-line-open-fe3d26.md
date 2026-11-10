Section: Fixed

- **`leyline rootfs import-oci` imports LLO's, mache's and rosary's published images** (beads `ley-line-open-fe3d26`, `ley-line-open-391965`).
  Each was refused whole over an entry the rootfs cannot carry faithfully:
  - LLO v0.27.0 over the `dev/console` character device and the mode-`0000` `etc/shadow` in its chainguard base;
  - mache and rosary, under `--symlinks resolve`, over dangling links (`usr/share/doc/apt/NEWS.Debian.gz`, `usr/share/zoneinfo/localtime`).

  The import now drops these entries, writes nothing in their place, and lists each in one of four new JSON fields:
  - `specialEntriesDropped`: device nodes and FIFOs strictly beneath `dev/`, `proc/` or `sys/`. libkrun's guest init mounts kernel filesystems over those directories before the workload runs.
  - `unreadableFilesDropped`: regular files without their owner-read bit. No unprivileged reader can read one, the guest's virtio-fs server included.
  - `linksDroppedKernelMount` (under `--symlinks resolve`): links whose resolution goes beneath, or ends at, `dev`, `proc` or `sys`.
  - `linksDroppedDangling` (under `--symlinks resolve`): links whose target the image does not hold.

  The runtime README's "Dropped entries" section cites the `init.c` lines behind each rule and states what a guest can still observe of each drop.

  Still refused by name:
  - devices elsewhere;
  - link loops;
  - a `..` above the image root, including after a missing component;
  - a directory link that contains itself.

  `--symlinks refuse` stays the default and is unchanged.

  Behavior changes for images that imported before:
  - Under `resolve`, a link into `dev`, `proc` or `sys` whose target the image held (`cpuinfo -> /proc/cpuinfo` beside an image file `proc/cpuinfo`) was a copy of the image's bytes. It is now dropped, which changes that image's rootfs digest.
  - A link that reaches `/work`, where a run's workspace is mounted, is now refused.
  - A directory entry whose owner cannot both read and search it is now refused: directories are written `0755`, so it would expose what it hides.

  The rootfs digest is still BLAKE3 over `rootfs.manifest.json`, and a dropped entry is not in it. An image with nothing to drop or newly refuse keeps its digest: hello-world's is unchanged, as is the test fixture's known answer.
