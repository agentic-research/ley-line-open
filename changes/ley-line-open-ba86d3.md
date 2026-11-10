Section: Added

- **A case-sensitive CAS root on macOS, and a by-name refusal where the CAS
  root would merge image paths** (bead `ley-line-open-ba86d3`). Debian-based
  images (Debian's perl ships `Pod/` and `pod/`) need a CAS root that tells
  case apart, and macOS's default APFS does not.
  - `leyline rootfs import-oci` probes the CAS root's case and normalization
    sensitivity once, before reading any layer, and reports it as
    `casRootFilesystem`. Before writing anything it refuses an image with two
    paths the CAS root would store as one, with the new
    `OciImportError::PathCollision`. The error names both paths and says the
    CAS root is case-insensitive (or normalization-insensitive), so the image
    isn't blamed. Before, the import failed with a bare `AlreadyExists` that
    named no path. Images without such a pair still import on any root, to
    the same digest.
  - `leyline rootfs provision-cas [--state-dir DIR] [--size-cap-gib N]` and
    `leyline_runtime::cas_volume::CasVolume` provide a CAS root on a
    case-sensitive filesystem:
    - **macOS:** a grow-on-demand, case-sensitive APFS sparsebundle, 64 GiB
      cap by default. `hdiutil` creates it once and attaches it at
      `<state>/cas-volume/mnt`, without sudo. The CAS root is
      `<state>/cas-volume/mnt/cas`, under `~/.local/state/leyline` unless
      `$LEYLINE_STATE_DIR` or `$XDG_STATE_HOME` says otherwise. Re-running
      the command reuses the existing volume.
    - **Linux:** checks that `<state>/cas` is case-sensitive.
    - **Other hosts:** refused by name.
  - `leyline execution-daemon --cas-volume [STATE_DIR]` serves that root in
    place of `--cas-root`. It re-attaches the volume at start and never
    creates one there. execution/v1 `ProvisionOutput` gains `casRoot`
    (appended at `@2`), the CAS root the provision made exist.
  - Consumers that validate provision responses against a pinned copy of
    `execution-tools.json` with `additionalProperties: false` must re-sync
    that copy.
