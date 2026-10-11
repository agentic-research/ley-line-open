//! ADR-0040 D6 — `leyline rebuild` (bead `ley-line-open-f31efd`).
//!
//! The projection is derived state. Ring 0 is `source_blobs` plus the
//! manifest (`_source`: relative path → content hash); every other table is
//! a function of ring 0 under one `parserId`, and `_meta.derived_from`
//! names which. `rebuild` reads ring 0 out of one arena and projects it into
//! a fresh one through the cold parse, reading nothing from disk.
//!
//! It refuses rather than guess:
//!
//! - an arena with no `derived_from` (parsed before D6) has no stated
//!   derivation to reproduce;
//! - an arena whose `_source` set no longer folds to its stamped `treeRoot`
//!   is not the tree the stamp names;
//! - a stamped `parserId` other than this binary's (against the arena's own
//!   query overrides) is a different derivation. Rebuilding under it would
//!   produce an arena that is not a reproduction, so it is refused; reparse
//!   the source tree with this binary instead;
//! - a file whose bytes are missing from `source_blobs`, or do not hash to
//!   `_source.content_hash`, fails the whole rebuild (`source_bytes` fails
//!   closed), as does a file that no longer parses.
//!
//! A whole-arena rebuild must land the stamped `treeRoot`; a rebuild of a
//! sub-manifest (`--path`, ADR-0040 F5) lands the `treeRoot` of that subset.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::source_write::Grant;
use crate::tree_root::{DerivedFrom, ParserInputs};

/// What a rebuild produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rebuilt {
    /// Files projected into the new arena.
    pub files: usize,
    /// The new arena's `derived_from`.
    pub derived_from: DerivedFrom,
}

/// Rebuild the files `grant` covers from `from`'s ring 0 into `into`, which
/// must hold no tables.
pub fn rebuild(from: &Connection, into: &Connection, grant: &Grant) -> Result<Rebuilt> {
    let stamp = DerivedFrom::read(from)?.context(
        "refusing to rebuild: the arena records no derived_from (it was parsed \
         before ADR-0040 D6); reparse it with this binary first",
    )?;
    let trusted = crate::cmd_parse::trusted_query_hashes_from_env();
    let query_set = leyline_ts::query_engine::resolve_query_set(from, &trusted)?.query_set;
    let parser_id = ParserInputs::current(&query_set).parser_id();
    if parser_id != stamp.parser_id {
        bail!(
            "refusing to rebuild: the arena was derived under parserId {}, this binary \
             derives under {parser_id}; a rebuild here would not reproduce it. Reparse \
             the source tree with this binary instead.",
            stamp.parser_id
        );
    }
    let recomputed = crate::tree_root::tree_root(from, stamp.parser_id)?;
    if recomputed != stamp.tree_root {
        bail!(
            "refusing to rebuild: the arena's _source set folds to treeRoot {recomputed}, \
             but derived_from names {}",
            stamp.tree_root
        );
    }
    let source_root = leyline_ts::schema::get_meta(from, "source_root")?
        .context("refusing to rebuild: the arena records no source_root")?;
    if let Grant::Paths(paths) = grant {
        for path in paths {
            let known: bool = from.query_row(
                "SELECT EXISTS(SELECT 1 FROM _source WHERE id = ?1)",
                [path],
                |r| r.get(0),
            )?;
            if !known {
                bail!("refusing to rebuild: no file '{path}' in the arena");
            }
        }
    }

    let mut files = BTreeMap::new();
    {
        let mut stmt = from.prepare("SELECT id, file_id FROM _source ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (id, file_id) = row?;
            if grant.covers(&id) {
                let bytes = leyline_ts::splice::source_bytes(from, file_id)
                    .with_context(|| format!("refusing to rebuild '{id}'"))?;
                files.insert(id, bytes);
            }
        }
    }

    let has_tables: bool = into.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table')",
        [],
        |r| r.get(0),
    )?;
    if has_tables {
        bail!("refusing to rebuild into a database that already holds tables");
    }
    copy_query_overrides(from, into)?;

    crate::cmd_parse::parse_tree_from_memory(into, Path::new(&source_root), &files)?;
    let derived_from = DerivedFrom::read(into)?
        .context("the rebuilt arena records no derived_from; the parse did not stamp it")?;
    if *grant == Grant::Whole && derived_from.tree_root != stamp.tree_root {
        bail!(
            "rebuild landed treeRoot {}, but the arena it was rebuilt from names {}",
            derived_from.tree_root,
            stamp.tree_root
        );
    }
    Ok(Rebuilt {
        files: files.len(),
        derived_from,
    })
}

