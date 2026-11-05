//! The one reader of `source_blobs` bytes: Σ axiom CA as verify-on-read
//! (bead `ley-line-open-12d305`).
//!
//! `source_blobs` (ADR-0028) keys every row on `σ_bytes(blob_bytes)`, and a
//! reader that serves the bytes stored under a key without re-deriving the
//! key from them serves whatever the row holds: a row changed under its key
//! becomes the file the key names. Every read of `source_blobs.blob_bytes`
//! goes through [`read`] or [`read_at`], which mint a [`Ref<Blob>`] from the
//! bytes ([`Ref::verified`]) and refuse the row when the bytes do not
//! re-derive the key. A mismatch is a [`SourceBlobError`] naming the blob and,
//! when the caller knows it, the path; the bytes are never returned.
//!
//! `task lint:source-blob-reads` (`tools/lint_source_blob_reads.sh`) fails on
//! any production `SELECT` of `blob_bytes` from `source_blobs` outside this
//! module.

use rusqlite::{Connection, OptionalExtension};

use crate::construction::{Blob, Ref};
use crate::substrate::Hash;

/// The one statement that reads a source blob's bytes.
const SELECT_BLOB_BYTES: &str = "SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1";

/// Why a source blob's bytes were not returned.
#[derive(Debug)]
pub enum SourceBlobErrorKind {
    /// The stored bytes hash to `actual`, not to the key they are stored
    /// under: the row was changed under its key, or corrupted.
    Tampered { actual: Hash },
    /// The row holds no inline bytes: they were moved into chunks
    /// (`leyline-fs` CDC activation) and are read there.
    ChunkActivated,
    /// The read itself failed.
    Sql(rusqlite::Error),
}

/// A refused read of `source_blobs`: the blob, the path it was read for
/// (when the caller named one), and why.
#[derive(Debug)]
pub struct SourceBlobError {
    pub path: Option<String>,
    pub blob: Hash,
    pub kind: SourceBlobErrorKind,
}

impl std::fmt::Display for SourceBlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "source blob {}", self.blob)?;
        if let Some(path) = &self.path {
            write!(f, " for '{path}'")?;
        }
        match &self.kind {
            SourceBlobErrorKind::Tampered { actual } => write!(
                f,
                ": its stored bytes hash to {actual}, not to its content address; refused"
            ),
            SourceBlobErrorKind::ChunkActivated => write!(
                f,
                ": chunk-activated in source_blobs (no inline bytes); read it through leyline-fs"
            ),
            SourceBlobErrorKind::Sql(e) => write!(f, ": read failed: {e}"),
        }
    }
}

impl std::error::Error for SourceBlobError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            SourceBlobErrorKind::Sql(e) => Some(e),
            _ => None,
        }
    }
}

/// The verified bytes `source_blobs` holds under `blob`: `None` when it
/// holds no such row, an error when the row's bytes do not hash to `blob`.
pub fn read(conn: &Connection, blob: Hash) -> Result<Option<Vec<u8>>, SourceBlobError> {
    read_for(conn, None, blob)
}

/// [`read`], for the file at `path`: a refusal names the path.
pub fn read_at(
    conn: &Connection,
    path: &str,
    blob: Hash,
) -> Result<Option<Vec<u8>>, SourceBlobError> {
    read_for(conn, Some(path), blob)
}

fn read_for(
    conn: &Connection,
    path: Option<&str>,
    blob: Hash,
) -> Result<Option<Vec<u8>>, SourceBlobError> {
    let refuse = |kind| SourceBlobError {
        path: path.map(str::to_owned),
        blob,
        kind,
    };
    let row: Option<Option<Vec<u8>>> = conn
        .prepare_cached(SELECT_BLOB_BYTES)
        .and_then(|mut stmt| {
            stmt.query_row([blob.as_bytes().as_slice()], |r| r.get(0))
                .optional()
        })
        .map_err(|e| refuse(SourceBlobErrorKind::Sql(e)))?;
    match row {
        None => Ok(None),
        Some(None) => Err(refuse(SourceBlobErrorKind::ChunkActivated)),
        Some(Some(bytes)) => match Ref::<Blob>::verified(blob, &bytes) {
            Some(_) => Ok(Some(bytes)),
            None => Err(refuse(SourceBlobErrorKind::Tampered {
                actual: *Ref::<Blob>::derive(&bytes).as_hash(),
            })),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::substrate::ContentAddressed;

    /// A `source_blobs` table without the NOT NULL constraint, so a
    /// chunk-activated row (no inline bytes) can be written.
    fn store(rows: &[(Hash, Option<&[u8]>)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE source_blobs (blob_hash BLOB PRIMARY KEY, blob_bytes BLOB)",
        )
        .unwrap();
        for (hash, bytes) in rows {
            conn.execute(
                "INSERT INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
                rusqlite::params![hash.as_bytes().as_slice(), bytes],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn a_blob_whose_bytes_hash_to_its_key_is_returned() {
        let key = b"package a\n".hash();
        let conn = store(&[(key, Some(b"package a\n"))]);
        assert_eq!(
            read(&conn, key).unwrap().as_deref(),
            Some(&b"package a\n"[..])
        );
        assert_eq!(
            read_at(&conn, "a.go", key).unwrap().as_deref(),
            Some(&b"package a\n"[..])
        );
    }

    #[test]
    fn an_absent_blob_is_none() {
        let conn = store(&[]);
        assert_eq!(read(&conn, b"x".hash()).unwrap(), None);
    }

    #[test]
    fn bytes_changed_under_their_key_are_refused_naming_path_and_blob() {
        let key = b"package a\n".hash();
        let conn = store(&[(key, Some(b"package b\n"))]);
        let err = read_at(&conn, "a.go", key).unwrap_err();
        assert_eq!(err.path.as_deref(), Some("a.go"));
        assert_eq!(err.blob, key);
        assert!(matches!(
            err.kind,
            SourceBlobErrorKind::Tampered { actual } if actual == b"package b\n".hash()
        ));
        let msg = err.to_string();
        assert!(msg.contains("'a.go'"), "{msg}");
        assert!(msg.contains(&key.to_string()), "{msg}");
        assert!(msg.contains(&b"package b\n".hash().to_string()), "{msg}");
        assert!(msg.contains("content address"), "{msg}");

        let err = read(&conn, key).unwrap_err();
        assert_eq!(err.path, None);
        assert!(!err.to_string().contains(" for '"), "{err}");
    }

    #[test]
    fn a_chunk_activated_row_is_refused_not_served_empty() {
        let key = b"package a\n".hash();
        let conn = store(&[(key, None)]);
        let err = read(&conn, key).unwrap_err();
        assert!(matches!(err.kind, SourceBlobErrorKind::ChunkActivated));
        assert!(err.to_string().contains("chunk-activated"), "{err}");
    }

    #[test]
    fn a_failed_read_is_a_typed_sql_refusal() {
        let conn = Connection::open_in_memory().unwrap();
        let err = read_at(&conn, "a.go", b"x".hash()).unwrap_err();
        assert!(matches!(err.kind, SourceBlobErrorKind::Sql(_)));
        assert!(std::error::Error::source(&err).is_some());
        assert!(err.to_string().contains("read failed"), "{err}");
    }

    #[test]
    fn only_a_sql_refusal_has_a_source() {
        let key = b"package a\n".hash();
        let conn = store(&[(key, Some(b"package b\n"))]);
        assert!(std::error::Error::source(&read(&conn, key).unwrap_err()).is_none());
    }
}
