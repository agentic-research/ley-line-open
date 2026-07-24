# CDC Reachability GC Design

**Bead:** `ley-line-open-035363`

**Status:** Approved implementation scope

**Date:** 2026-07-24

## Goal

Bound storage growth in long-lived CDC projections without weakening the
materialize-on-read correctness contract. Garbage collection is explicit and
off the write path:

```text
leyline cdc gc --db <projection.db> [--dry-run] [--json]
```

`nodes.record` remains authoritative. This operation only removes private
derived chunk rows that no committed manifest references.

## Reachability and Transaction Boundary

`content_manifest.chunk_hash` is the complete root set for
`content_chunks.chunk_hash`. A chunk is collectible exactly when no manifest
row references it:

```sql
NOT EXISTS (
  SELECT 1
  FROM content_manifest AS manifest
  WHERE manifest.chunk_hash = content_chunks.chunk_hash
)
```

Accounting, reachability, deletion, and final totals run inside one SQLite
`IMMEDIATE` transaction. This excludes concurrent manifest writers between the
reachability decision and deletion. A failed delete rolls the transaction back;
there is no partial sweep or success report. GC creates the private
`content_manifest_chunk_hash` index if an older activated projection lacks it;
an `EXPLAIN QUERY PLAN` gate proves reachability uses indexed manifest lookups
instead of a quadratic correlated scan. Dry-run builds that index inside the
transaction for the same bounded plan, then rolls the whole transaction back
so the compatibility migration is not persisted.

`NOT EXISTS` is used instead of `NOT IN`, avoiding NULL-sensitive membership
semantics and expressing the root relationship directly. Shared chunks remain
until their final manifest reference disappears.

## API and Report

The library surface is:

```rust
collect_unreachable_chunks(&Connection, GcOptions) -> Result<GcReport>
```

`GcReport` records before, unreachable, deleted, and remaining chunk rows and
deduplicated payload bytes. Byte totals are `SUM(length(chunk_bytes))`, not the
SQLite file size or bytes returned to the filesystem; deleted pages remain
inside SQLite until separate compaction. Dry-run reports the same unreachable
set but records zero deleted rows and bytes and leaves remaining totals
unchanged. A second completed run reports deterministic zero deletion.

The command opens an existing database read/write without creating a missing
path. A database missing `content_chunks` or `content_manifest` is rejected
without schema mutation.

## Falsifiable Gates

- Dry-run accounts for all orphans and changes no rows.
- A chunk shared by two manifests survives removal of one manifest.
- After the final manifest disappears, the shared chunk is collected.
- Mixed live and orphan chunks delete only the orphans; live bytes reconstruct
  exactly.
- An injected delete failure preserves every chunk.
- A second run is idempotent with deterministic zero accounting.
- Reachability uses the manifest hash index on newly activated and existing
  projections.
- Lookalike tables and misspelled database paths are rejected without mutation.
- Public CLI dispatch reaches the destructive GC implementation.
- Feature-disabled CLI builds keep `cdc gc` discoverable and name the required
  `cdc` feature.
- `task test:fs-cdc`, `task test:cdc-activation`, the GC mutation scope, and
  `task ci` pass.

## Compatibility

No public schema, `leyline-schema`, Cap'n Proto wire, `SCHEMA_VERSION`, or
compatibility version changes. The operation uses the private `content_*`
tables introduced by CDC activation.
