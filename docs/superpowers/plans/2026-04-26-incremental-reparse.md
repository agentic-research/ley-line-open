# Incremental Reparse Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `leyline parse` incremental — skip unchanged files, delete+reparse only what changed, so subsequent runs are near-instant.

**Architecture:** Add `_file_index` and `_meta` tables to track per-file mtime+size. On parse, open existing .db, classify files as unchanged/changed/new/deleted, delete stale rows by source_id (or path prefix for nodes), reparse only changed+new files, sweep orphaned directory nodes.

**Tech Stack:** Rust (edition 2024), rusqlite, leyline-ts, leyline-schema

## Hard Rules

1. Every change gets a test. The incremental logic gets unit tests AND integration tests.
2. No code duplication. Reuse existing `project_file`, `ensure_dirs`, `collect_files`.
3. The fresh-parse path (no existing .db) must produce identical output to the current code.
4. The `nodes` table has no `source_id` column — use path-prefix deletion: `WHERE id = ?1 OR id LIKE ?1 || '/%'`.

---

### Task 1: Add `_file_index` and `_meta` schema

**Files:**
- Modify: `rs/ll-open/ts/src/schema.rs`

- [ ] **Step 1: Add DDL constants and create function**

Add after the existing `IMPORTS_DDL`:

```rust
/// DDL for the `_file_index` table — tracks per-file mtime+size for incremental reparse.
pub const FILE_INDEX_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _file_index (
    path TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL
);";

/// DDL for the `_meta` table — stores parse metadata (source root, git SHA, etc.).
pub const META_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

/// Create `_file_index` and `_meta` tables (idempotent).
pub fn create_index_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(FILE_INDEX_DDL)?;
    conn.execute_batch(META_DDL)?;
    Ok(())
}

/// Insert or update a file index entry.
pub fn upsert_file_index(conn: &Connection, path: &str, mtime: i64, size: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _file_index (path, mtime, size) VALUES (?1, ?2, ?3)",
        params![path, mtime, size],
    )?;
    Ok(())
}

/// Read all file index entries into a HashMap.
pub fn read_file_index(conn: &Connection) -> Result<std::collections::HashMap<String, (i64, i64)>> {
    let mut stmt = conn.prepare("SELECT path, mtime, size FROM _file_index")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))
    })?;
    let mut map = std::collections::HashMap::new();
    for row in rows {
        let (path, (mtime, size)) = row?;
        map.insert(path, (mtime, size));
    }
    Ok(map)
}

/// Set a metadata key-value pair.
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}
```

- [ ] **Step 2: Add unit test**

Add to the existing `#[cfg(test)] mod tests` block in `schema.rs`:

```rust
    #[test]
    fn file_index_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();

        upsert_file_index(&conn, "main.go", 1000, 500).unwrap();
        upsert_file_index(&conn, "util.go", 2000, 300).unwrap();

        let index = read_file_index(&conn).unwrap();
        assert_eq!(index.len(), 2);
        assert_eq!(index["main.go"], (1000, 500));
        assert_eq!(index["util.go"], (2000, 300));

        // Upsert overwrites
        upsert_file_index(&conn, "main.go", 3000, 600).unwrap();
        let index = read_file_index(&conn).unwrap();
        assert_eq!(index["main.go"], (3000, 600));
    }

    #[test]
    fn meta_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        create_index_schema(&conn).unwrap();

        set_meta(&conn, "source_root", "/tmp/project").unwrap();
        let val: String = conn.query_row(
            "SELECT value FROM _meta WHERE key = 'source_root'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(val, "/tmp/project");
    }
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-ts --lib`

Expected: All existing tests + 2 new tests pass.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/ts/src/schema.rs
git commit -m "feat(ts): add _file_index + _meta schema for incremental reparse"
```

---

### Task 2: Add `delete_file_rows` function

**Files:**
- Modify: `rs/ll-open/ts/src/schema.rs`

- [ ] **Step 1: Add the deletion function**

This deletes all rows for a single source file across all tables. The `nodes` table uses path-prefix deletion since it has no `source_id` column.

```rust
/// Delete all rows for a source file across all tables.
///
/// The `nodes` table uses path-prefix deletion (`id = path OR id LIKE path/%`)
/// because it has no `source_id` column. All other tables use `source_id = path`.
pub fn delete_file_rows(conn: &Connection, path: &str) -> Result<()> {
    // nodes: path-prefix (file node + all AST children)
    conn.execute(
        "DELETE FROM nodes WHERE id = ?1 OR id LIKE ?1 || '/%'",
        [path],
    )?;
    // Tables with source_id
    conn.execute("DELETE FROM _ast WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _source WHERE id = ?1", [path])?;
    conn.execute("DELETE FROM node_refs WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM node_defs WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _imports WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _file_index WHERE path = ?1", [path])?;
    Ok(())
}

