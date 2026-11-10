Section: Fixed

- **A microVM run whose executable sits at the rootfs top level runs**
  (bead `ley-line-open-fe130c`). The plan anchored a guest-relative
  executable at `/` only when a workspace was mounted. Without one, libkrun's
  init was handed a bare name such as `hello`, `execvp` searched `PATH`, and
  the run exited 127, which broke most scratch and distroless images
  imported with `leyline rootfs import-oci`. `compile_plan` now anchors every
  executable at `/`, so no run depends on `PATH` to find its own program.
