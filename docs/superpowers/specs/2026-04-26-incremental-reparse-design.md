# Git-aware Incremental Reparse

**Date:** 2026-04-26
**Status:** Approved
**Bead:** ley-line-open-9b279e

## Problem

`leyline parse` deletes the .db and reparses every file on every invocation. For a 200-file Go project, this takes ~2s. On daemon restart or reparse-on-change, only a handful of files have changed. We should skip unchanged files and only reparse what's different.

## Design

### New tables

```sql
CREATE TABLE IF NOT EXISTS _file_index (
    path TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS _meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

`_file_index` tracks the mtime+size of each file at last parse. `_meta` stores the source root path and optionally the git HEAD SHA for future git-diff optimization.

### Parse flow

```
1. If output .db exists → open it (incremental mode)
   Else → create fresh .db, create all schemas, insert root node

2. In incremental mode:
   a. Read _file_index into HashMap<path, (mtime, size)>
   b. Walk source directory, collect current files with stat()
   c. Classify each file:
      - UNCHANGED: mtime AND size match _file_index → skip
      - CHANGED: mtime OR size differs → delete old rows, reparse
      - NEW: not in _file_index → parse and insert
   d. Classify stale entries:
      - DELETED: in _file_index but not on disk → delete old rows

3. For CHANGED and DELETED files, delete all rows scoped to that file:
   DELETE FROM nodes WHERE id = ?1 OR id LIKE ?1 || '/%'
   DELETE FROM _ast WHERE source_id = ?1
   DELETE FROM _source WHERE id = ?1
   DELETE FROM node_refs WHERE source_id = ?1
   DELETE FROM node_defs WHERE source_id = ?1
   DELETE FROM _imports WHERE source_id = ?1
   DELETE FROM _file_index WHERE path = ?1

4. For CHANGED and NEW files:
   ensure_dirs() for parent paths (INSERT OR IGNORE — dirs may already exist)
   project_file() + extract_go_refs() (same as today)
   INSERT OR REPLACE INTO _file_index (path, mtime, size)

5. Post-sweep: remove orphaned empty directory nodes
   Loop until no rows deleted:
     DELETE FROM nodes WHERE kind = 1 AND id != ''
       AND id NOT IN (SELECT DISTINCT parent_id FROM nodes WHERE parent_id != '')

6. Update _meta: set parse_time, source_root
```

### Root node handling

The root node (`id=""`) uses `INSERT OR IGNORE` instead of `INSERT`. On fresh .db it creates the root; on existing .db it's a no-op.

### `ensure_dirs` change

Directory nodes now use `INSERT OR IGNORE` instead of relying on a `HashSet<String>` to track which dirs were created in the current run. This handles the case where a directory already exists from a previous parse.

### Cross-file reference consistency

`node_refs` stores string tokens ("Validate", "fmt.Println"), not foreign keys to `node_defs` node_ids. Resolution happens at query time. When file B changes:
- B's old refs and defs are deleted (scoped by source_id)
- B's new refs and defs are inserted
- A's refs to B's tokens are unaffected

Dangling refs (refs to tokens with no matching def) are correct, not corruption. They represent calls to standard library or external packages.

### mtime+size as cache key

Same approach as Make, cargo, rsync. Known limitation: sub-second writes on HFS+ (1-second mtime resolution) with identical file sizes won't be detected. APFS (default macOS) uses nanosecond resolution. Acceptable for a developer tool.

### What this does NOT cover

- Git-diff optimization (using `git diff --name-only` instead of stat-every-file) — follow-up
- File watcher for live reparse — separate bead
- Progressive/streaming parse with incremental arena flips — separate bead
- jj integration — stays in ley-line (private)
