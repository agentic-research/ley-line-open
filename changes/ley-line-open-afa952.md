Section: Removed

- **The CDC `nodes` target** (bead `ley-line-open-afa952`). CDC indexes
  `source_blobs` only. The per-node target measured +21% database size for
  1.9 MB of dedup on a real mache projection (bead `ley-line-open-baa57f`:
  every leaf sat below the 8 KiB chunking floor). Removed with it:
  `leyline_fs::chunked` (`content_manifest`, `content_manifest_meta`,
  `content_generation`, the `nodes` generation triggers,
  `WITNESS_FRESH_PREDICATE`, chunked graph reads and incremental write
  refresh), `activate_chunked_content*`, `ActivationReport`,
  `ActivationProgress`, `cmd_daemon::CdcActivationReport`,
  `cmd_cdc::*_source_blobs*` (the source_blobs functions now carry the plain
  names), `cdc enable --target`, the execution daemon's `--cdc-target`, the
  `nodes` value of the daemon's `--cdc-target` (now default `source-blobs`;
  `nodes` is refused), `BlobActivationReport::stranded_nodes_manifest_rows`
  and `GcReport::reaped_manifest_rows`/`reaped_manifest_nodes`, and the
  `cdc_storage_bound` and `cdc_churn` examples. `DaemonOptions::cdc` is a
  `bool`. `leyline cdc enable --db x.db` now activates `source_blobs`; mache's
  `daemon --cdc --cdc-target source-blobs` spawn is unchanged.
  `SqliteGraphAdapter` reads always serve `nodes.record`. A database activated
  under the old target keeps its node-manifest tables; nothing reads them,
  `leyline cdc gc` collects the chunks only they referenced, and
  `daemon --reset-arena` rebuilds without them.
