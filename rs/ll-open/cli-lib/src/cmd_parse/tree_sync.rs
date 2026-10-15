//! ADR-0045 T1, T3: bring the store's manifest (`_tree`) and the parse's
//! diagnostics up to what one pass saw, inside the pass's transaction.
//!
//! The pass is authoritative over a set of paths: the whole walked tree
//! for a whole-directory pass, the in-memory tree for `leyline rebuild`,
//! or the scope's paths (and a proposal's directory edits) for a scoped
//! pass. Each such path ends as a file entry (with its mode, its blob in
//! `source_blobs`, and the walk's stat), a directory entry, or no entry.
//! Directories above an entry are entries (`tree::put_file`/`put_dir`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use leyline_core::{ContentAddressed, Hash};
use rusqlite::Connection;

use super::{InMemory, SyntaxError};
use crate::tree::{self, Diagnostic, Entry, Mode};

/// What the pass decided per path.
enum Seen<'a> {
    /// A file or symlink on disk.
    Disk { abs: PathBuf, symlink: bool },
    /// File bytes read from memory.
    Memory(&'a [u8]),
    Dir,
    Gone,
}

/// The paths a pass is authoritative over.
pub(super) enum Reach<'a> {
    Walked(&'a crate::walk::Walked),
    Tree,
    Paths(&'a [String]),
}

/// What the parse made of the files it took up.
pub(super) struct Outcomes<'a> {
    /// Projected this pass: blob hash and first syntax error.
    pub projected: &'a HashMap<String, ([u8; 32], Option<SyntaxError>)>,
    /// Taken up and not projected, with why.
    pub unprojected: &'a BTreeMap<String, String>,
}

fn rel_of(source: &Path, abs: &Path) -> String {
    abs.strip_prefix(source)
        .unwrap_or(abs)
        .to_string_lossy()
        .into_owned()
}

fn stat_of(meta: &std::fs::Metadata) -> (i64, i64) {
    let mtime = meta
        .modified()
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64;
    (mtime, meta.len() as i64)
}

/// The mode a file on disk is stored with.
pub(crate) fn disk_mode(meta: &std::fs::Metadata) -> Mode {
    if meta.file_type().is_symlink() {
        return Mode::Symlink;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return Mode::Executable;
        }
    }
    Mode::Regular
}

