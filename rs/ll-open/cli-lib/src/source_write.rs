//! ADR-0040 D3 — the one write path (bead `ley-line-open-f2ffbd`).
//!
//! `write(grant, path, bytes)` is the only way a file's bytes change in an
//! arena. It refuses a path outside the grant before touching anything, then
//! hands the bytes to the cold parse as that one file's content, so the
//! write lands exactly what a cold parse of the same bytes lands:
//!
//! - the bytes in `source_blobs` under their hash, and `_source.content_hash`
//!   naming that row (bead `0d3b72`);
//! - `_ast` rows with `node_hash`, their `node_content` / `node_child` rows,
//!   `node_defs` / `node_refs`, and the `_ast_blob` pointer (bead `143f17`);
//! - `treeRoot` recomputed from the committed `_source` set and stamped into
//!   the sibling head when the arena is file-backed (D1).
//!
//! Publication stays with the caller: the daemon's mount publishes on
//! `fsync`, `leyline serve` flushes its image to the arena, and `leyline
//! splice` writes a file-backed database in place.
//!
//! The bytes land in the arena only. A later full-tree parse that reads the
//! file from disk replaces them with the disk bytes, as it replaces any other
//! arena state that disagrees with the working tree.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use leyline_core::Hash;
use rusqlite::{Connection, OptionalExtension};

/// The paths a writer may change: ADR-0029's sub-manifest of paths, the
/// ring-2 slice grant. `Whole` is the grant the arena's owner holds — the
/// daemon over its own source tree, `leyline serve` over its own arena, and
/// `leyline splice` over a database the caller names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Grant {
    Whole,
    Paths(BTreeSet<String>),
}

impl Grant {
    /// Whether `path` (a `_source.id`, the file's path relative to the
    /// tree root) is inside this grant.
    pub fn covers(&self, path: &str) -> bool {
        match self {
            Grant::Whole => true,
            Grant::Paths(paths) => paths.contains(path),
        }
    }
}

/// What a write produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Written {
    /// The `source_blobs` key of the bytes, now `_source.content_hash`.
    pub source_blob_hash: Hash,
    /// The arena's `treeRoot` after the write.
    pub tree_root: Hash,
}