/// Remove directory nodes that have no children (post-deletion cleanup).
/// Returns the number of orphaned directories removed.
pub fn sweep_orphaned_dirs(conn: &Connection) -> Result<usize> {
    let mut total = 0;
    loop {
        let removed = conn.execute(
            "DELETE FROM nodes WHERE kind = 1 AND id != '' \
             AND id NOT IN (SELECT DISTINCT parent_id FROM nodes WHERE parent_id IS NOT NULL AND parent_id != '')",
            [],
        )?;
        if removed == 0 {
            break;
        }
        total += removed;
    }
    Ok(total)
}
```

- [ ] **Step 2: Add unit tests**

```rust
    #[test]
    fn delete_file_rows_cleans_all_tables() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();

        // Insert rows for two files
        insert_node(&conn, "", "", "", 1, 0, 0, "").unwrap();
        insert_node(&conn, "a.go", "", "a.go", 1, 0, 0, "").unwrap();
        insert_node(&conn, "a.go/func", "a.go", "func", 0, 10, 0, "body").unwrap();
        insert_node(&conn, "b.go", "", "b.go", 1, 0, 0, "").unwrap();
        insert_node(&conn, "b.go/func", "b.go", "func", 0, 10, 0, "body").unwrap();

        insert_source(&conn, "a.go", "go", b"package a").unwrap();
        insert_source(&conn, "b.go", "go", b"package b").unwrap();
        insert_ref(&conn, "Foo", "a.go/call", "a.go").unwrap();
        insert_ref(&conn, "Bar", "b.go/call", "b.go").unwrap();
        insert_def(&conn, "Foo", "a.go/func", "a.go").unwrap();
        insert_def(&conn, "Bar", "b.go/func", "b.go").unwrap();
        upsert_file_index(&conn, "a.go", 100, 50).unwrap();
        upsert_file_index(&conn, "b.go", 200, 60).unwrap();

        // Delete a.go
        delete_file_rows(&conn, "a.go").unwrap();

        // a.go rows gone
        let a_nodes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE id = 'a.go' OR id LIKE 'a.go/%'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(a_nodes, 0, "a.go nodes should be deleted");

        let a_source: i64 = conn.query_row(
            "SELECT COUNT(*) FROM _source WHERE id = 'a.go'", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(a_source, 0);

        let a_refs: i64 = conn.query_row(
            "SELECT COUNT(*) FROM node_refs WHERE source_id = 'a.go'", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(a_refs, 0);

        let a_index: i64 = conn.query_row(
            "SELECT COUNT(*) FROM _file_index WHERE path = 'a.go'", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(a_index, 0);

        // b.go rows intact
        let b_nodes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE id = 'b.go' OR id LIKE 'b.go/%'",
            [], |r| r.get(0),
        ).unwrap();
        assert!(b_nodes >= 2, "b.go nodes should still exist");

        let b_refs: i64 = conn.query_row(
            "SELECT COUNT(*) FROM node_refs WHERE source_id = 'b.go'", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(b_refs, 1);
    }

    #[test]
    fn sweep_orphaned_dirs_removes_empty_parents() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();

        // Tree: root → src → src/pkg → src/pkg/a.go
        insert_node(&conn, "", "", "", 1, 0, 0, "").unwrap();
        insert_node(&conn, "src", "", "src", 1, 0, 0, "").unwrap();
        insert_node(&conn, "src/pkg", "src", "pkg", 1, 0, 0, "").unwrap();
        insert_node(&conn, "src/pkg/a.go", "src/pkg", "a.go", 1, 0, 0, "").unwrap();

        // Delete the file
        conn.execute("DELETE FROM nodes WHERE id = 'src/pkg/a.go'", []).unwrap();

        // Sweep should remove src/pkg (empty) then src (empty)
        let removed = sweep_orphaned_dirs(&conn).unwrap();
        assert_eq!(removed, 2, "should remove src/pkg and src");

        // Only root remains
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "only root node should remain");
    }
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-ts --lib`

Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/ts/src/schema.rs
git commit -m "feat(ts): add delete_file_rows + sweep_orphaned_dirs"
```

---

### Task 3: Rewrite `cmd_parse` for incremental mode

**Files:**
- Modify: `rs/ll-open/cli-lib/src/cmd_parse.rs`

- [ ] **Step 1: Rewrite `cmd_parse` function**

Replace the body of `cmd_parse` with the incremental-aware version. The key changes:

1. Don't delete the output .db — open it if it exists
2. Create all schemas (idempotent)
3. Use `INSERT OR IGNORE` for root node
4. Read `_file_index` to determine what changed
5. Delete stale rows for changed/deleted files
6. Only parse changed + new files
7. Sweep orphaned directories
8. Update `_meta`

