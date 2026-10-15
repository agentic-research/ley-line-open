//! ADR-0045 T1 — the store's manifest: `_tree`, one entry per path.
//!
//! Every path the store holds is an entry: a file of any bytes with a git
//! mode (regular, executable, symlink whose blob is its target), or a
//! directory. The directory set is closed under parents. `_source` is the
//! projection's registry of the files the parse projects; `_tree` is what
//! the store holds, and `treeRoot` folds it ([`crate::tree_root`]).
//!
//! `mtime` and `size` are the walk's stat cache, as `nodes.mtime`/`size`
//! are for projected files: a file whose stat matches is not read again.
//! They are not identity.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use leyline_core::Hash;
use rusqlite::{Connection, OptionalExtension};

/// DDL of the manifest and of the parse's diagnostics (ADR-0045 T1, T3).
pub const TREE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _tree (
    path      TEXT PRIMARY KEY,
    mode      INTEGER NOT NULL,
    blob_hash BLOB,
    mtime     INTEGER,
    size      INTEGER
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS _parse_diagnostics (
    path       TEXT PRIMARY KEY,
    blob_hash  BLOB NOT NULL,
    projected  INTEGER NOT NULL,
    start_byte INTEGER,
    end_byte   INTEGER,
    start_row  INTEGER,
    start_col  INTEGER,
    message    TEXT NOT NULL
) WITHOUT ROWID;";

/// The git mode of a directory entry.
pub const DIRECTORY_MODE: u32 = 0o040000;

/// A file entry's git mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum Mode {
    /// `0o100644`.
    Regular,
    /// `0o100755`.
    Executable,
    /// `0o120000`: the blob is the link's target.
    Symlink,
}

impl Mode {
    /// The git mode as an integer.
    pub fn git(self) -> u32 {
        match self {
            Mode::Regular => 0o100644,
            Mode::Executable => 0o100755,
            Mode::Symlink => 0o120000,
        }
    }

    /// The file mode a git mode names; `None` for a directory or anything
    /// git does not store.
    pub fn from_git(mode: u32) -> Option<Mode> {
        match mode {
            0o100644 => Some(Mode::Regular),
            0o100755 => Some(Mode::Executable),
            0o120000 => Some(Mode::Symlink),
            _ => None,
        }
    }

    /// The wire spelling: git's six octal digits (`100644`).
    pub fn octal(self) -> String {
        format!("{:o}", self.git())
    }

    /// The mode a wire spelling names.
    pub fn from_octal(text: &str) -> Option<Mode> {
        u32::from_str_radix(text, 8).ok().and_then(Mode::from_git)
    }
}

/// What the store holds at a path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Entry {
    File { mode: Mode, blob: Hash },
    Dir,
}

impl Entry {
    /// The entry's git mode.
    pub fn git_mode(self) -> u32 {
        match self {
            Entry::File { mode, .. } => mode.git(),
            Entry::Dir => DIRECTORY_MODE,
        }
    }

    /// The file's blob; `None` for a directory.
    pub fn blob(self) -> Option<Hash> {
        match self {
            Entry::File { blob, .. } => Some(blob),
            Entry::Dir => None,
        }
    }
}

/// Create `_tree` and `_parse_diagnostics` (idempotent).
pub fn ensure_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(TREE_DDL)
        .context("create _tree and _parse_diagnostics")
}

/// Whether `conn` has a manifest.
pub fn exists(conn: &Connection) -> bool {
    conn.prepare("SELECT 1 FROM _tree LIMIT 1").is_ok()
}

fn decode(path: &str, mode: u32, blob: Option<Vec<u8>>) -> Result<Entry> {
    if mode == DIRECTORY_MODE {
        return Ok(Entry::Dir);
    }
    let Some(mode) = Mode::from_git(mode) else {
        bail!("_tree entry {path:?} has mode {mode:o}, which git does not store");
    };
    let Some(bytes) = blob else {
        bail!("_tree file entry {path:?} has no blob_hash");
    };
    let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!(
            "_tree entry {path:?} blob_hash is {} bytes, expected 32",
            bytes.len()
        )
    })?;
    Ok(Entry::File {
        mode,
        blob: Hash::from_bytes(bytes),
    })
}

/// Every entry, by path. Fails closed on an entry that is not a git entry.
pub fn entries(conn: &Connection) -> Result<BTreeMap<String, Entry>> {
    let mut stmt = conn.prepare_cached("SELECT path, mode, blob_hash FROM _tree")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, Option<Vec<u8>>>(2)?,
        ))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (path, mode, blob) = row?;
        let entry = decode(&path, mode, blob)?;
        out.insert(path, entry);
    }
    Ok(out)
}

