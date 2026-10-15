//! A daemon's checkout of its source directory (bead
//! `ley-line-open-60f39f`; R3 of the proposed ADR-0043, for the one
//! checkout a daemon has).
//!
//! A daemon started with `--source <dir>` checks `<dir>` out. Three things
//! hold a file's bytes, each by blob hash (σ over the bytes, the hash
//! `_source.content_hash` and `source_blobs` use):
//!
//! - the **store**: `_source.content_hash`, which every accepted proposal
//!   advances (ADR-0040 D3, ADR-0042);
//! - the **directory**: the file under `<dir>`, which a person edits;
//! - the **index**: `_checkout_index`, the hash last materialized into the
//!   directory or ingested from it — git's index.
//!
//! Comparing the directory and the store each against the index tells which
//! side moved, path by path. "No file" is a value on every side: a file
//! created on one side moved from no file, and a file deleted on one side
//! moved to no file (bead `ley-line-open-47fb03`).
//!
//! | directory vs index | store vs index | action |
//! |---|---|---|
//! | same | same | nothing |
//! | same | moved | materialize the stored bytes into the directory, or remove the file the store deleted |
//! | changed | same | ingest the directory's bytes, or its deletion |
//! | changed | moved | converged when both hold the same bytes or both lost the file; otherwise a conflict |
//!
//! A conflict overwrites neither side: the store keeps its bytes (or its
//! deletion), the directory keeps its file (or its deletion), and the path
//! is recorded in `_checkout_conflict` until a proposal or an edit makes
//! the two agree. A path neither the store nor the index holds is a file
//! new in the directory: it is ingested.
//!
//! **Ordering.** A proposal commits to the store and leaves the index alone;
//! then each deleted file is removed (unlink, then `fsync` of its
//! directory) and each written file materialized (a temp file in the same
//! directory, `fsync`, `rename`, `fsync` of the directory); then its index
//! entry advances. A crash after the commit leaves directory == index while
//! the store moved, which the next [`reconcile`] carries out. A crash after
//! the rename or the unlink leaves directory == store while the index lags,
//! which converges. No ordering of crashes loses an accepted proposal or a
//! person's edit.
//!
//! Startup, the watcher, `reparse` over the checkout and every accepted
//! proposal run [`reconcile`]. A daemon without a source directory has no
//! checkout: its proposals land in the store and nowhere else.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use leyline_core::{ContentAddressed, Hash};
use rusqlite::{Connection, OptionalExtension};

use crate::cmd_parse::ParseResult;

/// What reconciling one path does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Both sides agree with the index.
    Nothing,
    /// The store moved: write its bytes into the directory.
    Materialize,
    /// The directory changed: parse its bytes into the store.
    Ingest,
    /// Both moved to the same bytes, or both lost the file: the index
    /// follows them.
    Converge,
    /// Both moved, to different bytes: reported, nothing overwritten.
    Conflict,
    /// The store deleted the file: remove it from the directory.
    Remove,
}

/// The action for a path the store holds as `stored`. `disk` is the hash of
/// the directory's file (`None` when the directory has no such file) and
/// `index` the index entry (`None` when there is none).
pub fn classify(disk: Option<Hash>, stored: Hash, index: Option<Hash>) -> Action {
    let disk_moved = disk != index;
    let store_moved = Some(stored) != index;
    match (disk_moved, store_moved) {
        (false, false) => Action::Nothing,
        (false, true) => Action::Materialize,
        (true, false) => Action::Ingest,
        (true, true) if disk == Some(stored) => Action::Converge,
        (true, true) => Action::Conflict,
    }
}

/// The action for a path the store does not hold: [`classify`] with the
/// store's side "no file". An index entry means the two once agreed on a
/// file, so the store deleted it: removed from an unchanged directory, a
/// conflict with a changed one. No index entry and a file on disk is a file
/// new to the directory, ingested.
pub fn classify_unstored(disk: Option<Hash>, index: Option<Hash>) -> Action {
    match (disk, index) {
        (None, None) => Action::Nothing,
        (Some(_), None) => Action::Ingest,
        (None, Some(_)) => Action::Converge,
        (Some(d), Some(i)) if d == i => Action::Remove,
        (Some(_), Some(_)) => Action::Conflict,
    }
}

/// A path both sides changed, to different bytes, since they last agreed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Conflict {
    pub path: String,
    /// The directory's bytes; `None` when the file was deleted there.
    pub disk: Option<Hash>,
    /// The store's bytes, which the store keeps; `None` when a proposal
    /// deleted the file.
    pub stored: Option<Hash>,
    /// Where the two last agreed.
    pub index: Option<Hash>,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let side =
            |h: Option<Hash>| h.map_or_else(|| "deleted".to_string(), |h| format!("blob {h}"));
        write!(
            f,
            "'{}' changed in the source directory ({}) and in the store ({}) \
             since they last agreed; neither was overwritten",
            self.path,
            side(self.disk),
            side(self.stored)
        )
    }
}

