Section: Fixed

- **Every microVM guest has a writable, bounded `/tmp`** (bead `ley-line-open-db5899`, libkrun patch `0012`).
  - **The problem.** Guest init mounted only `/dev`, `/proc` and `/sys`, and an OCI import drops an image's empty `tmp/`, so a guest such as Dolt's `sql-server` failed with "open /tmp: no such file or directory".
  - **The fix.** Init now mounts a tmpfs on `/tmp` before the workload starts: mode 1777, `nosuid` and `nodev`, sized at 25 % of the guest's RAM. A write past that bound fails `ENOSPC`. The tmpfs is per-boot and memory-backed, so nothing written there reaches the host or outlives the run.
  - **Import changes.** `leyline rootfs import-oci` now treats `tmp/` like `dev/`, `proc/` and `sys/`, because the tmpfs shadows whatever the image holds there. A device or FIFO beneath `tmp/` is dropped and reported instead of refused. Under `--symlinks resolve`, a link into `tmp/`, such as `var/tmp -> /tmp`, is dropped and reported in `linksDroppedKernelMount` instead of being copied.
  - **Rootfs digests.** An image whose resolved links reached into `tmp/` gets a different rootfs digest. Regular files under `tmp/` are still imported, as under `dev/`.