/// The entry at `path`, if the store holds one.
pub fn entry(conn: &Connection, path: &str) -> Result<Option<Entry>> {
    let row: Option<(u32, Option<Vec<u8>>)> = conn
        .prepare_cached("SELECT mode, blob_hash FROM _tree WHERE path = ?1")?
        .query_row([path], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    row.map(|(mode, blob)| decode(path, mode, blob)).transpose()
}

/// The paths strictly under `dir`, in path order.
pub fn under(conn: &Connection, dir: &str) -> Result<Vec<(String, Entry)>> {
    let prefix = format!("{dir}/");
    let mut stmt = conn.prepare_cached(
        "SELECT path, mode, blob_hash FROM _tree \
          WHERE substr(path, 1, length(?1)) = ?1 ORDER BY path",
    )?;
    let rows = stmt.query_map([&prefix], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, Option<Vec<u8>>>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (path, mode, blob) = row?;
        let entry = decode(&path, mode, blob)?;
        out.push((path, entry));
    }
    Ok(out)
}

/// Each directory above `path`, outermost first (`a`, `a/b` for `a/b/c`).
pub fn parents(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(i, _)| &path[..i])
}

/// Record a file entry, with the stat the walk saw (`None` for bytes that
/// came from a proposal), and every missing parent directory.
pub fn put_file(
    conn: &Connection,
    path: &str,
    mode: Mode,
    blob: Hash,
    stat: Option<(i64, i64)>,
) -> Result<()> {
    put_parents(conn, path)?;
    conn.prepare_cached(
        "INSERT OR REPLACE INTO _tree (path, mode, blob_hash, mtime, size) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(rusqlite::params![
        path,
        mode.git(),
        blob.as_bytes().as_slice(),
        stat.map(|s| s.0),
        stat.map(|s| s.1),
    ])?;
    Ok(())
}

/// Record a directory entry and every missing parent.
pub fn put_dir(conn: &Connection, path: &str) -> Result<()> {
    put_parents(conn, path)?;
    put_one_dir(conn, path)
}

fn put_one_dir(conn: &Connection, path: &str) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR REPLACE INTO _tree (path, mode, blob_hash, mtime, size) \
         VALUES (?1, ?2, NULL, NULL, NULL)",
    )?
    .execute(rusqlite::params![path, DIRECTORY_MODE])?;
    Ok(())
}

fn put_parents(conn: &Connection, path: &str) -> Result<()> {
    for dir in parents(path) {
        put_one_dir(conn, dir)?;
    }
    Ok(())
}

/// Remove the entry at `path` (its diagnostic with it). Directories above
/// it stay (ADR-0045 T1).
pub fn remove(conn: &Connection, path: &str) -> Result<()> {
    conn.prepare_cached("DELETE FROM _tree WHERE path = ?1")?
        .execute([path])?;
    conn.prepare_cached("DELETE FROM _parse_diagnostics WHERE path = ?1")?
        .execute([path])?;
    Ok(())
}

/// The stat cache of every file entry: `path → (mtime, size)`.
pub fn stats(conn: &Connection) -> Result<std::collections::HashMap<String, (i64, i64)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT path, mtime, size FROM _tree WHERE mtime IS NOT NULL AND size IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The bytes the store holds for `blob`.
pub fn blob_bytes(conn: &Connection, blob: Hash) -> Result<Option<Vec<u8>>> {
    Ok(conn
        .prepare_cached("SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1")?
        .query_row([blob.as_bytes().as_slice()], |r| r.get(0))
        .optional()?)
}

/// A file's diagnostic (ADR-0045 T3): the first error tree-sitter found,
/// or why the file was not projected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub path: String,
    pub blob: Hash,
    /// Whether the file has projection rows (with error nodes).
    pub projected: bool,
    /// `(start_byte, end_byte, start_row, start_col)` of the first `ERROR`
    /// or `MISSING` node; `None` when the file was not projected.
    pub span: Option<(i64, i64, i64, i64)>,
    pub message: String,
}

/// Replace `path`'s diagnostic with `diagnostic` (`None` clears it).
pub fn set_diagnostic(conn: &Connection, path: &str, diagnostic: Option<&Diagnostic>) -> Result<()> {
    conn.prepare_cached("DELETE FROM _parse_diagnostics WHERE path = ?1")?
        .execute([path])?;
    if let Some(d) = diagnostic {
        conn.prepare_cached(
            "INSERT INTO _parse_diagnostics \
             (path, blob_hash, projected, start_byte, end_byte, start_row, start_col, message) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(rusqlite::params![
            d.path,
            d.blob.as_bytes().as_slice(),
            d.projected,
            d.span.map(|s| s.0),
            d.span.map(|s| s.1),
            d.span.map(|s| s.2),
            d.span.map(|s| s.3),
            d.message,
        ])?;
    }
    Ok(())
}