```rust
pub fn cmd_parse(source: &Path, output: &Path, lang_filter: Option<&str>) -> Result<()> {
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }

    let lang_filter = lang_filter
        .map(TsLanguage::from_name)
        .transpose()
        .context("invalid --lang")?;

    // Collect all source files on disk.
    let mut files = Vec::new();
    collect_files(source, &mut files)?;

    // Open existing .db or create fresh.
    let incremental = output.exists();
    let conn =
        Connection::open(output).with_context(|| format!("open {}", output.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;

    // Create all schemas (idempotent).
    create_ast_schema(&conn)?;
    create_refs_schema(&conn)?;
    leyline_ts::schema::create_index_schema(&conn)?;

    let mtime_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64;

    // Root node — INSERT OR IGNORE so it's a no-op on existing .db.
    conn.execute(
        "INSERT OR IGNORE INTO nodes (id, parent_id, name, kind, size, mtime, record) \
         VALUES ('', '', '', 1, 0, ?1, '')",
        [mtime_now],
    )?;

    // Read existing file index (empty HashMap if fresh .db).
    let old_index = if incremental {
        leyline_ts::schema::read_file_index(&conn)?
    } else {
        HashMap::new()
    };

    // Build current file map: rel_path → (mtime, size, abs_path, lang).
    let mut current_files: HashMap<String, (i64, i64, PathBuf, TsLanguage)> = HashMap::new();
    for path in &files {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let lang = match TsLanguage::from_extension(ext) {
            Some(l) => l,
            None => continue,
        };
        if let Some(filter) = lang_filter
            && lang != filter
        {
            continue;
        }
        let rel = path.strip_prefix(source).unwrap_or(path);
        let rel_str = rel.to_string_lossy().to_string();

        let meta = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?;
        let file_mtime = meta
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        let file_size = meta.len() as i64;

        current_files.insert(rel_str, (file_mtime, file_size, path.clone(), lang));
    }

    // Classify files.
    let mut to_parse: Vec<(String, PathBuf, TsLanguage)> = Vec::new();
    let mut unchanged = 0u64;

    for (rel, (file_mtime, file_size, abs_path, lang)) in &current_files {
        if let Some(&(old_mtime, old_size)) = old_index.get(rel) {
            if *file_mtime == old_mtime && *file_size == old_size {
                unchanged += 1;
                continue;
            }
        }
        to_parse.push((rel.clone(), abs_path.clone(), *lang));
    }

    // Deleted files: in old_index but not in current_files.
    let mut deleted = 0u64;
    for old_path in old_index.keys() {
        if !current_files.contains_key(old_path) {
            leyline_ts::schema::delete_file_rows(&conn, old_path)?;
            deleted += 1;
        }
    }

    // Delete stale rows for changed files (before re-inserting).
    for (rel, _, _) in &to_parse {
        if old_index.contains_key(rel) {
            leyline_ts::schema::delete_file_rows(&conn, rel)?;
        }
    }

    // Parse changed + new files.
    let mut dirs_created: HashSet<String> = HashSet::new();
    let mut parsed = 0u64;
    let mut errors = 0u64;

    for (rel, abs_path, lang) in &to_parse {
        let content =
            std::fs::read(abs_path).with_context(|| format!("read {}", abs_path.display()))?;

        let rel_path = Path::new(rel);
        ensure_dirs(&conn, rel_path, mtime_now, &mut dirs_created)?;

        match project_file(&conn, &content, *lang, rel, mtime_now) {
            Ok(()) => {
                let meta = std::fs::metadata(abs_path)?;
                let file_mtime = meta
                    .modified()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as i64;
                let file_size = meta.len() as i64;
                leyline_ts::schema::upsert_file_index(&conn, rel, file_mtime, file_size)?;
                parsed += 1;
            }
            Err(e) => {
                eprintln!("warn: {}: {e:#}", abs_path.display());
                errors += 1;
            }
        }
    }

    // Sweep orphaned directory nodes.
    let swept = leyline_ts::schema::sweep_orphaned_dirs(&conn)?;
    if swept > 0 {
        eprintln!("{swept} orphaned directory nodes removed");
    }

    // Update metadata.
    let source_abs = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
    leyline_ts::schema::set_meta(
        &conn,
        "source_root",
        &source_abs.to_string_lossy(),
    )?;
    leyline_ts::schema::set_meta(
        &conn,
        "parse_time",
        &chrono::Utc::now().to_rfc3339(),
    )?;

    eprintln!(
        "{parsed} parsed, {unchanged} unchanged, {deleted} deleted, {errors} errors -> {}",
        output.display()
    );

    Ok(())
}
```

Wait — `chrono` is a new dependency. Use a simpler timestamp instead:

Replace the chrono line with:
```rust
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    leyline_ts::schema::set_meta(&conn, "parse_time", &now.to_string())?;
```