/// The arena-resident query overrides are a `parserId` input: carry them,
/// verbatim, so the rebuild resolves the same query set.
fn copy_query_overrides(from: &Connection, into: &Connection) -> Result<()> {
    leyline_ts::schema::create_query_blob_tables(into)?;
    if !leyline_schema::table_exists(from, "_queries")? {
        return Ok(());
    }
    let mut blobs = from.prepare("SELECT blob_hash, blob_bytes FROM query_blobs")?;
    let blob_rows = blobs.query_map([], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    for row in blob_rows {
        let (hash, bytes) = row?;
        into.execute(
            "INSERT INTO query_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
            rusqlite::params![hash, bytes],
        )?;
    }
    let mut queries = from.prepare("SELECT lang, kind, blob_hash FROM _queries")?;
    let query_rows = queries.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in query_rows {
        let (lang, kind, hash) = row?;
        into.execute(
            "INSERT INTO _queries (lang, kind, blob_hash) VALUES (?1, ?2, ?3)",
            rusqlite::params![lang, kind, hash],
        )?;
    }
    Ok(())
}

/// `leyline rebuild <from> <into> [--path P]...`: rebuild into a new file.
pub fn cmd_rebuild(from: &Path, into: &Path, paths: Vec<String>) -> Result<()> {
    if into.exists() {
        bail!("refusing to rebuild: {} already exists", into.display());
    }
    let src = Connection::open_with_flags(from, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {} read-only", from.display()))?;
    let dst = Connection::open(into).with_context(|| format!("open {}", into.display()))?;
    dst.pragma_update(None, "journal_mode", "DELETE")?;
    let grant = if paths.is_empty() {
        Grant::Whole
    } else {
        Grant::Paths(paths.into_iter().collect())
    };
    let rebuilt = rebuild(&src, &dst, &grant)?;
    println!(
        "{} files rebuilt -> {} treeRoot={} parserId={}",
        rebuilt.files,
        into.display(),
        rebuilt.derived_from.tree_root,
        rebuilt.derived_from.parser_id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_write::write;
    use std::collections::{BTreeSet, HashMap};
    use std::path::PathBuf;
    use tempfile::TempDir;

    const A: &str = "package a\n\nfunc A() int { return 1 }\n";
    const A2: &str = "package a\n\nfunc A() int { return 2 }\n\nfunc B(x int) int { return x }\n";
    const B: &str = "package b\n\nfunc C() string { return \"c\" }\n";
    const R: &str = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";

    /// A cold-parsed, file-backed arena over `files`.
    struct Arena {
        src: TempDir,
        dir: TempDir,
        db: PathBuf,
        conn: Connection,
    }

    fn arena(files: &[(&str, &str)]) -> Arena {
        let src = tempfile::tempdir().unwrap();
        for (rel, text) in files {
            let p = src.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("live.db");
        let conn = Connection::open(&db).unwrap();
        crate::cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
        Arena { src, dir, db, conn }
    }

    const FILES: &[(&str, &str)] = &[("a.go", A), ("sub/b.go", B), ("sub/deep/r.rs", R)];

    /// The rows of an arena, canonicalised as ADR-0040 D6 declares: interned
    /// ids (`file_id`, nids, `name_id`, `kind_id`) rendered as what they name;
    /// `nodes.mtime`, `_meta.parse_time`, `_meta.source_root` and the
    /// absolute `_source.path` dropped; `_lsp*` excluded; content-addressed
    /// stores restricted to what the projection reaches, since a write
    /// leaves the replaced bytes and subtrees behind without changing what
    /// the arena projects.
    struct Logical {
        files: HashMap<i64, String>,
        dirs: HashMap<i64, String>,
        names: HashMap<i64, String>,
        kinds: HashMap<i64, String>,
    }

    /// One rendered row: its table, the file it belongs to (if any), and
    /// its canonical text.
    type Row = (String, Option<String>, String);

    impl Logical {
        fn new(conn: &Connection) -> Self {
            let map = |sql: &str| -> HashMap<i64, String> {
                let mut stmt = conn.prepare(sql).unwrap();
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            };
            let names = map("SELECT name_id, text FROM names");
            let mut dirs = HashMap::new();
            let raw: HashMap<i64, (Option<i64>, i64)> = {
                let mut stmt = conn
                    .prepare("SELECT dir_id, parent_dir_id, name_id FROM dirs")
                    .unwrap();
                stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            };
            for &id in raw.keys() {
                let mut parts = Vec::new();
                let mut cur = Some(id);
                while let Some(d) = cur {
                    let (parent, name) = raw[&d];
                    if parent.is_some() {
                        parts.push(names[&name].clone());
                    }
                    cur = parent;
                }
                parts.reverse();
                dirs.insert(id, parts.join("/"));
            }
            Self {
                files: map("SELECT file_id, id FROM _source"),
                dirs,
                names,
                kinds: map("SELECT kind_id, lang || ':' || raw_kind FROM kinds"),
            }
        }

        fn nid(&self, n: i64) -> (Option<String>, String) {
            if n < 0 {
                return (None, format!("dir:{}", self.dirs[&-n]));
            }
            let fid = n >> 24;
            let ord = n - leyline_schema::file_nid(fid, 0);
            let rel = self.files[&fid].clone();
            (Some(rel.clone()), format!("{rel}#{ord}"))
        }

        fn rows(&self, conn: &Connection) -> Vec<Row> {
            let reach = reachable_hashes(conn, OCCURRENCES, i64::MIN, i64::MAX);
            let named = |col: &str, table: &str| -> BTreeSet<Vec<u8>> {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {col} FROM {table} WHERE {col} IS NOT NULL"
                    ))
                    .unwrap();
                stmt.query_map([], |r| r.get(0))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            };
            let source_hashes = named("content_hash", "_source");
            let ast_blobs = named("blob_hash", "_ast_blob");
            let tables: Vec<String> = {
                let mut stmt = conn
                    .prepare(
                        "SELECT name FROM sqlite_master WHERE type = 'table' \
                         AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '\\_lsp%' ESCAPE '\\' \
                         AND name NOT IN ('names', 'kinds', 'dirs', 'files') ORDER BY name",
                    )
                    .unwrap();
                stmt.query_map([], |r| r.get(0))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            };
            let mut out = Vec::new();
            for table in tables {
                let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
                let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
                let mut q = stmt.query([]).unwrap();
                'row: while let Some(r) = q.next().unwrap() {
                    let mut file = None;
                    let mut text = Vec::new();
                    for (i, col) in cols.iter().enumerate() {
                        let v: rusqlite::types::Value = r.get(i).unwrap();
                        use rusqlite::types::Value as V;
                        let keyed = |set: &BTreeSet<Vec<u8>>| match &v {
                            V::Blob(b) => set.contains(b),
                            _ => false,
                        };
                        match (table.as_str(), col.as_str()) {
                            ("nodes", "mtime") | ("_source", "path") => continue,
                            // Not functions of `node_hash`: neither is in its
                            // preimage, so `INSERT OR IGNORE` keeps whichever
                            // file's node landed first, and a write leaves the
                            // replaced file's value behind (bead `b5d354`).
                            ("node_content", "lang" | "raw_kind") => continue,
                            ("_meta", "key") if matches!(&v, V::Text(k) if k == "parse_time" || k == "source_root") =>
                            {
                                continue 'row;
                            }
                            ("source_blobs", "blob_hash") if !keyed(&source_hashes) => {
                                continue 'row;
                            }
                            ("capnp_blobs", "blob_hash") if !keyed(&ast_blobs) => continue 'row,
                            ("node_content", "node_hash") | ("node_child", "parent_hash")
                                if !keyed(&reach) =>
                            {
                                continue 'row;
                            }
                            _ => {}
                        }
                        let rendered = match (col.as_str(), &v) {
                            (_, V::Null) => "∅".to_string(),
                            ("nid" | "parent_nid" | "container_nid", V::Integer(n)) => {
                                let (f, s) = self.nid(*n);
                                file = file.or(f);
                                s
                            }
                            ("file_id", V::Integer(n)) => {
                                let rel = self.files[n].clone();
                                file = file.or(Some(rel.clone()));
                                rel
                            }
                            ("name_id", V::Integer(n)) => self.names[n].clone(),
                            ("kind_id", V::Integer(n)) => self.kinds[n].clone(),
                            (_, V::Integer(n)) => n.to_string(),
                            (_, V::Real(x)) => x.to_string(),
                            (_, V::Text(s)) => format!("{s:?}"),
                            (_, V::Blob(b)) => hex::encode(b),
                        };
                        text.push(format!("{col}={rendered}"));
                    }
                    out.push((table.clone(), file, text.join(" ")));
                }
            }
            out.sort();
            out
        }
    }

    /// The `node_hash`es the rows `roots_sql` selects reach: those hashes
    /// and everything below them in `node_child`.
    fn reachable_hashes(
        conn: &Connection,
        roots_sql: &[&str],
        lo: i64,
        hi: i64,
    ) -> BTreeSet<Vec<u8>> {
        let mut children: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
        let mut stmt = conn
            .prepare("SELECT parent_hash, child_hash FROM node_child")
            .unwrap();
        for row in stmt
            .query_map([], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .unwrap()
        {
            let (p, c) = row.unwrap();
            children.entry(p).or_default().push(c);
        }
        let mut stack: Vec<Vec<u8>> = Vec::new();
        for table in roots_sql {
            let mut s = conn
                .prepare(&format!(
                    "SELECT node_hash FROM {table} WHERE node_hash IS NOT NULL \
                     AND nid BETWEEN ?1 AND ?2"
                ))
                .unwrap();
            stack.extend(
                s.query_map([lo, hi], |r| r.get(0))
                    .unwrap()
                    .map(Result::unwrap),
            );
        }
        let mut seen = BTreeSet::new();
        while let Some(h) = stack.pop() {
            if seen.insert(h.clone())
                && let Some(cs) = children.get(&h)
            {
                stack.extend(cs.iter().cloned());
            }
        }
        seen
    }

    const OCCURRENCES: &[&str] = &["_ast", "node_refs", "node_defs"];

    fn logical(conn: &Connection) -> Vec<Row> {
        Logical::new(conn).rows(conn)
    }

    /// Assert two row sets agree, naming only the rows that differ.
    fn assert_same_rows(rebuilt: &[Row], original: &[Row]) {
        let only = |x: &[Row], y: &[Row]| -> Vec<String> {
            x.iter()
                .filter(|r| !y.contains(r))
                .map(|(t, _, s)| format!("{t}: {s}"))
                .collect()
        };
        let (extra, missing) = (only(rebuilt, original), only(original, rebuilt));
        assert!(
            extra.is_empty() && missing.is_empty() && rebuilt.len() == original.len(),
            "only in the rebuild: {extra:#?}\nonly in the original: {missing:#?}"
        );
    }

    /// The rows one file contributes: every row a nid or `file_id` places in
    /// it, and the `node_content` / `node_child` rows its occurrences reach.
    fn file_image(conn: &Connection, rel: &str) -> Vec<String> {
        let rows = logical(conn);
        let fid: i64 = conn
            .query_row("SELECT file_id FROM _source WHERE id = ?1", [rel], |r| {
                r.get(0)
            })
            .unwrap();
        let (lo, hi) = leyline_schema::file_nid_range(fid);
        let hashes: BTreeSet<String> = reachable_hashes(conn, OCCURRENCES, lo, hi)
            .into_iter()
            .map(hex::encode)
            .collect();
        rows.into_iter()
            .filter(|(table, file, text)| {
                file.as_deref() == Some(rel)
                    || (table == "node_content"
                        && hashes
                            .iter()
                            .any(|h| text.starts_with(&format!("node_hash={h} "))))
                    || (table == "node_child"
                        && hashes
                            .iter()
                            .any(|h| text.starts_with(&format!("parent_hash={h} "))))
            })
            .map(|(table, _, text)| format!("{table}: {text}"))
            .collect()
    }

    fn rebuilt(from: &Connection, grant: &Grant) -> (Connection, Rebuilt) {
        let into = Connection::open_in_memory().unwrap();
        let r = rebuild(from, &into, grant).unwrap();
        (into, r)
    }

    /// F3, the cold-parse half: a rebuild reproduces the logical row set.
    #[test]
    fn a_rebuild_reproduces_a_cold_parsed_arena() {
        let a = arena(FILES);
        let (into, r) = rebuilt(&a.conn, &Grant::Whole);
        assert_eq!(r.files, 3);
        assert_same_rows(&logical(&into), &logical(&a.conn));
        assert_eq!(
            DerivedFrom::read(&into).unwrap(),
            DerivedFrom::read(&a.conn).unwrap()
        );
    }

    /// F3 in its stated form: cold parse, writes through D3, rebuild into a
    /// fresh arena; the logical row sets, `treeRoot` and `derived_from`
    /// agree.
    #[test]
    fn a_rebuild_after_writes_reproduces_the_written_arena() {
        let a = arena(FILES);
        write(&a.conn, &Grant::Whole, "a.go", A2.as_bytes().to_vec()).unwrap();
        let w = write(
            &a.conn,
            &Grant::Whole,
            "sub/b.go",
            b"package b\n\nfunc D() bool { return true }\n".to_vec(),
        )
        .unwrap();
        let (into, r) = rebuilt(&a.conn, &Grant::Whole);
        assert_eq!(r.derived_from.tree_root, w.tree_root);
        assert_same_rows(&logical(&into), &logical(&a.conn));
        assert_eq!(
            DerivedFrom::read(&into).unwrap(),
            DerivedFrom::read(&a.conn).unwrap()
        );
    }

    /// F5: a sub-manifest rebuilt on its own produces the same file-local
    /// row images the full arena holds, compared modulo `file_id`.
    #[test]
    fn a_sub_manifest_rebuild_matches_the_full_arenas_files() {
        let a = arena(FILES);
        let sub = Grant::Paths(["sub/deep/r.rs".to_string()].into());
        let (into, r) = rebuilt(&a.conn, &sub);
        assert_eq!(r.files, 1);
        let only: Vec<String> = into
            .prepare("SELECT id FROM _source")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(only, vec!["sub/deep/r.rs".to_string()]);
        let image = file_image(&into, "sub/deep/r.rs");
        assert!(image.iter().any(|row| row.starts_with("_ast: ")));
        assert_eq!(image, file_image(&a.conn, "sub/deep/r.rs"));
        // The subset's own tree root, not the whole arena's.
        assert_ne!(
            r.derived_from.tree_root,
            DerivedFrom::read(&a.conn).unwrap().unwrap().tree_root
        );
        assert_eq!(
            r.derived_from.tree_root,
            crate::tree_root::tree_root(&into, r.derived_from.parser_id).unwrap()
        );
    }

    /// Every parse stamps `derived_from`, and it names what the head names.
    #[test]
    fn derived_from_matches_the_head() {
        let a = arena(FILES);
        let stamp = DerivedFrom::read(&a.conn).unwrap().unwrap();
        assert_eq!(
            stamp.projection_schema_version,
            crate::daemon::version::PROJECTION_SCHEMA_VERSION
        );
        match crate::tree_root::verify(&a.db).unwrap() {
            crate::tree_root::Verification::Valid {
                tree_root,
                parser_id,
            } => {
                assert_eq!(stamp.tree_root, tree_root);
                assert_eq!(stamp.parser_id, parser_id);
            }
            other => panic!("head does not verify: {other:?}"),
        }
        // A write restamps it with the tree the head now names.
        let w = write(&a.conn, &Grant::Whole, "a.go", A2.as_bytes().to_vec()).unwrap();
        assert_eq!(
            DerivedFrom::read(&a.conn).unwrap().unwrap().tree_root,
            w.tree_root
        );
        assert_ne!(w.tree_root, stamp.tree_root);
    }

    #[test]
    fn derived_from_round_trips_and_rejects_malformed_values() {
        let d = DerivedFrom {
            tree_root: leyline_core::Hash::from_bytes([7; 32]),
            parser_id: leyline_core::Hash::from_bytes([9; 32]),
            projection_schema_version: "projection-v6".into(),
        };
        assert_eq!(DerivedFrom::from_meta(&d.to_meta()).unwrap(), d);
        let hex7 = "07".repeat(32);
        for bad in [
            String::new(),
            format!("{hex7} {hex7}"),
            format!("{hex7} {hex7} v extra"),
            format!("{} {hex7} v", "07".repeat(31)),
            format!("{hex7} zz{} v", "07".repeat(31)),
        ] {
            assert!(DerivedFrom::from_meta(&bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn a_missing_blob_fails_the_rebuild() {
        let a = arena(FILES);
        a.conn
            .execute(
                "DELETE FROM source_blobs WHERE blob_hash = \
                 (SELECT content_hash FROM _source WHERE id = 'sub/b.go')",
                [],
            )
            .unwrap();
        let into = Connection::open_in_memory().unwrap();
        let err = rebuild(&a.conn, &into, &Grant::Whole).unwrap_err();
        assert!(format!("{err:#}").contains("sub/b.go"), "{err:#}");
        assert!(format!("{err:#}").contains("no such blob"), "{err:#}");
    }

    #[test]
    fn bytes_that_do_not_match_their_hash_fail_the_rebuild() {
        let a = arena(FILES);
        a.conn
            .execute(
                "UPDATE source_blobs SET blob_bytes = X'00' WHERE blob_hash = \
                 (SELECT content_hash FROM _source WHERE id = 'a.go')",
                [],
            )
            .unwrap();
        let into = Connection::open_in_memory().unwrap();
        let err = rebuild(&a.conn, &into, &Grant::Whole).unwrap_err();
        assert!(
            format!("{err:#}").contains("stored bytes hash to"),
            "{err:#}"
        );
    }

    #[test]
    fn an_arena_without_derived_from_is_refused() {
        let a = arena(FILES);
        a.conn
            .execute("DELETE FROM _meta WHERE key = 'derived_from'", [])
            .unwrap();
        let into = Connection::open_in_memory().unwrap();
        let err = rebuild(&a.conn, &into, &Grant::Whole).unwrap_err();
        assert!(
            format!("{err:#}").contains("records no derived_from"),
            "{err:#}"
        );
    }

    /// A different `parserId` is a different derivation: refused, not
    /// rebuilt under this binary's.
    #[test]
    fn a_foreign_parser_id_is_refused() {
        let a = arena(FILES);
        let mut stamp = DerivedFrom::read(&a.conn).unwrap().unwrap();
        stamp.parser_id = leyline_core::Hash::from_bytes([1; 32]);
        leyline_ts::schema::set_meta(&a.conn, "derived_from", &stamp.to_meta()).unwrap();
        let into = Connection::open_in_memory().unwrap();
        let err = rebuild(&a.conn, &into, &Grant::Whole).unwrap_err();
        assert!(
            format!("{err:#}").contains("derived under parserId"),
            "{err:#}"
        );
    }

    /// A `_source` set that no longer folds to the stamped `treeRoot` is not
    /// the tree the stamp names.
    #[test]
    fn a_source_set_that_disagrees_with_its_stamp_is_refused() {
        let a = arena(FILES);
        let mut stamp = DerivedFrom::read(&a.conn).unwrap().unwrap();
        stamp.tree_root = leyline_core::Hash::from_bytes([2; 32]);
        leyline_ts::schema::set_meta(&a.conn, "derived_from", &stamp.to_meta()).unwrap();
        let into = Connection::open_in_memory().unwrap();
        let err = rebuild(&a.conn, &into, &Grant::Whole).unwrap_err();
        assert!(format!("{err:#}").contains("folds to treeRoot"), "{err:#}");
    }

    #[test]
    fn an_unknown_path_and_a_non_empty_target_are_refused() {
        let a = arena(FILES);
        let into = Connection::open_in_memory().unwrap();
        let err = rebuild(
            &a.conn,
            &into,
            &Grant::Paths(["nope.go".to_string()].into()),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no file 'nope.go'"), "{err:#}");

        into.execute_batch("CREATE TABLE t(x)").unwrap();
        let err = rebuild(&a.conn, &into, &Grant::Whole).unwrap_err();
        assert!(
            format!("{err:#}").contains("already holds tables"),
            "{err:#}"
        );
    }

    /// The rebuild reads nothing from disk: with the source tree deleted it
    /// still reproduces the arena.
    #[test]
    fn a_rebuild_needs_no_source_tree() {
        let a = arena(FILES);
        let expected = logical(&a.conn);
        std::fs::remove_dir_all(a.src.path()).unwrap();
        let (into, _) = rebuilt(&a.conn, &Grant::Whole);
        assert_same_rows(&logical(&into), &expected);
    }

    /// Arena-resident query overrides are a `parserId` input; the rebuild
    /// carries them.
    #[test]
    fn query_overrides_are_carried_into_the_rebuild() {
        let a = arena(FILES);
        a.conn
            .execute_batch(
                "INSERT INTO query_blobs (blob_hash, blob_bytes) VALUES (X'AB', X'CD'); \
                 INSERT INTO _queries (lang, kind, blob_hash) VALUES ('go', 'other', X'AB');",
            )
            .unwrap();
        let (into, _) = rebuilt(&a.conn, &Grant::Whole);
        let row: (String, String, Vec<u8>, Vec<u8>) = into
            .query_row(
                "SELECT q.lang, q.kind, q.blob_hash, b.blob_bytes FROM _queries q \
                   JOIN query_blobs b USING (blob_hash)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, ("go".into(), "other".into(), vec![0xAB], vec![0xCD]));
    }

    #[test]
    fn cmd_rebuild_writes_a_new_file_and_refuses_an_existing_one() {
        let a = arena(FILES);
        drop(a.conn);
        let out = a.dir.path().join("rebuilt.db");
        cmd_rebuild(&a.db, &out, vec!["a.go".into()]).unwrap();
        let conn = Connection::open(&out).unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM _source")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(ids, vec!["a.go".to_string()]);
        // The rebuilt arena carries its own head, and it verifies.
        assert!(matches!(
            crate::tree_root::verify(&out).unwrap(),
            crate::tree_root::Verification::Valid { .. }
        ));
        let err = cmd_rebuild(&a.db, &out, Vec::new()).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    }
}