/// What one [`reconcile`] did.
#[derive(Debug)]
pub struct Reconciled {
    /// The parse that ingested the directory's changes. A whole-directory
    /// reconcile always parses; a scoped one only when a path was ingested.
    pub parse: Option<ParseResult>,
    /// Paths whose stored bytes were written into the directory.
    pub materialized: Vec<String>,
    /// Paths the store deleted that were removed from the directory.
    pub removed: Vec<String>,
    /// Paths the directory could not be brought up to the store for: a
    /// write or a removal that failed, with the reason. The store keeps its
    /// side, and the next reconcile tries again.
    pub unmaterialized: Vec<(String, String)>,
    /// Paths both sides changed.
    pub conflicts: Vec<Conflict>,
    /// Files read from the directory to compare them; a file whose stat
    /// matches the store's record of it is not read.
    pub hashed: usize,
}

impl Reconciled {
    /// The paths this reconcile left out of step, in one line, or `None`
    /// when every path agrees.
    pub fn problems(&self) -> Option<String> {
        let mut all: Vec<String> = self
            .unmaterialized
            .iter()
            .map(|(path, why)| {
                format!("'{path}' was not brought up to the store in the source directory: {why}")
            })
            .collect();
        all.extend(self.conflicts.iter().map(Conflict::to_string));
        (!all.is_empty()).then(|| all.join("; "))
    }
}

/// Where a file's bytes are staged before they are renamed over it: a
/// hidden sibling, so the rename stays on one file system and a directory
/// walk skips it.
pub fn staging_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.leyline-checkout"))
}