/// A symlink's target as the bytes git stores for it.
pub(crate) fn link_bytes(abs: &Path) -> Result<Vec<u8>> {
    let target = std::fs::read_link(abs).with_context(|| format!("readlink {}", abs.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(target.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Ok(target.to_string_lossy().into_owned().into_bytes())
    }
}

/// Store `bytes` in `source_blobs` (once per hash) and return their hash.
pub(crate) fn store_blob(conn: &Connection, bytes: &[u8]) -> Result<Hash> {
    let hash = bytes.hash();
    conn.prepare_cached("INSERT OR IGNORE INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)")?
        .execute(rusqlite::params![hash.as_bytes().as_slice(), bytes])?;
    Ok(hash)
}

pub(super) fn sync(
    conn: &Connection,
    source: &Path,
    reach: Reach<'_>,
    memory: Option<&InMemory<'_>>,
    outcomes: &Outcomes<'_>,
) -> Result<()> {
    let removes = |rel: &str| memory.is_some_and(|m| m.removes(rel));
    let mut seen: BTreeMap<String, Seen<'_>> = BTreeMap::new();
    let whole = !matches!(reach, Reach::Paths(_));
    match reach {
        Reach::Walked(walked) => {
            for (abs, symlink) in walked
                .files
                .iter()
                .map(|p| (p, false))
                .chain(walked.symlinks.iter().map(|p| (p, true)))
            {
                let rel = rel_of(source, abs);
                if !removes(&rel) {
                    seen.insert(
                        rel,
                        Seen::Disk {
                            abs: abs.clone(),
                            symlink,
                        },
                    );
                }
            }
            for dir in &walked.dirs {
                seen.insert(rel_of(source, dir), Seen::Dir);
            }
        }
        Reach::Tree => {}
        Reach::Paths(paths) => {
            for rel in paths {
                if removes(rel) {
                    seen.insert(rel.clone(), Seen::Gone);
                    continue;
                }
                if memory.is_some_and(|m| m.bytes(rel).is_some()) {
                    continue; // taken from memory below
                }
                let abs = source.join(rel);
                match std::fs::symlink_metadata(&abs) {
                    Ok(m) if m.file_type().is_symlink() => {
                        seen.insert(rel.clone(), Seen::Disk { abs, symlink: true });
                    }
                    Ok(m) if m.is_dir() => {
                        seen.insert(rel.clone(), Seen::Dir);
                    }
                    Ok(m) if m.is_file() => {
                        seen.insert(rel.clone(), Seen::Disk { abs, symlink: false });
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        seen.insert(rel.clone(), Seen::Gone);
                    }
                    Err(e) => log::warn!("tree: skip '{rel}': {e}"),
                }
            }
        }
    }
    if let Some(m) = memory {
        let files = match m {
            InMemory::Written { files, .. } | InMemory::Tree { files, .. } => *files,
        };
        for (rel, bytes) in files {
            if !removes(rel) {
                seen.insert(rel.clone(), Seen::Memory(bytes));
            }
        }
        for dir in &m.edits().mkdirs {
            seen.insert(dir.clone(), Seen::Dir);
        }
        for dir in &m.edits().rmdirs {
            seen.insert(dir.clone(), Seen::Gone);
        }
    }

    // A whole pass also removes every entry it did not see.
    let existing: BTreeMap<String, Entry> = if whole {
        tree::entries(conn)?
    } else {
        BTreeMap::new()
    };
    if whole {
        for rel in existing.keys() {
            seen.entry(rel.clone()).or_insert(Seen::Gone);
        }
    }
    let current = |rel: &str| -> Result<Option<Entry>> {
        if whole {
            Ok(existing.get(rel).copied())
        } else {
            tree::entry(conn, rel)
        }
    };
    let stats = tree::stats(conn)?;

    // Removals first, then directories, then files: a file's parents are
    // made as it lands, and a directory a file replaces is gone first.
    for (rel, s) in &seen {
        if !matches!(s, Seen::Gone) {
            continue;
        }
        let was = current(rel)?;
        if was.is_none() {
            continue;
        }
        tree::remove(conn, rel)?;
        // A directory gone from disk takes what the store held under it.
        if was == Some(Entry::Dir) && !whole && !memory.is_some_and(|m| m.edits().rmdirs.contains(rel)) {
            for (under, _) in tree::under(conn, rel)? {
                tree::remove(conn, &under)?;
            }
        }
    }
    for (rel, s) in &seen {
        if matches!(s, Seen::Dir) && current(rel)? != Some(Entry::Dir) {
            tree::put_dir(conn, rel)?;
        }
    }
    let mut stored_blob: HashMap<&str, Hash> = HashMap::new();
    for (rel, s) in &seen {
        let was = current(rel)?;
        let (mode, stat, bytes): (Mode, Option<(i64, i64)>, Option<Vec<u8>>) = match s {
            Seen::Gone | Seen::Dir => continue,
            Seen::Memory(bytes) => {
                let mode = memory
                    .and_then(|m| m.edits().modes.get(rel).copied())
                    .or(match was {
                        Some(Entry::File { mode, .. }) => Some(mode),
                        _ => None,
                    })
                    .unwrap_or(Mode::Regular);
                (mode, None, Some(bytes.to_vec()))
            }
            Seen::Disk { abs, symlink } => {
                let meta = match std::fs::symlink_metadata(abs) {
                    Ok(m) => m,
                    Err(e) => {
                        log::warn!("tree: skip '{rel}': {e}");
                        continue;
                    }
                };
                let mode = disk_mode(&meta);
                let stat = stat_of(&meta);
                let fresh = !outcomes.projected.contains_key(rel.as_str())
                    && !outcomes.unprojected.contains_key(rel.as_str());
                if fresh
                    && matches!(was, Some(Entry::File { mode: m, .. }) if m == mode)
                    && stats.get(rel.as_str()) == Some(&stat)
                {
                    continue; // unchanged since the walk last stored it
                }
                let bytes = if *symlink {
                    None
                } else if outcomes.projected.contains_key(rel.as_str()) {
                    None
                } else {
                    match std::fs::read(abs) {
                        Ok(b) => Some(b),
                        Err(e) => {
                            log::warn!("tree: skip '{rel}': cannot read it: {e}");
                            continue;
                        }
                    }
                };
                let bytes = if *symlink { Some(link_bytes(abs)?) } else { bytes };
                (mode, Some(stat), bytes)
            }
        };
        let blob = match (bytes, outcomes.projected.get(rel.as_str())) {
            (Some(bytes), _) => store_blob(conn, &bytes)?,
            (None, Some((hash, _))) => Hash::from_bytes(*hash),
            (None, None) => continue,
        };
        // A file whose bytes changed and that this pass did not take up
        // (a file the parse does not project) has no diagnostic.
        let changed = was.and_then(Entry::blob) != Some(blob);
        if changed
            && !outcomes.projected.contains_key(rel.as_str())
            && !outcomes.unprojected.contains_key(rel.as_str())
        {
            tree::set_diagnostic(conn, rel, None)?;
        }
        tree::put_file(conn, rel, mode, blob, stat)?;
        stored_blob.insert(rel.as_str(), blob);
    }

    for (rel, (hash, syntax)) in outcomes.projected {
        let diagnostic = syntax.as_ref().map(|e| Diagnostic {
            path: rel.clone(),
            blob: Hash::from_bytes(*hash),
            projected: true,
            span: Some((e.start_byte, e.end_byte, e.start_row, e.start_col)),
            message: e.message.clone(),
        });
        tree::set_diagnostic(conn, rel, diagnostic.as_ref())?;
    }
    for (rel, message) in outcomes.unprojected {
        let blob = match stored_blob.get(rel.as_str()) {
            Some(b) => Some(*b),
            None => current(rel)?.and_then(Entry::blob),
        };
        if let Some(blob) = blob {
            tree::set_diagnostic(
                conn,
                rel,
                Some(&Diagnostic {
                    path: rel.clone(),
                    blob,
                    projected: false,
                    span: None,
                    message: message.clone(),
                }),
            )?;
        }
    }
    Ok(())
}
