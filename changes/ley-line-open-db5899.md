Section: Fixed

- **Every microVM guest has a writable, bounded `/tmp`** (bead `ley-line-open-db5899`, libkrun patch `0012`).
  - **The problem.** Guest init mounted only `/dev`, `/proc` and `/sys`, and an OCI import drops an image's empty `tmp/`, so a guest such as Dolt's `sql-server` failed with "open /tmp: no such file or directory".
  - **The fix.** Init now mounts a tmpfs on `/tmp` before the workload starts: mode 1777, `nosuid` and `nodev`, sized at 25 % of the guest's RAM. A write past that bound fails `ENOSPC`. The tmpfs is per-boot and memory-backed, so nothing written there reaches the host or outlives the run. Init mounts only over a real directory; a `/tmp` that is a link or a file stops the guest.
  - **Import changes.** `leyline rootfs import-oci` now treats `tmp/` like `dev/`, `proc/` and `sys/`, because the tmpfs shadows whatever the image holds there.
    - A device or FIFO beneath `tmp/` is dropped and reported. Such an image was refused before and now imports.
    - Under `--symlinks resolve`, a link into `tmp/` is dropped and reported in `linksDroppedKernelMount`. Before, it was copied, or dropped as dangling.
  - **Rootfs digests.** A digest changes only for an image where such a link resolved to files the image held under `tmp/`. A link that was dangling before, or `var/tmp -> /tmp` over an empty `tmp/`, keeps its digest, and only the report field it appears in changes. Regular files under `tmp/` are still imported, as under `dev/`.