- [ ] **Step 2: Update `ensure_dirs` to use INSERT OR IGNORE**

The `ensure_dirs` function currently uses a `HashSet` to avoid re-inserting dirs within a single run. For incremental mode, dirs might already exist from a previous parse. Change `insert_node` to `INSERT OR IGNORE`:

```rust
fn ensure_dirs(
    conn: &Connection,
    rel: &Path,
    mtime: i64,
    created: &mut HashSet<String>,
) -> Result<()> {
    let mut accumulated = String::new();
    let components: Vec<_> = rel
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
        .collect();

    for comp in components {
        let name = comp.as_os_str().to_string_lossy();
        let parent = accumulated.clone();
        if accumulated.is_empty() {
            accumulated = name.to_string();
        } else {
            accumulated = format!("{accumulated}/{name}");
        }
        if created.insert(accumulated.clone()) {
            conn.execute(
                "INSERT OR IGNORE INTO nodes (id, parent_id, name, kind, size, mtime, record) \
                 VALUES (?1, ?2, ?3, 1, 0, ?4, '')",
                rusqlite::params![&accumulated, &parent, &*name, mtime],
            )?;
        }
    }
    Ok(())
}
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli-lib && cargo test -p leyline-cli-lib`

Expected: All tests pass. The existing integration tests (which create fresh .db files) should still work identically.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/cmd_parse.rs
git commit -m "feat(cli): incremental reparse — skip unchanged, delete+reparse changed"
```

---

### Task 4: Integration tests for incremental behavior

**Files:**
- Modify: `rs/ll-open/cli-lib/tests/integration.rs`

- [ ] **Step 1: Add test for skip-unchanged**

```rust
#[tokio::test]
async fn test_incremental_skip_unchanged() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("incr.db");

    // Write two Go files
    std::fs::write(
        src.path().join("a.go"),
        b"package main\n\nfunc A() {}\n",
    ).unwrap();
    std::fs::write(
        src.path().join("b.go"),
        b"package main\n\nfunc B() {}\n",
    ).unwrap();

    // First parse: both files parsed
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let defs_after_first: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs", [], |r| r.get(0),
    ).unwrap();
    assert!(defs_after_first >= 2, "should have defs for A and B");
    drop(conn);

    // Second parse: no changes — should skip both
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    // Verify data unchanged
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let defs_after_second: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(defs_after_first, defs_after_second, "no changes = same data");

    // Verify _file_index has both files
    let index_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _file_index", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(index_count, 2);
}
```

- [ ] **Step 2: Add test for changed file reparse**

```rust
#[tokio::test]
async fn test_incremental_reparse_changed_file() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("incr-change.db");

    std::fs::write(
        src.path().join("main.go"),
        b"package main\n\nfunc Old() {}\n",
    ).unwrap();

    // First parse
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let old_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'Old'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(old_def, 1);
    drop(conn);

    // Modify the file (change function name)
    // Sleep briefly to ensure mtime changes on all filesystems
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::write(
        src.path().join("main.go"),
        b"package main\n\nfunc New() {}\nfunc Extra() {}\n",
    ).unwrap();

    // Second parse: should detect change and reparse
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // Old def gone, new defs present
    let old_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'Old'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(old_def, 0, "Old def should be gone after reparse");

    let new_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'New'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(new_def, 1, "New def should exist");

    let extra_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'Extra'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(extra_def, 1, "Extra def should exist");
}
```

- [ ] **Step 3: Add test for deleted file cleanup**

```rust
#[tokio::test]
async fn test_incremental_deleted_file() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("incr-delete.db");

    std::fs::write(src.path().join("keep.go"), b"package main\n\nfunc Keep() {}\n").unwrap();
    std::fs::write(src.path().join("remove.go"), b"package main\n\nfunc Remove() {}\n").unwrap();

    // First parse: both files
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let remove_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'Remove'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(remove_def, 1);
    drop(conn);

    // Delete remove.go from disk
    std::fs::remove_file(src.path().join("remove.go")).unwrap();

    // Second parse: should detect deletion
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // Remove.go rows gone
    let remove_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'Remove'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(remove_def, 0, "Remove def should be deleted");

    let remove_source: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _source WHERE id = 'remove.go'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(remove_source, 0);

    // Keep.go rows intact
    let keep_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'Keep'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(keep_def, 1, "Keep def should still exist");

    // _file_index only has keep.go
    let index_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _file_index", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(index_count, 1);
}
```

- [ ] **Step 4: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-cli-lib`

Expected: All existing tests + 3 new tests pass.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`

- [ ] **Step 6: Commit and push**

```bash
git add rs/ll-open/cli-lib/tests/integration.rs
git commit -m "test: incremental reparse — skip unchanged, reparse changed, delete removed"
git push origin main
```
