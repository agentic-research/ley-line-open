//! ADR-0040 D3 — the one write path (bead `ley-line-open-f2ffbd`), and the
//! proposal it is the one-file case of (ADR-0041 P1-P2).
//!
//! `propose(grant, writes)` is the only way a file's bytes change in an
//! arena; `write(grant, path, bytes)` is its one-file, unchecked case. A
//! proposal is refused whole before anything is touched when a path is
//! outside the grant, a file moved since its author read it (its
//! [`Base`]), or a file does not parse; otherwise every file is handed to
//! the cold parse in one pass and one transaction, so the proposal lands
//! exactly what a cold parse of the same bytes lands:
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

/// What a proposal's author read of a file, checked before the proposal is
/// applied (ADR-0041 P2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Base {
    /// No check: the write replaces whatever the file holds. What a writer
    /// that read nothing first (a splice of known bytes) uses.
    Any,
    /// The `source_blobs` hash of the file's bytes when the author read
    /// them. The proposal is refused unless the file still holds them.
    Blob(Hash),
}

/// One file a proposal writes: its `_source.id`, the base its author read,
/// and its new bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proposed {
    pub path: String,
    pub base: Base,
    pub bytes: Vec<u8>,
}

/// A proposal refused because a file it writes moved since its author read
/// it. Typed so a caller can map it to `ESTALE`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stale {
    pub path: String,
    /// The blob the author read.
    pub expected: Hash,
    /// The blob the file holds now.
    pub current: Hash,
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "proposal refused: '{}' moved since it was read (read blob {}, now {})",
            self.path, self.expected, self.current
        )
    }
}

impl std::error::Error for Stale {}

/// What an accepted proposal produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Accepted {
    /// Each written path's new `source_blobs` key, now its `_source.content_hash`.
    pub blobs: std::collections::BTreeMap<String, Hash>,
    /// The arena's `treeRoot` after the proposal.
    pub tree_root: Hash,
}