/// Create the checkout's tables. An index created over a store that already
/// holds files starts as the store: the agreement every parse before the
/// index existed left behind, so an edit made while an upgraded daemon was
/// down is ingested, as it was before.
pub fn ensure_tables(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let had_index = table_exists(conn, "_checkout_index")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _checkout_index (
             path TEXT PRIMARY KEY,
             blob_hash BLOB NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS _checkout_conflict (
             path TEXT PRIMARY KEY,
             disk_hash BLOB,
             stored_hash BLOB,
             index_hash BLOB
         ) WITHOUT ROWID;",
    )?;
    if !had_index && table_exists(conn, "_source")? {
        conn.execute(
            "INSERT INTO _checkout_index (path, blob_hash) \
             SELECT id, content_hash FROM _source WHERE content_hash IS NOT NULL",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Reconcile the checkout of `dir` (the whole directory when `scope` is
/// `None`, else the paths it names) against the store and the index: see
/// the module documentation. `lang_filter` is the daemon's `--language`;
/// a path the parse would skip is not part of the checkout.
///
/// Materializing or removing a file can fail without failing the
/// reconcile: the path is reported in [`Reconciled::unmaterialized`], and a
/// file the store deleted is kept out of the store whether or not it could
/// be removed. Conflicts are recorded in `_checkout_conflict` and reported
/// in [`Reconciled::conflicts`].
pub fn reconcile(
    conn: &Connection,
    dir: &Path,
    lang_filter: Option<&str>,
    scope: Option<&[String]>,
) -> Result<Reconciled> {
    ensure_tables(conn)?;
    let lang = lang_filter
        .map(leyline_ts::languages::TsLanguage::from_name)
        .transpose()
        .context("invalid --lang")?;
    let has_source = table_exists(conn, "_source")?;
    let stored: HashMap<String, Hash> = if has_source {
        hashes(
            conn,
            "SELECT id, content_hash FROM _source WHERE content_hash IS NOT NULL",
        )?
    } else {
        HashMap::new()
    };
    let stats = if has_source {
        leyline_ts::schema::read_file_stats(conn)?
    } else {
        HashMap::new()
    };
    let index = hashes(conn, "SELECT path, blob_hash FROM _checkout_index")?;

    let paths: BTreeSet<String> = match scope {
        Some(paths) => paths.iter().cloned().collect(),
        None => {
            let mut walked = Vec::new();
            crate::walk::walk_into(dir, &mut walked)?;
            walked
                .iter()
                .filter_map(|abs| abs.strip_prefix(dir).ok())
                .map(|rel| rel.to_string_lossy().into_owned())
                .chain(stored.keys().cloned())
                .chain(index.keys().cloned())
                .collect()
        }
    };

    let mut ingest = Vec::new();
    let mut converge: Vec<(String, Option<Hash>)> = Vec::new();
    let mut materialize: Vec<(String, Hash)> = Vec::new();
    let mut remove: Vec<String> = Vec::new();
    let mut conflicts = Vec::new();
    let mut hashed = 0usize;
    for rel in &paths {
        let abs = dir.join(rel);
        if !inside(rel) || crate::cmd_parse::file_language(&abs, lang).is_none() {
            continue;
        }
        // A symlink, a directory, or a file the parse skips for its size
        // is not part of the checkout.
        let meta = std::fs::symlink_metadata(&abs).ok();
        if let Some(m) = &meta
            && !(m.is_file() && (m.len() as i64) <= crate::cmd_parse::MAX_PARSE_FILE_SIZE)
        {
            continue;
        }
        let indexed = index.get(rel).copied();
        let held = stored.get(rel).copied();
        // `meta` is `Some` exactly when the file is on disk: anything else
        // there left the loop above. A file whose stat matches the store's
        // record of it holds the stored bytes; a file new to both the store
        // and the index is ingested unread.
        let disk = match (&meta, held, indexed) {
            (None, _, _) => None,
            (Some(m), Some(held), _) if stats.get(rel) == Some(&stat_pair(m)) => Some(held),
            (Some(_), None, None) => {
                ingest.push(rel.clone());
                continue;
            }
            (Some(_), _, _) => {
                hashed += 1;
                match std::fs::read(&abs) {
                    Ok(bytes) => Some(bytes.hash()),
                    Err(e) => {
                        log::warn!("checkout: skip '{rel}': cannot read it: {e}");
                        continue;
                    }
                }
            }
        };
        let action = match held {
            Some(held) => classify(disk, held, indexed),
            None => classify_unstored(disk, indexed),
        };
        match action {
            Action::Nothing => {}
            Action::Materialize => materialize.extend(held.map(|h| (rel.clone(), h))),
            Action::Remove => remove.push(rel.clone()),
            Action::Ingest => ingest.push(rel.clone()),
            Action::Converge => converge.push((rel.clone(), held)),
            Action::Conflict => conflicts.push(Conflict {
                path: rel.clone(),
                disk,
                stored: held,
                index: indexed,
            }),
        }
    }

    // The directory is brought up to the store before anything is parsed
    // from it, so the parse reads the bytes it just received. Removals go
    // first, so a proposal that turns a file into a directory of files
    // lands in the directory too. A path the store deleted is kept out of
    // the parse whether or not its file could be removed.
    let mut materialized = Vec::new();
    let mut removed = Vec::new();
    let mut unmaterialized = Vec::new();
    let mut kept_out = BTreeSet::new();
    for rel in remove {
        match remove_from(dir, &rel) {
            Ok(()) => {
                set_index(conn, &rel, None)?;
                removed.push(rel);
            }
            Err(e) => {
                log::warn!("checkout: '{rel}' was not removed from the source directory: {e:#}");
                kept_out.insert(rel.clone());
                unmaterialized.push((rel, format!("{e:#}")));
            }
        }
    }
    for (rel, held) in materialize {
        match blob(conn, held).and_then(|bytes| write_into(dir, &rel, &bytes)) {
            Ok(()) => {
                set_index(conn, &rel, Some(held))?;
                materialized.push(rel);
            }
            Err(e) => {
                log::warn!("checkout: '{rel}' was not written to the source directory: {e:#}");
                unmaterialized.push((rel, format!("{e:#}")));
            }
        }
    }

    // A conflict keeps the store's side through the parse: its bytes, or
    // its deletion.
    let mut kept = BTreeMap::new();
    for c in &conflicts {
        log::warn!("checkout conflict: {c}");
        match c.stored {
            Some(stored) => {
                kept.insert(c.path.clone(), blob(conn, stored)?);
            }
            None => {
                kept_out.insert(c.path.clone());
            }
        }
    }
    let parse = match scope {
        None => Some(crate::cmd_parse::parse_checkout(
            conn,
            dir,
            lang_filter,
            None,
            &kept,
            &kept_out,
        )?),
        Some(_) if !ingest.is_empty() => Some(crate::cmd_parse::parse_checkout(
            conn,
            dir,
            lang_filter,
            Some(&ingest),
            &kept,
            &kept_out,
        )?),
        Some(_) => None,
    };

    let tx = conn.unchecked_transaction()?;
    // An ingested path's index is what the parse stored from the directory.
    for rel in &ingest {
        let now: Option<Vec<u8>> = conn
            .query_row(
                "SELECT content_hash FROM _source WHERE id = ?1",
                [rel],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        set_index(conn, rel, now.and_then(to_hash))?;
    }
    for (rel, held) in &converge {
        set_index(conn, rel, *held)?;
    }
    match scope {
        None => {
            conn.execute("DELETE FROM _checkout_conflict", [])?;
        }
        Some(_) => {
            for rel in &paths {
                conn.execute("DELETE FROM _checkout_conflict WHERE path = ?1", [rel])?;
            }
        }
    }
    for c in &conflicts {
        conn.execute(
            "INSERT INTO _checkout_conflict (path, disk_hash, stored_hash, index_hash) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                c.path,
                c.disk.map(|h| h.as_bytes().to_vec()),
                c.stored.map(|h| h.as_bytes().to_vec()),
                c.index.map(|h| h.as_bytes().to_vec()),
            ],
        )?;
    }
    tx.commit()?;

    Ok(Reconciled {
        parse,
        materialized,
        removed,
        unmaterialized,
        conflicts,
        hashed,
    })
}

/// Write `bytes` as the file `rel` names under `dir`: staged beside it,
/// synced, renamed over it, and the directory synced, so a crash leaves the
/// old file or the new one. The file keeps its permissions. Refused when
/// `rel` is not a plain relative path or its directory resolves outside
/// `dir` (a symlink out of the tree).
pub fn write_into(dir: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    if !inside(rel) {
        bail!("refusing to write '{rel}': not a path inside the source directory");
    }
    let target = dir.join(rel);
    let parent = target
        .parent()
        .with_context(|| format!("refusing to write '{rel}': it names no directory"))?;
    let root = dir
        .canonicalize()
        .with_context(|| format!("resolve the source directory {}", dir.display()))?;
    // The nearest directory that exists is checked before anything is
    // created, and the file's own directory after.
    let existing = parent
        .ancestors()
        .find(|a| a.exists())
        .unwrap_or(dir)
        .canonicalize()?;
    if !existing.starts_with(&root) {
        bail!(
            "refusing to write '{rel}': {} is outside the source directory {}",
            existing.display(),
            root.display()
        );
    }
    std::fs::create_dir_all(parent)?;
    let resolved = parent.canonicalize()?;
    if !resolved.starts_with(&root) {
        bail!(
            "refusing to write '{rel}': {} is outside the source directory {}",
            resolved.display(),
            root.display()
        );
    }
    let staging = staging_path(&target);
    let written = (|| -> Result<()> {
        let mut file = std::fs::File::create(&staging)
            .with_context(|| format!("stage {}", staging.display()))?;
        file.write_all(bytes)?;
        if let Ok(meta) = std::fs::metadata(&target) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        std::fs::rename(&staging, &target)
            .with_context(|| format!("rename {} over {}", staging.display(), target.display()))?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if written.is_err() {
        // A staged file that never reached its target is not left behind.
        let _ = std::fs::remove_file(&staging);
    }
    written.with_context(|| format!("write '{rel}' into {}", dir.display()))
}

/// Remove the file `rel` names under `dir`, then each directory above it
/// that the removal left empty, up to `dir` itself, and sync the nearest
/// directory left standing, so the removal survives a crash. Refused when
/// `rel` is not a plain relative path or its directory resolves outside
/// `dir` (a symlink out of the tree).
pub fn remove_from(dir: &Path, rel: &str) -> Result<()> {
    if !inside(rel) {
        bail!("refusing to remove '{rel}': not a path inside the source directory");
    }
    let target = dir.join(rel);
    let parent = target
        .parent()
        .with_context(|| format!("refusing to remove '{rel}': it names no directory"))?;
    let root = dir
        .canonicalize()
        .with_context(|| format!("resolve the source directory {}", dir.display()))?;
    let resolved = parent
        .canonicalize()
        .with_context(|| format!("resolve the directory of '{rel}'"))?;
    if !resolved.starts_with(&root) {
        bail!(
            "refusing to remove '{rel}': {} is outside the source directory {}",
            resolved.display(),
            root.display()
        );
    }
    let removed = (|| -> Result<()> {
        std::fs::remove_file(&target)?;
        let mut standing = parent;
        while standing != dir && std::fs::remove_dir(standing).is_ok() {
            standing = standing.parent().unwrap_or(dir);
        }
        std::fs::File::open(standing)?.sync_all()?;
        Ok(())
    })();
    removed.with_context(|| format!("remove '{rel}' from {}", dir.display()))
}

/// Whether `rel` is a plain relative path: one or more normal components,
/// no root, no `..`.
fn inside(rel: &str) -> bool {
    let path = Path::new(rel);
    path.components().next().is_some()
        && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// The `(mtime ns, size)` pair the parse records for a file.
fn stat_pair(meta: &std::fs::Metadata) -> (i64, i64) {
    let mtime = meta
        .modified()
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64;
    (mtime, meta.len() as i64)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn to_hash(bytes: Vec<u8>) -> Option<Hash> {
    <[u8; 32]>::try_from(bytes).ok().map(Hash::from_bytes)
}

/// `path → hash` rows of a two-column query; rows whose hash is not 32
/// bytes are not hashes and are left out.
fn hashes(conn: &Connection, sql: &str) -> Result<HashMap<String, Hash>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (path, bytes) = row?;
        if let Some(h) = to_hash(bytes) {
            out.insert(path, h);
        }
    }
    Ok(out)
}

fn blob(conn: &Connection, hash: Hash) -> Result<Vec<u8>> {
    conn.query_row(
        "SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1",
        [hash.as_bytes().as_slice()],
        |r| r.get(0),
    )
    .optional()?
    .with_context(|| format!("the store holds no bytes for blob {hash}"))
}

fn set_index(conn: &Connection, rel: &str, hash: Option<Hash>) -> Result<()> {
    match hash {
        Some(h) => conn.execute(
            "INSERT OR REPLACE INTO _checkout_index (path, blob_hash) VALUES (?1, ?2)",
            rusqlite::params![rel, h.as_bytes().as_slice()],
        )?,
        None => conn.execute("DELETE FROM _checkout_index WHERE path = ?1", [rel])?,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_write::{Base, Grant, Proposed, propose};
    use tempfile::TempDir;

    const A: &str = "package a\n\nfunc A() int { return 1 }\n";
    const A2: &str = "package a\n\nfunc A() int { return 2 }\n";
    const A3: &str = "package a\n\nfunc A() int { return 3 }\n";
    const B: &str = "package a\n\nfunc B() string { return \"b\" }\n";
    const B2: &str = "package a\n\nfunc B() string { return \"bee\" }\n";

    fn h(text: &str) -> Hash {
        text.as_bytes().hash()
    }

    /// A checkout of `files`, reconciled once from cold.
    struct Checkout {
        _dir: TempDir,
        src: PathBuf,
        conn: Connection,
    }

    fn checkout(files: &[(&str, &str)]) -> Checkout {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        for (rel, text) in files {
            let path = src.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let conn = Connection::open(dir.path().join("live.db")).unwrap();
        let cold = reconcile(&conn, &src, None, None).unwrap();
        assert_eq!(cold.parse.as_ref().unwrap().parsed, files.len() as u64);
        Checkout {
            _dir: dir,
            src,
            conn,
        }
    }

    /// Commit a proposal to the store without materializing it: the state
    /// a crash between `COMMIT` and the file write leaves.
    fn commit_only(c: &Checkout, path: &str, text: &str) {
        propose(
            &c.conn,
            &Grant::Whole,
            vec![Proposed::write(path, Base::Any, text.as_bytes().to_vec())],
        )
        .unwrap();
    }

    fn stored(c: &Checkout, path: &str) -> Option<Hash> {
        c.conn
            .query_row(
                "SELECT content_hash FROM _source WHERE id = ?1",
                [path],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .unwrap()
            .and_then(to_hash)
    }

    fn indexed(c: &Checkout, path: &str) -> Option<Hash> {
        hashes(&c.conn, "SELECT path, blob_hash FROM _checkout_index")
            .unwrap()
            .get(path)
            .copied()
    }

    fn conflict_paths(c: &Checkout) -> Vec<String> {
        let mut stmt = c
            .conn
            .prepare("SELECT path FROM _checkout_conflict ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn disk(c: &Checkout, path: &str) -> Option<String> {
        std::fs::read_to_string(c.src.join(path)).ok()
    }

    #[test]
    fn classify_follows_the_three_way_table() {
        let (x, y, z) = (h(A), h(A2), h(A3));
        assert_eq!(classify(Some(x), x, Some(x)), Action::Nothing);
        assert_eq!(classify(Some(x), y, Some(x)), Action::Materialize);
        assert_eq!(classify(None, y, None), Action::Materialize);
        assert_eq!(classify(Some(y), x, Some(x)), Action::Ingest);
        assert_eq!(classify(None, x, Some(x)), Action::Ingest);
        assert_eq!(classify(Some(y), y, Some(x)), Action::Converge);
        assert_eq!(classify(Some(y), y, None), Action::Converge);
        assert_eq!(classify(Some(z), y, Some(x)), Action::Conflict);
        assert_eq!(classify(None, y, Some(x)), Action::Conflict);
        assert_eq!(classify(Some(z), y, None), Action::Conflict);
    }

    /// A path the store does not hold: the same table with the store's side
    /// absent. The store deleted it when the index still names it.
    #[test]
    fn classify_unstored_follows_the_table_with_the_store_absent() {
        let (x, y) = (h(A), h(A2));
        assert_eq!(classify_unstored(None, None), Action::Nothing);
        assert_eq!(classify_unstored(Some(x), None), Action::Ingest);
        assert_eq!(classify_unstored(None, Some(x)), Action::Converge);
        assert_eq!(classify_unstored(Some(x), Some(x)), Action::Remove);
        assert_eq!(classify_unstored(Some(y), Some(x)), Action::Conflict);
    }

    fn commit(c: &Checkout, writes: Vec<Proposed>) {
        propose(&c.conn, &Grant::Whole, writes).unwrap();
    }

    fn scope(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    /// Bead `ley-line-open-47fb03`: a create is written into the directory
    /// (its directories made) and a delete removes the file, and the index
    /// follows each.
    #[test]
    fn a_proposed_create_is_materialized_and_a_proposed_delete_removes_the_file() {
        let c = checkout(&[("a.go", A), ("b.go", B)]);
        commit(
            &c,
            vec![
                Proposed::create("sub/c.go", A3.as_bytes().to_vec()),
                Proposed::delete("b.go", Base::Blob(h(B))),
            ],
        );
        let r = reconcile(&c.conn, &c.src, None, Some(&scope(&["sub/c.go", "b.go"]))).unwrap();
        assert_eq!(r.materialized, ["sub/c.go"]);
        assert_eq!(r.removed, ["b.go"]);
        assert_eq!(r.problems(), None);
        assert!(r.parse.is_none(), "nothing was ingested");
        assert_eq!(disk(&c, "sub/c.go").as_deref(), Some(A3));
        assert_eq!(disk(&c, "b.go"), None);
        assert_eq!(indexed(&c, "sub/c.go"), Some(h(A3)));
        assert_eq!(indexed(&c, "b.go"), None);
        assert_eq!(stored(&c, "b.go"), None);

        // The last file of a directory takes the emptied directory with it.
        commit(&c, vec![Proposed::delete("sub/c.go", Base::Blob(h(A3)))]);
        let r = reconcile(&c.conn, &c.src, None, Some(&scope(&["sub/c.go"]))).unwrap();
        assert_eq!(r.removed, ["sub/c.go"]);
        assert!(!c.src.join("sub").exists());
        assert!(c.src.exists(), "the checkout's own directory stays");

        // Nothing is left to do.
        let quiet = reconcile(&c.conn, &c.src, None, None).unwrap();
        assert!(quiet.removed.is_empty() && quiet.materialized.is_empty());
        assert_eq!(stored(&c, "b.go"), None);
    }

    /// A create and a delete committed before a crash are carried out by
    /// the whole-directory reconcile a restart runs: the deleted file is
    /// removed from the directory, not ingested back into the store.
    #[test]
    fn a_create_and_delete_committed_before_a_crash_are_carried_out_at_restart() {
        let c = checkout(&[("a.go", A), ("b.go", B)]);
        commit(
            &c,
            vec![
                Proposed::delete("a.go", Base::Blob(h(A))),
                Proposed::create("c.go", A.as_bytes().to_vec()),
            ],
        );
        // The restart: a new connection to the same database.
        let db = c.src.parent().unwrap().join("live.db");
        let restarted = Connection::open(&db).unwrap();
        let r = reconcile(&restarted, &c.src, None, None).unwrap();
        assert_eq!(r.removed, ["a.go"]);
        assert_eq!(r.materialized, ["c.go"]);
        assert_eq!(r.problems(), None);
        let c = Checkout {
            conn: restarted,
            ..c
        };
        assert_eq!(disk(&c, "a.go"), None);
        assert_eq!(stored(&c, "a.go"), None, "the delete is not undone");
        assert_eq!(disk(&c, "c.go").as_deref(), Some(A));
        assert_eq!(stored(&c, "c.go"), Some(h(A)));
        assert_eq!(indexed(&c, "a.go"), None);
        assert_eq!(indexed(&c, "c.go"), Some(h(A)));
    }

    /// The store deleted a file the directory changed: a conflict. Neither
    /// side is overwritten — the store does not take the file back, the
    /// directory keeps it — until the two agree.
    #[test]
    fn a_store_delete_against_a_directory_edit_is_a_conflict_that_keeps_both() {
        let c = checkout(&[("a.go", A), ("b.go", B)]);
        commit(&c, vec![Proposed::delete("b.go", Base::Blob(h(B)))]);
        std::fs::write(c.src.join("b.go"), B2).unwrap();

        let r = reconcile(&c.conn, &c.src, None, None).unwrap();
        assert_eq!(
            r.conflicts,
            [Conflict {
                path: "b.go".to_string(),
                disk: Some(h(B2)),
                stored: None,
                index: Some(h(B)),
            }]
        );
        assert!(r.removed.is_empty());
        assert_eq!(stored(&c, "b.go"), None, "the store keeps the delete");
        assert_eq!(disk(&c, "b.go").as_deref(), Some(B2), "the edit is kept");
        assert_eq!(conflict_paths(&c), ["b.go"]);
        let problems = r.problems().unwrap();
        assert!(problems.contains("in the store (deleted)"), "{problems}");

        // The person deletes it too: the two agree.
        std::fs::remove_file(c.src.join("b.go")).unwrap();
        let settled = reconcile(&c.conn, &c.src, None, None).unwrap();
        assert!(settled.conflicts.is_empty());
        assert!(conflict_paths(&c).is_empty());
        assert_eq!(indexed(&c, "b.go"), None);
    }

    /// A delete the directory refuses (a read-only directory) is reported,
    /// and a whole-directory pass does not ingest the file back.
    #[cfg(unix)]
    #[test]
    fn a_removal_the_directory_refuses_keeps_the_store_deletion() {
        use std::os::unix::fs::PermissionsExt;
        let c = checkout(&[("a.go", A), ("ro/b.go", B)]);
        commit(&c, vec![Proposed::delete("ro/b.go", Base::Blob(h(B)))]);
        let ro = c.src.join("ro");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = reconcile(&c.conn, &c.src, None, None);
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        let r = r.unwrap();
        assert!(r.removed.is_empty());
        assert_eq!(r.unmaterialized.len(), 1, "{:?}", r.unmaterialized);
        assert_eq!(r.unmaterialized[0].0, "ro/b.go");
        assert!(r.problems().unwrap().contains("ro/b.go"));
        assert_eq!(stored(&c, "ro/b.go"), None, "not ingested back");
        assert_eq!(indexed(&c, "ro/b.go"), Some(h(B)), "tried again next time");

        let retried = reconcile(&c.conn, &c.src, None, None).unwrap();
        assert_eq!(retried.removed, ["ro/b.go"]);
        assert_eq!(disk(&c, "ro/b.go"), None);
    }

    #[test]
    fn remove_from_refuses_a_path_outside_the_directory() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(dir.path().join("outside.go"), A).unwrap();
        for rel in ["../outside.go", "/etc/hosts", "", "./a.go"] {
            let err = remove_from(&src, rel).unwrap_err();
            assert!(format!("{err:#}").contains("refusing"), "{rel}: {err:#}");
        }
        assert!(dir.path().join("outside.go").exists());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), src.join("link")).unwrap();
            let err = remove_from(&src, "link/outside.go").unwrap_err();
            assert!(format!("{err:#}").contains("outside"), "{err:#}");
            assert!(dir.path().join("outside.go").exists());
        }
    }

    #[test]
    fn a_cold_reconcile_indexes_what_it_ingested_and_the_next_reads_nothing() {
        let c = checkout(&[("a.go", A), ("b.go", B)]);
        assert_eq!(indexed(&c, "a.go"), Some(h(A)));
        assert_eq!(indexed(&c, "b.go"), Some(h(B)));
        let again = reconcile(&c.conn, &c.src, None, None).unwrap();
        assert_eq!(again.parse.as_ref().unwrap().unchanged, 2);
        assert_eq!(again.hashed, 0, "a file whose stat matches is not read");
        assert!(again.materialized.is_empty() && again.conflicts.is_empty());
        assert_eq!(again.problems(), None);
    }

    #[test]
    fn a_scoped_reconcile_materializes_the_store_and_ingests_the_directory() {
        let c = checkout(&[("a.go", A), ("b.go", B)]);
        commit_only(&c, "a.go", A2);
        std::fs::write(c.src.join("b.go"), B2).unwrap();

        let scope = ["a.go".to_string(), "b.go".to_string()];
        let r = reconcile(&c.conn, &c.src, None, Some(&scope)).unwrap();
        assert_eq!(r.materialized, ["a.go"]);
        assert_eq!(r.hashed, 2, "both files' stats moved, so both were read");
        assert_eq!(disk(&c, "a.go").as_deref(), Some(A2));
        assert_eq!(indexed(&c, "a.go"), Some(h(A2)));
        assert_eq!(r.parse.as_ref().unwrap().changed_files, ["b.go"]);
        assert_eq!(stored(&c, "b.go"), Some(h(B2)));
        assert_eq!(indexed(&c, "b.go"), Some(h(B2)));
        assert!(!staging_path(&c.src.join("a.go")).exists());

        // Nothing left to ingest: a scoped reconcile runs no parse.
        let quiet = reconcile(&c.conn, &c.src, None, Some(&scope)).unwrap();
        assert!(quiet.parse.is_none());
        assert!(quiet.materialized.is_empty());
    }

    #[test]
    fn a_conflict_keeps_the_store_through_a_whole_directory_pass() {
        let c = checkout(&[("a.go", A), ("b.go", B)]);
        commit_only(&c, "a.go", A2);
        std::fs::remove_file(c.src.join("a.go")).unwrap();

        let r = reconcile(&c.conn, &c.src, None, None).unwrap();
        assert_eq!(
            r.conflicts,
            [Conflict {
                path: "a.go".to_string(),
                disk: None,
                stored: Some(h(A2)),
                index: Some(h(A)),
            }]
        );
        assert_eq!(
            r.parse.as_ref().unwrap().unchanged,
            2,
            "the kept file and b.go, each once"
        );
        assert_eq!(
            stored(&c, "a.go"),
            Some(h(A2)),
            "the store keeps the proposal"
        );
        assert_eq!(disk(&c, "a.go"), None, "the deletion is not undone");
        assert_eq!(indexed(&c, "a.go"), Some(h(A)));
        assert_eq!(conflict_paths(&c), ["a.go"]);
        let problems = r.problems().unwrap();
        assert!(
            problems.contains("'a.go' changed in the source directory (deleted)"),
            "{problems}"
        );

        // A second pass reports it again, once.
        let again = reconcile(&c.conn, &c.src, None, None).unwrap();
        assert_eq!(again.conflicts.len(), 1);
        assert_eq!(conflict_paths(&c), ["a.go"]);

        // The directory comes to hold the stored bytes: converged.
        std::fs::write(c.src.join("a.go"), A2).unwrap();
        let settled = reconcile(&c.conn, &c.src, None, None).unwrap();
        assert!(settled.conflicts.is_empty());
        assert!(settled.materialized.is_empty());
        assert!(conflict_paths(&c).is_empty());
        assert_eq!(indexed(&c, "a.go"), Some(h(A2)));
    }

    #[test]
    fn a_crash_after_the_rename_converges_without_a_write() {
        let c = checkout(&[("a.go", A)]);
        commit_only(&c, "a.go", A2);
        std::fs::write(c.src.join("a.go"), A2).unwrap();
        let r = reconcile(&c.conn, &c.src, None, Some(&["a.go".to_string()])).unwrap();
        assert!(r.materialized.is_empty() && r.conflicts.is_empty());
        assert!(r.parse.is_none());
        assert_eq!(indexed(&c, "a.go"), Some(h(A2)));
    }

    #[test]
    fn an_index_entry_with_no_file_on_either_side_is_dropped() {
        let c = checkout(&[("a.go", A)]);
        set_index(&c.conn, "gone.go", Some(h(B))).unwrap();
        reconcile(&c.conn, &c.src, None, None).unwrap();
        assert_eq!(indexed(&c, "gone.go"), None);
        assert_eq!(indexed(&c, "a.go"), Some(h(A)));
    }

    #[test]
    fn an_upgraded_arena_ingests_an_edit_made_while_it_was_down() {
        // A store parsed before the checkout index existed.
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.go"), A).unwrap();
        let conn = Connection::open(dir.path().join("live.db")).unwrap();
        crate::cmd_parse::parse_into_conn(&conn, &src, None, None).unwrap();
        assert!(!table_exists(&conn, "_checkout_index").unwrap());
        std::fs::write(src.join("a.go"), A3).unwrap();

        let r = reconcile(&conn, &src, None, None).unwrap();
        assert!(r.conflicts.is_empty(), "{:?}", r.conflicts);
        let c = Checkout {
            _dir: dir,
            src,
            conn,
        };
        assert_eq!(stored(&c, "a.go"), Some(h(A3)));
        assert_eq!(indexed(&c, "a.go"), Some(h(A3)));
    }

    #[test]
    fn a_file_the_language_filter_skips_is_not_checked_out() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.go"), A).unwrap();
        std::fs::write(src.join("x.py"), "x = 1\n").unwrap();
        let conn = Connection::open(dir.path().join("live.db")).unwrap();
        reconcile(&conn, &src, Some("go"), None).unwrap();
        let index = hashes(&conn, "SELECT path, blob_hash FROM _checkout_index").unwrap();
        assert_eq!(index.keys().collect::<Vec<_>>(), ["a.go"]);
    }

    /// A path out of the tree, a directory, and a file over the parse size
    /// limit are not checkout files: a reconcile scoped to them reads,
    /// parses and indexes nothing.
    #[test]
    fn a_path_that_is_not_a_checkout_file_is_left_alone() {
        let c = checkout(&[("a.go", A)]);
        let outside = c.src.parent().unwrap().join("escape.go");
        std::fs::write(&outside, A).unwrap();
        std::fs::create_dir_all(c.src.join("d.go")).unwrap();
        let limit = crate::cmd_parse::MAX_PARSE_FILE_SIZE as usize;
        std::fs::write(c.src.join("big.go"), "x".repeat(limit + 1)).unwrap();

        let scope = [
            "../escape.go".to_string(),
            "d.go".to_string(),
            "big.go".to_string(),
        ];
        let r = reconcile(&c.conn, &c.src, None, Some(&scope)).unwrap();
        assert!(r.parse.is_none(), "{:?}", r.parse);
        assert_eq!(r.hashed, 0);
        let index = hashes(&c.conn, "SELECT path, blob_hash FROM _checkout_index").unwrap();
        assert_eq!(index.keys().collect::<Vec<_>>(), ["a.go"]);
    }

    #[test]
    fn a_file_at_the_parse_size_limit_is_checked_out() {
        let limit = crate::cmd_parse::MAX_PARSE_FILE_SIZE as usize;
        let mut text = String::from("package a\n//");
        text.push_str(&"x".repeat(limit - text.len() - 1));
        text.push('\n');
        assert_eq!(text.len(), limit);
        let c = checkout(&[("big.go", &text)]);
        assert_eq!(indexed(&c, "big.go"), Some(h(&text)));
    }

    #[test]
    fn write_into_refuses_a_path_outside_the_directory() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        for rel in ["../outside/a.go", "/etc/a.go", "", "./a.go"] {
            let err = write_into(&src, rel, b"x").unwrap_err();
            assert!(format!("{err:#}").contains("refusing"), "{rel}: {err:#}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, src.join("link")).unwrap();
            let err = write_into(&src, "link/a.go", b"x").unwrap_err();
            assert!(format!("{err:#}").contains("outside"), "{err:#}");
            let err = write_into(&src, "link/deeper/a.go", b"x").unwrap_err();
            assert!(format!("{err:#}").contains("outside"), "{err:#}");
            assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        }
    }

    #[test]
    fn write_into_creates_directories_and_keeps_permissions() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().to_path_buf();
        write_into(&src, "new/dir/c.go", b"package c\n").unwrap();
        assert_eq!(
            std::fs::read(src.join("new/dir/c.go")).unwrap(),
            b"package c\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = src.join("run.sh");
            std::fs::write(&script, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            write_into(&src, "run.sh", b"#!/bin/sh\necho\n").unwrap();
            let mode = std::fs::metadata(&script).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn a_failed_write_leaves_no_staged_file() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().to_path_buf();
        // A directory where the file goes: the rename over it fails.
        std::fs::create_dir_all(src.join("a.go/inner")).unwrap();
        assert!(write_into(&src, "a.go", b"package a\n").is_err());
        assert!(!staging_path(&src.join("a.go")).exists());
    }

    #[test]
    fn the_staging_path_is_a_hidden_sibling() {
        assert_eq!(
            staging_path(Path::new("/w/src/pkg/a.go")),
            PathBuf::from("/w/src/pkg/.a.go.leyline-checkout")
        );
    }
}
