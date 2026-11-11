Section: Breaking

- **Rootfs manifest v2 authenticates directories with their modes and symbolic links, and `leyline rootfs import-oci` writes it: every image's rootfs digest changes** (beads `ley-line-open-4f08ea`, `ley-line-open-faba8f`; closes `ley-line-open-18a7a2`). The rootfs digest stays BLAKE3 over `rootfs.manifest.json`'s exact bytes, and those bytes are now `{"version":2,"entries":[…]}`: every directory with its mode, every regular file with its mode and content, and every symbolic link with its target bytes, in component-wise path order, each entry's parent a listed directory.
  - **Canonical bytes only.** A v2 manifest whose bytes are not the ones its content serializes to is refused, so one tree has one digest (`faba8f`).
  - **v1 is frozen.** It stays readable, in any byte form it shipped in, and keeps its rules: files only, directories implied at `0755`, no links. A reader that does not know a version refuses it.
  - **Links are kept.** `--symlinks preserve` is the new default. A link's target is never followed at import or on resolve, and each run's copy recreates it, so `open("/etc/mtab")` reaches the guest kernel's `/proc/mounts` instead of failing `ENOENT`, and nothing is dropped as a kernel-mount or dangling link. `refuse` and `resolve` remain.
  - **Directory modes.** Directories keep their modes, setuid and setgid cleared and owner `rwx` always set, and empty directories are kept.
  - **What is refused.** A link at a kernel mount point (`dev`, `proc`, `sys`, `tmp`) is refused by name, as a regular file there already was.
  - **The executable.** A run's executable is resolved through the manifest's own links as the guest's kernel resolves it (`bin/sh` through `bin -> usr/bin`), never through a host link. The native tier runs the resolved file.
  - **The import JSON** gains `directories` and `symlinks` counts.
  - **The first-party acceptance images' digests:** `llo` `9d0c…`, `mache` `…`, `rosary` `…`, `hello` `…` (see `tests/oci_rootfs.rs`).
  - **Shared vectors.** `execution/v1/test-vectors/rootfs-manifest.json` pins the v2 bytes and digest beside v1's.
  - **Cloister** adopts it under `cloister-b74da4`.
