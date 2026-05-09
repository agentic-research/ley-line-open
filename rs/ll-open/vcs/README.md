# leyline-vcs

jj sidecar — automatic versioning of arena snapshots.

## What's here

- **`VersionedGraph`** — wraps any `Graph` implementation. A debounced commit loop watches the Controller generation counter and snapshots each new generation as a jj commit.
- **`.leyline/` virtual directory** — exposed in the mount, provides:
  - `status` — current generation, dirty state
  - `log` — recent commit history
  - `revert` — write a commit hash to revert to that generation
- **`.staging/` virtual directory** — CoW overlay for atomic multi-node edits (powered by `StagingGraph` in `leyline-fs`).

## Dependencies

Requires [jj](https://martinvonz.github.io/jj/) installed and on PATH. Uses `jj-lib` for programmatic access.