/// The diagnostic recorded for `path`, if any.
pub fn diagnostic(conn: &Connection, path: &str) -> Result<Option<Diagnostic>> {
    conn.prepare_cached(
        "SELECT blob_hash, projected, start_byte, end_byte, start_row, start_col, message \
           FROM _parse_diagnostics WHERE path = ?1",
    )?
    .query_row([path], |r| {
        let blob: Vec<u8> = r.get(0)?;
        let start: Option<i64> = r.get(2)?;
        Ok((
            blob,
            r.get::<_, bool>(1)?,
            start
                .map(|s| -> rusqlite::Result<_> { Ok((s, r.get(3)?, r.get(4)?, r.get(5)?)) })
                .transpose()?,
            r.get::<_, String>(6)?,
        ))
    })
    .optional()?
    .map(|(blob, projected, span, message)| {
        let blob: [u8; 32] = blob
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("_parse_diagnostics {path:?} blob_hash is not 32 bytes"))?;
        Ok(Diagnostic {
            path: path.to_string(),
            blob: Hash::from_bytes(blob),
            projected,
            span,
            message,
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ensure_tables(&c).unwrap();
        c
    }

    #[test]
    fn modes_spell_as_git_spells_them() {
        assert_eq!(Mode::Regular.octal(), "100644");
        assert_eq!(Mode::Executable.octal(), "100755");
        assert_eq!(Mode::Symlink.octal(), "120000");
        for m in [Mode::Regular, Mode::Executable, Mode::Symlink] {
            assert_eq!(Mode::from_octal(&m.octal()), Some(m));
            assert_eq!(Mode::from_git(m.git()), Some(m));
        }
        assert_eq!(Mode::from_octal("040000"), None, "a directory is not a file mode");
        assert_eq!(Mode::from_octal("100600"), None);
        assert_eq!(Mode::from_octal("x"), None);
        assert_eq!(Entry::Dir.git_mode(), 0o040000);
    }

    #[test]
    fn a_file_brings_its_parent_directories_and_a_removal_leaves_them() {
        let c = conn();
        let h = Hash::from_bytes([7; 32]);
        put_file(&c, "a/b/c.go", Mode::Executable, h, Some((1, 2))).unwrap();
        let all = entries(&c).unwrap();
        assert_eq!(
            all.into_iter().collect::<Vec<_>>(),
            vec![
                ("a".to_string(), Entry::Dir),
                ("a/b".to_string(), Entry::Dir),
                (
                    "a/b/c.go".to_string(),
                    Entry::File {
                        mode: Mode::Executable,
                        blob: h
                    }
                ),
            ]
        );
        assert_eq!(stats(&c).unwrap().get("a/b/c.go"), Some(&(1, 2)));
        remove(&c, "a/b/c.go").unwrap();
        assert_eq!(entry(&c, "a/b").unwrap(), Some(Entry::Dir));
        assert_eq!(entry(&c, "a/b/c.go").unwrap(), None);
        assert_eq!(under(&c, "a").unwrap(), vec![("a/b".to_string(), Entry::Dir)]);
    }

    #[test]
    fn under_matches_only_the_directory_and_not_a_sibling_prefix() {
        let c = conn();
        let h = Hash::from_bytes([1; 32]);
        put_file(&c, "a/x", Mode::Regular, h, None).unwrap();
        put_file(&c, "a-b", Mode::Regular, h, None).unwrap();
        put_file(&c, "ab/y", Mode::Regular, h, None).unwrap();
        let paths: Vec<String> = under(&c, "a").unwrap().into_iter().map(|e| e.0).collect();
        assert_eq!(paths, ["a/x"]);
    }

    #[test]
    fn an_entry_git_does_not_store_fails_closed() {
        let c = conn();
        c.execute_batch(
            "INSERT INTO _tree (path, mode, blob_hash) VALUES ('x', 33188, NULL);",
        )
        .unwrap();
        let err = entries(&c).unwrap_err().to_string();
        assert!(err.contains("no blob_hash"), "{err}");
        c.execute_batch("UPDATE _tree SET mode = 33152, blob_hash = zeroblob(32);")
            .unwrap();
        let err = entry(&c, "x").unwrap_err().to_string();
        assert!(err.contains("git does not store"), "{err}");
        c.execute_batch("UPDATE _tree SET mode = 33188, blob_hash = X'01';")
            .unwrap();
        let err = entry(&c, "x").unwrap_err().to_string();
        assert!(err.contains("expected 32"), "{err}");
    }

    #[test]
    fn a_diagnostic_round_trips_and_clears() {
        let c = conn();
        let d = Diagnostic {
            path: "a.go".into(),
            blob: Hash::from_bytes([3; 32]),
            projected: true,
            span: Some((4, 9, 1, 2)),
            message: "syntax error".into(),
        };
        set_diagnostic(&c, "a.go", Some(&d)).unwrap();
        assert_eq!(diagnostic(&c, "a.go").unwrap(), Some(d.clone()));
        let unprojected = Diagnostic {
            projected: false,
            span: None,
            ..d
        };
        set_diagnostic(&c, "a.go", Some(&unprojected)).unwrap();
        assert_eq!(diagnostic(&c, "a.go").unwrap(), Some(unprojected));
        set_diagnostic(&c, "a.go", None).unwrap();
        assert_eq!(diagnostic(&c, "a.go").unwrap(), None);
    }
}
