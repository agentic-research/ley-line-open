Section: Breaking

- **`current_root` is omitted when no root is published, on every daemon
  wire site, and a controller read error is never sent as a root** (bead
  `ley-line-open-80f037`). One rule, documented on
  `StatusResponse.rootPublished` in `daemon.capnp`, now governs the
  `status`, `reparse`, `snapshot`, `flush`, `enrich` and `load` responses
  and the `daemon.sheaf.invalidate` event: `root_published` is always
  present, and `current_root` is present exactly when it is `true`.
  - Breaking: `current_root` used to be 64 zero hex digits before the first
    publish (and after `--reset-arena`). The key is now absent instead. A
    consumer that read the zero spelling as "no root" tests
    `root_published` or the key's absence.
  - Breaking: the `daemon.sheaf.invalidate` event used to send `""` as
    `current_root` when the controller could not be read. The watcher's
    event now carries `root_error` (text) in place of `root_published` and
    `current_root`, and still delivers the invalidation. The consumer-driven
    `sheaf_invalidate` op reads the root before it touches the cache and
    fails with the controller error, leaving the cache unchanged and
    emitting nothing.
  - Additive: `rootPublished` on `ReparseResponse @9`, `SnapshotResponse
    @3`, `FlushResponse @2`, `EnrichResponse @4` and `LoadResponse @3`. Go:
    `FlushResponse.RootPublished`, `SnapshotResponse.RootPublished`, and
    `SheafInvalidatePayload.{Scope, ChangedFiles, RootPublished,
    CurrentRoot, RootError}`; `DecodeEvent` now types the
    `daemon.sheaf.invalidate` topic as well as `sheaf.invalidate`.
  - `ops::read_root_hex` returns `Option<String>` (`None` = unpublished).