/// Write `bytes` as the content of the file `path` names.
///
/// Refused, with no change to the arena, when `path` is outside `grant`,
/// when no `_source` row names it, when the arena does not record where the
/// file's tree is rooted, or when the bytes do not parse cleanly in the
/// file's language. Any failure inside the projection rolls the whole write
/// back.
pub fn write(conn: &Connection, grant: &Grant, path: &str, bytes: Vec<u8>) -> Result<Written> {
    if !grant.covers(path) {
        bail!("write to '{path}' refused: the path is outside the grant");
    }
    let (language, abs_path): (String, Option<String>) = conn
        .query_row(
            "SELECT language, path FROM _source WHERE id = ?1",
            [path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .with_context(|| format!("write to '{path}' refused: no such file in this arena"))?;
    let abs_path = abs_path.with_context(|| {
        format!(
            "write to '{path}' refused: the arena records no tree root for it \
             (it was not produced by `leyline parse` or the daemon)"
        )
    })?;
    let root = tree_root_dir(Path::new(&abs_path), path)?;
    let lang = leyline_ts::languages::TsLanguage::from_name(&language)?;
    leyline_ts::splice::check_syntax(&bytes, lang.ts_language())
        .with_context(|| format!("write to '{path}' refused"))?;

    let source_blob_hash = {
        use leyline_core::ContentAddressed;
        bytes.as_slice().hash()
    };
    let result = crate::cmd_parse::parse_written_file(
        conn,
        &root,
        crate::cmd_parse::WrittenFile {
            rel: path.to_string(),
            bytes,
        },
    )?;
    Ok(Written {
        source_blob_hash,
        tree_root: result.tree_root,
    })
}

/// A mount's handle on [`write`]: the FUSE/NFS mount computes an edited
/// file's bytes and hands them here, under the grant the mount was built
/// with.
#[cfg(feature = "mount")]
pub struct MountWriter {
    pub grant: Grant,
}

#[cfg(feature = "mount")]
impl leyline_fs::graph::SourceWriter for MountWriter {
    fn write(&self, conn: &Connection, source_id: &str, bytes: Vec<u8>) -> Result<()> {
        write(conn, &self.grant, source_id, bytes).map(|_| ())
    }
}

/// The directory `rel` is relative to, given the file's absolute path.
fn tree_root_dir(abs: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    if !abs.ends_with(rel_path) {
        bail!(
            "write to '{rel}' refused: its recorded path {} does not end in it",
            abs.display()
        );
    }
    let depth = rel_path.components().count();
    abs.ancestors()
        .nth(depth)
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "write to '{rel}' refused: no tree root above {}",
                abs.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_core::ContentAddressed;
    use tempfile::TempDir;

    const A: &str = "package a\n\nfunc A() int { return 1 }\n";
    const A2: &str = "package a\n\nfunc A() int { return 2 }\n\nfunc B(x int) int { return x }\n";
    const B: &str = "package a\n\nfunc C() string { return \"c\" }\n";

    /// A cold-parsed, file-backed arena over `files`.
    struct Arena {
        _src: TempDir,
        _dir: TempDir,
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
        Arena {
            _src: src,
            _dir: dir,
            db,
            conn,
        }
    }

    fn file_id(conn: &Connection, rel: &str) -> i64 {
        conn.query_row("SELECT file_id FROM _source WHERE id = ?1", [rel], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// Every row a cold parse writes for one file, with the arena-local
    /// `file_id` masked out of every nid (ADR-0040 F5).
    #[derive(Debug, PartialEq)]
    struct FileImage {
        ast: Vec<(i64, String, i64, i64, i64, i64, i64, i64, Option<Vec<u8>>)>,
        content: Vec<(Vec<u8>, String, String, Option<String>, Option<i64>)>,
        children: Vec<(Vec<u8>, i64, Vec<u8>, Option<String>)>,
        defs: Vec<(String, i64, Option<Vec<u8>>)>,
        refs: Vec<(String, i64, Option<Vec<u8>>)>,
        ast_blob_resolves: bool,
    }

    fn rows<T, F>(conn: &Connection, sql: &str, lo: i64, hi: i64, f: F) -> Vec<T>
    where
        F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([lo, hi], f)
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn image(conn: &Connection, rel: &str) -> FileImage {
        let fid = file_id(conn, rel);
        let (lo, hi) = leyline_schema::file_nid_range(fid);
        let mask = leyline_schema::file_nid(1, 0) - 1;
        let ast = rows(
            conn,
            "SELECT a.nid, k.raw_kind, a.start_byte, a.end_byte, a.start_row, a.start_col, \
                    a.end_row, a.end_col, a.node_hash \
               FROM _ast a JOIN kinds k ON k.kind_id = a.kind_id \
              WHERE a.nid BETWEEN ?1 AND ?2 ORDER BY a.nid",
            lo,
            hi,
            |r| {
                Ok((
                    r.get::<_, i64>(0)? & mask,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            },
        );
        let content = rows(
            conn,
            "SELECT node_hash, kind, raw_kind, token, arity FROM node_content \
              WHERE node_hash IN (SELECT node_hash FROM _ast WHERE nid BETWEEN ?1 AND ?2) \
              ORDER BY node_hash",
            lo,
            hi,
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        );
        let children = rows(
            conn,
            "SELECT parent_hash, ordinal, child_hash, field FROM node_child \
              WHERE parent_hash IN (SELECT node_hash FROM _ast WHERE nid BETWEEN ?1 AND ?2) \
              ORDER BY parent_hash, ordinal",
            lo,
            hi,
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        );
        let defs = rows(
            conn,
            "SELECT token, nid, node_hash FROM node_defs WHERE nid BETWEEN ?1 AND ?2 \
              ORDER BY nid, token",
            lo,
            hi,
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? & mask, r.get(2)?)),
        );
        let refs = rows(
            conn,
            "SELECT token, nid, node_hash FROM node_refs WHERE nid BETWEEN ?1 AND ?2 \
              ORDER BY nid, token",
            lo,
            hi,
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? & mask, r.get(2)?)),
        );
        let ast_blob_resolves = conn
            .query_row(
                "SELECT count(*) FROM _ast_blob b JOIN capnp_blobs c ON c.blob_hash = b.blob_hash \
                  WHERE b.file_id = ?1",
                [fid],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 1;
        FileImage {
            ast,
            content,
            children,
            defs,
            refs,
            ast_blob_resolves,
        }
    }

    fn whole_image(conn: &Connection) -> Vec<u8> {
        conn.serialize("main").unwrap().to_vec()
    }

    /// Bead `0d3b72`: the hash `_source.content_hash` names after a write is
    /// a `source_blobs` row holding exactly the written bytes.
    #[test]
    fn a_write_stores_its_bytes_in_source_blobs_under_their_hash() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let w = write(&a.conn, &Grant::Whole, "a.go", A2.as_bytes().to_vec()).unwrap();

        let expected = A2.as_bytes().hash();
        assert_eq!(w.source_blob_hash, expected);
        let stored: Vec<u8> = a
            .conn
            .query_row(
                "SELECT content_hash FROM _source WHERE id = 'a.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, expected.as_bytes().to_vec());
        let blob: Vec<u8> = a
            .conn
            .query_row(
                "SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1",
                [&stored],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(blob, A2.as_bytes());
        assert_eq!(
            leyline_ts::splice::source_bytes(&a.conn, file_id(&a.conn, "a.go")).unwrap(),
            A2.as_bytes(),
        );
    }

    /// Bead `143f17`: a written file's rows are the rows a cold parse of the
    /// same bytes writes — `node_hash` on `_ast`, the `node_content` and
    /// `node_child` rows, defs and refs, and a resolving `_ast_blob`.
    #[test]
    fn a_write_projects_the_file_exactly_as_a_cold_parse_of_the_same_bytes() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        write(&a.conn, &Grant::Whole, "a.go", A2.as_bytes().to_vec()).unwrap();
        let cold = arena(&[("a.go", A2), ("b.go", B)]);

        let written = image(&a.conn, "a.go");
        assert!(
            written.ast.iter().any(|row| row.8.is_some()),
            "the written file's _ast rows carry node_hash",
        );
        assert!(!written.content.is_empty() && !written.children.is_empty());
        assert!(
            written.defs.iter().any(|d| d.0 == "B"),
            "the new def is extracted"
        );
        assert!(written.ast_blob_resolves);
        assert_eq!(written, image(&cold.conn, "a.go"));
        assert_eq!(image(&a.conn, "b.go"), image(&cold.conn, "b.go"));
    }

    /// D1 through D3: the head's `treeRoot` moves with the write and still
    /// verifies against the arena; it is the cold parse's root for the same
    /// tree.
    #[test]
    fn a_write_restamps_the_heads_tree_root() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let crate::tree_root::Verification::Valid {
            tree_root: before, ..
        } = crate::tree_root::verify(&a.db).unwrap()
        else {
            panic!("a cold parse stamps a valid head");
        };
        let w = write(&a.conn, &Grant::Whole, "a.go", A2.as_bytes().to_vec()).unwrap();
        let crate::tree_root::Verification::Valid {
            tree_root: after, ..
        } = crate::tree_root::verify(&a.db).unwrap()
        else {
            panic!("the head verifies after a write");
        };
        assert_ne!(before, after);
        assert_eq!(w.tree_root, after);
        let cold = arena(&[("a.go", A2), ("b.go", B)]);
        let crate::tree_root::Verification::Valid {
            tree_root: cold_root,
            ..
        } = crate::tree_root::verify(&cold.db).unwrap()
        else {
            panic!("a cold parse stamps a valid head");
        };
        assert_eq!(after, cold_root);
    }

    /// ADR-0040 F6: a write outside the grant is refused and changes no byte
    /// of the arena.
    #[test]
    fn a_write_outside_the_grant_is_refused_with_no_side_effect() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let before = whole_image(&a.conn);
        let grant = Grant::Paths(BTreeSet::from(["b.go".to_string()]));
        let err = write(&a.conn, &grant, "a.go", A2.as_bytes().to_vec()).unwrap_err();
        assert!(format!("{err:#}").contains("outside the grant"), "{err:#}");
        assert_eq!(whole_image(&a.conn), before);

        write(&a.conn, &grant, "b.go", A2.as_bytes().to_vec()).unwrap();
        assert_ne!(
            whole_image(&a.conn),
            before,
            "a path inside the grant is written"
        );
    }

    #[test]
    fn a_write_that_does_not_parse_is_refused_with_no_side_effect() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let before = whole_image(&a.conn);
        let err = write(
            &a.conn,
            &Grant::Whole,
            "a.go",
            b"package a\nfunc (".to_vec(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("syntax errors"), "{err:#}");
        assert_eq!(whole_image(&a.conn), before);
    }

    /// A written file the cold parse refuses fails the pass, and the
    /// transaction that already swept the file's rows rolls back: the file
    /// is not left out of the arena. `write` checks syntax first, and every
    /// grammar refuses the NUL byte the parse's binary-file rule rejects, so
    /// the pass is driven directly.
    #[test]
    fn a_written_file_the_cold_parse_refuses_rolls_back_and_keeps_the_file() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let before = image(&a.conn, "a.go");
        let err = crate::cmd_parse::parse_written_file(
            &a.conn,
            a._src.path(),
            crate::cmd_parse::WrittenFile {
                rel: "a.go".to_string(),
                bytes: b"package a\n\n// \0\n".to_vec(),
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("binary file"), "{err:#}");
        assert_eq!(image(&a.conn, "a.go"), before);
        assert_eq!(
            leyline_ts::splice::source_bytes(&a.conn, file_id(&a.conn, "a.go")).unwrap(),
            A.as_bytes(),
        );
    }

    #[test]
    fn a_write_to_a_file_the_arena_does_not_hold_is_refused() {
        let a = arena(&[("a.go", A)]);
        let before = whole_image(&a.conn);
        let err = write(&a.conn, &Grant::Whole, "z.go", A2.as_bytes().to_vec()).unwrap_err();
        assert!(format!("{err:#}").contains("no such file"), "{err:#}");
        assert_eq!(whole_image(&a.conn), before);
    }

    /// The written bytes, not the disk's, are what the arena holds: the
    /// cold parse is handed the write and never reads the file.
    #[test]
    fn a_write_does_not_read_or_touch_the_file_on_disk() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let disk = a._src.path().join("a.go");
        write(&a.conn, &Grant::Whole, "a.go", A2.as_bytes().to_vec()).unwrap();
        assert_eq!(std::fs::read_to_string(&disk).unwrap(), A);
        std::fs::remove_file(&disk).unwrap();
        write(&a.conn, &Grant::Whole, "a.go", B.as_bytes().to_vec()).unwrap();
        assert_eq!(
            leyline_ts::splice::source_bytes(&a.conn, file_id(&a.conn, "a.go")).unwrap(),
            B.as_bytes(),
        );
    }

    /// The mount's writer is `write` under the grant it was built with.
    #[cfg(feature = "mount")]
    #[test]
    fn the_mount_writer_writes_under_its_grant() {
        use leyline_fs::graph::SourceWriter;
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let narrow = MountWriter {
            grant: Grant::Paths(BTreeSet::from(["b.go".to_string()])),
        };
        let err = narrow
            .write(&a.conn, "a.go", A2.as_bytes().to_vec())
            .unwrap_err();
        assert!(format!("{err:#}").contains("outside the grant"), "{err:#}");

        let whole = MountWriter {
            grant: Grant::Whole,
        };
        whole
            .write(&a.conn, "a.go", A2.as_bytes().to_vec())
            .unwrap();
        assert_eq!(
            leyline_ts::splice::source_bytes(&a.conn, file_id(&a.conn, "a.go")).unwrap(),
            A2.as_bytes(),
        );
    }

    #[test]
    fn grant_paths_covers_exactly_its_paths() {
        let g = Grant::Paths(BTreeSet::from(["a/b.go".to_string()]));
        assert!(g.covers("a/b.go"));
        assert!(!g.covers("a/c.go"));
        assert!(!g.covers("a"));
        assert!(Grant::Whole.covers("anything"));
    }

    #[test]
    fn the_tree_root_is_the_directory_above_the_relative_path() {
        assert_eq!(
            tree_root_dir(Path::new("/w/src/pkg/a.go"), "pkg/a.go").unwrap(),
            PathBuf::from("/w/src"),
        );
        assert!(tree_root_dir(Path::new("/w/src/pkg/a.go"), "other/a.go").is_err());
    }
}