/// Apply `writes` as one change: every file lands, in one transaction, or
/// none does (ADR-0041 P1).
///
/// Refused, with no change to the arena, when any path is outside `grant`,
/// names no `_source` row, or appears twice; when the arena does not record
/// where a file's tree is rooted, or the files are under different roots;
/// when any base no longer matches the file ([`Stale`]); or when any file's
/// bytes do not parse cleanly in its language. Any failure inside the
/// projection rolls the whole proposal back.
///
/// Bases are checked before the projection's transaction opens, on the
/// writer's connection. They hold against every other writer because the
/// arena has one writer: the daemon's writer lock, or the one process that
/// owns a file-backed database.
pub fn propose(conn: &Connection, grant: &Grant, writes: Vec<Proposed>) -> Result<Accepted> {
    use leyline_core::ContentAddressed;
    if writes.is_empty() {
        bail!("proposal refused: it writes nothing");
    }
    for w in &writes {
        if !grant.covers(&w.path) {
            bail!(
                "write to '{}' refused: the path is outside the grant",
                w.path
            );
        }
    }

    let mut root: Option<PathBuf> = None;
    let mut files = std::collections::BTreeMap::new();
    let mut blobs = std::collections::BTreeMap::new();
    for w in writes {
        let path = w.path;
        let (language, abs_path, held): (String, Option<String>, Option<Vec<u8>>) = conn
            .query_row(
                "SELECT language, path, content_hash FROM _source WHERE id = ?1",
                [&path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .with_context(|| format!("write to '{path}' refused: no such file in this arena"))?;
        if let Base::Blob(expected) = w.base {
            let current = held
                .and_then(|h| <[u8; 32]>::try_from(h).ok())
                .map(Hash::from_bytes)
                .unwrap_or(Hash::ZERO);
            if current != expected {
                return Err(Stale {
                    path,
                    expected,
                    current,
                }
                .into());
            }
        }
        let abs_path = abs_path.with_context(|| {
            format!(
                "write to '{path}' refused: the arena records no tree root for it \
                 (it was not produced by `leyline parse` or the daemon)"
            )
        })?;
        let file_root = tree_root_dir(Path::new(&abs_path), &path)?;
        match &root {
            None => root = Some(file_root),
            Some(r) if *r != file_root => bail!(
                "proposal refused: '{path}' is under {}, not the proposal's tree {}",
                file_root.display(),
                r.display()
            ),
            Some(_) => {}
        }
        let lang = leyline_ts::languages::TsLanguage::from_name(&language)?;
        leyline_ts::splice::check_syntax(&w.bytes, lang.ts_language())
            .with_context(|| format!("write to '{path}' refused"))?;
        blobs.insert(path.clone(), w.bytes.as_slice().hash());
        if files.insert(path.clone(), w.bytes).is_some() {
            bail!("proposal refused: it writes '{path}' twice");
        }
    }

    let root = root.expect("a non-empty proposal names a root");
    let result = crate::cmd_parse::parse_written_files(conn, &root, &files)?;
    Ok(Accepted {
        blobs,
        tree_root: result.tree_root,
    })
}

/// Write `bytes` as the content of the file `path` names, whatever it holds:
/// a one-file [`propose`] with [`Base::Any`].
pub fn write(conn: &Connection, grant: &Grant, path: &str, bytes: Vec<u8>) -> Result<Written> {
    let accepted = propose(
        conn,
        grant,
        vec![Proposed {
            path: path.to_string(),
            base: Base::Any,
            bytes,
        }],
    )?;
    Ok(Written {
        source_blob_hash: accepted.blobs[path],
        tree_root: accepted.tree_root,
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
    /// A one-file [`propose`] against the base the mount took when the edit
    /// began; a file that moved since is refused with the mount's
    /// [`leyline_fs::graph::StaleWrite`].
    fn write(
        &self,
        conn: &Connection,
        source_id: &str,
        bytes: Vec<u8>,
        base: Option<Hash>,
    ) -> Result<()> {
        let proposed = Proposed {
            path: source_id.to_string(),
            base: base.map_or(Base::Any, Base::Blob),
            bytes,
        };
        match propose(conn, &self.grant, vec![proposed]) {
            Ok(_) => Ok(()),
            Err(err) => match err.downcast::<Stale>() {
                Ok(stale) => Err(leyline_fs::graph::StaleWrite {
                    source_id: stale.path,
                    expected: stale.expected,
                    current: stale.current,
                }
                .into()),
                Err(other) => Err(other),
            },
        }
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
        let err = crate::cmd_parse::parse_written_files(
            &a.conn,
            a._src.path(),
            &std::collections::BTreeMap::from([(
                "a.go".to_string(),
                b"package a\n\n// \0\n".to_vec(),
            )]),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("binary file"), "{err:#}");
        assert_eq!(image(&a.conn, "a.go"), before);
        assert_eq!(
            leyline_ts::splice::source_bytes(&a.conn, file_id(&a.conn, "a.go")).unwrap(),
            A.as_bytes(),
        );
    }

    /// A write is one transaction: its rows, the sweep and the `_meta`
    /// stamps (`derived_from` among them) commit together or not at all.
    /// The stamps used to run after `COMMIT`, so a reader of the WAL saw
    /// the new `_source` under the old `derived_from`, and a failure while
    /// stamping left them disagreeing for good. The failure is injected
    /// with a trigger on the `derived_from` stamp, the last step.
    #[test]
    fn a_write_that_fails_after_its_rows_leaves_the_arena_unchanged() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        a.conn
            .execute_batch(
                "CREATE TRIGGER refuse_derived_from BEFORE INSERT ON _meta
                   WHEN NEW.key = 'derived_from'
                 BEGIN SELECT RAISE(ABORT, 'injected stamp failure'); END;",
            )
            .unwrap();
        let before = whole_image(&a.conn);

        let err = write(&a.conn, &Grant::Whole, "a.go", A2.as_bytes().to_vec()).unwrap_err();
        assert!(
            format!("{err:#}").contains("injected stamp failure"),
            "{err:#}"
        );
        assert!(
            whole_image(&a.conn) == before,
            "nothing of a failed write is committed"
        );
        assert_eq!(
            leyline_ts::splice::source_bytes(&a.conn, file_id(&a.conn, "a.go")).unwrap(),
            A.as_bytes(),
        );
    }

    const B2: &str = "package a\n\nfunc C() string { return \"see\" }\n";

    fn proposed(path: &str, base: Base, text: &str) -> Proposed {
        Proposed {
            path: path.to_string(),
            base,
            bytes: text.as_bytes().to_vec(),
        }
    }

    fn blob(text: &str) -> Base {
        Base::Blob(text.as_bytes().hash())
    }

    /// A proposal lands every file it names in one pass, exactly as a cold
    /// parse of the resulting tree.
    #[test]
    fn a_proposal_lands_all_its_files_as_a_cold_parse() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let w = propose(
            &a.conn,
            &Grant::Whole,
            vec![proposed("a.go", blob(A), A2), proposed("b.go", blob(B), B2)],
        )
        .unwrap();
        let cold = arena(&[("a.go", A2), ("b.go", B2)]);
        assert_eq!(image(&a.conn, "a.go"), image(&cold.conn, "a.go"));
        assert_eq!(image(&a.conn, "b.go"), image(&cold.conn, "b.go"));
        let crate::tree_root::Verification::Valid {
            tree_root: cold_root,
            ..
        } = crate::tree_root::verify(&cold.db).unwrap()
        else {
            panic!("a cold parse stamps a valid head");
        };
        assert_eq!(w.tree_root, cold_root);
    }

    /// A proposal is applied whole or not at all: one file that does not
    /// parse refuses the others.
    #[test]
    fn a_proposal_with_one_invalid_file_changes_nothing() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let before = whole_image(&a.conn);
        let err = propose(
            &a.conn,
            &Grant::Whole,
            vec![
                proposed("a.go", blob(A), A2),
                proposed("b.go", blob(B), "package a\nfunc ("),
            ],
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("b.go"), "{err:#}");
        assert!(
            whole_image(&a.conn) == before,
            "nothing of the proposal lands"
        );
    }

    /// A base is the blob its author read. When the file has moved since,
    /// the proposal is refused, says which file moved and to what, and
    /// changes nothing: a stale overwrite never wins.
    #[test]
    fn a_stale_base_is_refused_and_names_what_moved() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        write(&a.conn, &Grant::Whole, "a.go", A2.as_bytes().to_vec()).unwrap();
        let before = whole_image(&a.conn);

        let err = propose(
            &a.conn,
            &Grant::Whole,
            vec![
                proposed("a.go", blob(A), "package a\n\nfunc A() int { return 3 }\n"),
                proposed("b.go", blob(B), B2),
            ],
        )
        .unwrap_err();
        let stale = err.downcast_ref::<Stale>().expect("a typed stale refusal");
        assert_eq!(
            stale,
            &Stale {
                path: "a.go".to_string(),
                expected: A.as_bytes().hash(),
                current: A2.as_bytes().hash(),
            }
        );
        assert!(
            whole_image(&a.conn) == before,
            "nothing of the proposal lands"
        );
        let message = err.to_string();
        assert!(
            message.contains("'a.go' moved since it was read")
                && message.contains(&A.as_bytes().hash().to_string())
                && message.contains(&A2.as_bytes().hash().to_string()),
            "the refusal names the file and both blobs: {message}"
        );
    }

    /// One proposal is one tree: files recorded under different roots are
    /// refused together, since one pass projects one tree.
    #[test]
    fn a_proposal_spanning_two_trees_is_refused() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        a.conn
            .execute(
                "UPDATE _source SET path = '/elsewhere/b.go' WHERE id = 'b.go'",
                [],
            )
            .unwrap();
        let before = whole_image(&a.conn);
        let err = propose(
            &a.conn,
            &Grant::Whole,
            vec![proposed("a.go", blob(A), A2), proposed("b.go", blob(B), B2)],
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not the proposal's tree"),
            "{err:#}"
        );
        assert!(
            whole_image(&a.conn) == before,
            "nothing of the proposal lands"
        );
    }

    /// Every path of a proposal must be inside the grant, or none lands.
    #[test]
    fn a_proposal_reaching_outside_its_grant_changes_nothing() {
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let before = whole_image(&a.conn);
        let grant = Grant::Paths(BTreeSet::from(["a.go".to_string()]));
        let err = propose(
            &a.conn,
            &grant,
            vec![proposed("a.go", blob(A), A2), proposed("b.go", blob(B), B2)],
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("outside the grant"), "{err:#}");
        assert!(
            whole_image(&a.conn) == before,
            "nothing of the proposal lands"
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
            .write(&a.conn, "a.go", A2.as_bytes().to_vec(), None)
            .unwrap_err();
        assert!(format!("{err:#}").contains("outside the grant"), "{err:#}");

        let whole = MountWriter {
            grant: Grant::Whole,
        };
        whole
            .write(&a.conn, "a.go", A2.as_bytes().to_vec(), None)
            .unwrap();
        assert_eq!(
            leyline_ts::splice::source_bytes(&a.conn, file_id(&a.conn, "a.go")).unwrap(),
            A2.as_bytes(),
        );
    }

    /// The mount's writer checks the base the mount hands it, and answers a
    /// file that moved with the mount's own stale refusal, so the mount can
    /// reply `ESTALE`.
    #[cfg(feature = "mount")]
    #[test]
    fn the_mount_writer_refuses_a_moved_file_as_stale() {
        use leyline_fs::graph::{SourceWriter, StaleWrite};
        let a = arena(&[("a.go", A), ("b.go", B)]);
        let writer = MountWriter {
            grant: Grant::Whole,
        };
        writer
            .write(
                &a.conn,
                "a.go",
                A2.as_bytes().to_vec(),
                Some(A.as_bytes().hash()),
            )
            .unwrap();
        let err = writer
            .write(
                &a.conn,
                "a.go",
                b"package a\n\nfunc A() int { return 9 }\n".to_vec(),
                Some(A.as_bytes().hash()),
            )
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<StaleWrite>(),
            Some(&StaleWrite {
                source_id: "a.go".to_string(),
                expected: A.as_bytes().hash(),
                current: A2.as_bytes().hash(),
            })
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
