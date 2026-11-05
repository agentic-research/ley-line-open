//! Arena-local locators: [`Nid`], [`FileId`], [`DirId`], [`NameId`],
//! [`KindId`] and [`ContentId`] (ADR-0047 §2.2 and §2.3, step M0, bead
//! `ley-line-open-12e252`).
//!
//! A locator is an integer an arena assigned: a `nodes` primary key, an
//! interned `files`, `dirs`, `names` or `kinds` row, a `node_content` row.
//! It names a row only inside one arena generation. ADR-0047 calls this the
//! **Locates** edge, `(Ref<V>, k) → B`: a locator is valid only as the pair
//! (version, local key), and a bare locator replayed against another
//! generation resolves to a different row without error. So a locator is
//! not an address. It is not a content hash, it is not interchangeable with
//! another kind of locator, and it never reaches a wire field, a signed
//! claim or a cache key.
//!
//! The types carry that where the compiler can see it:
//!
//! - each locator is its own `repr(transparent)` newtype over `i64`, so a
//!   [`Nid`] passed where a [`FileId`] belongs, or a [`NameId`] where a
//!   [`KindId`] belongs, is a type error;
//! - there is no `From`/`Into` between locators, from `i64`, or from a
//!   content address; [`from_raw`](Nid::from_raw) and [`get`](Nid::get) are
//!   the explicit crossings, for SQL text the type cannot see into;
//! - the SQL impls ([`ToSql`], [`FromSql`]) are the only encodings: a
//!   locator is stored in the arena it belongs to and read back from it.
//!   There is no `serde` impl and no Cap'n Proto impl, so a locator cannot
//!   be serialized onto the wire by construction. `ley-line-open-80f037`
//!   F3 refuses the bare-`nid` leak at runtime; this makes it a compile
//!   error. A wire field that needs to name a node renders its display path
//!   ([`crate::node_path`]).
//!
//! Nothing moves: the stored integers, the nid encoding and every table
//! column are unchanged. The newtypes are `repr(transparent)` and bind and
//! read as the same SQLite `INTEGER`.
//!
//! # Positive control
//!
//! A locator round-trips through the arena it belongs to.
//!
//! ```
//! use leyline_schema::{FileId, Nid, create_schema, ensure_file_id, file_nid};
//! let conn = rusqlite::Connection::open_in_memory().unwrap();
//! create_schema(&conn).unwrap();
//! let file: FileId = ensure_file_id(&conn, "src/a.rs").unwrap();
//! let nid: Nid = file_nid(file, 3);
//! let back: Nid = conn
//!     .query_row("SELECT ?1", [nid], |r| r.get(0))
//!     .unwrap();
//! assert_eq!(back, nid);
//! ```
//!
//! # Locators are not interchangeable (compile-fail)
//!
//! A [`Nid`] is not a [`FileId`]:
//!
//! ```compile_fail,E0308
//! use leyline_schema::{create_schema, ensure_file_id, file_nid};
//! let conn = rusqlite::Connection::open_in_memory().unwrap();
//! create_schema(&conn).unwrap();
//! let nid = file_nid(ensure_file_id(&conn, "a.rs").unwrap(), 0);
//! let _ = file_nid(nid, 0);
//! ```
//!
//! A [`FileId`] is not a [`DirId`]:
//!
//! ```compile_fail,E0308
//! use leyline_schema::{create_schema, dir_nid, ensure_file_id};
//! let conn = rusqlite::Connection::open_in_memory().unwrap();
//! create_schema(&conn).unwrap();
//! let _ = dir_nid(ensure_file_id(&conn, "a.rs").unwrap());
//! ```
//!
//! A [`KindId`] is not a [`NameId`]: swapping the two interned ids in a row
//! insert does not compile.
//!
//! ```compile_fail,E0308
//! use leyline_schema::{create_schema, ensure_file_id, file_nid, insert_node, intern_kind, intern_name};
//! let conn = rusqlite::Connection::open_in_memory().unwrap();
//! create_schema(&conn).unwrap();
//! let nid = file_nid(ensure_file_id(&conn, "a.rs").unwrap(), 1);
//! let kind = intern_kind(&conn, "rust", "identifier").unwrap();
//! let name = intern_name(&conn, "main").unwrap();
//! insert_node(&conn, nid, None, Some(kind), Some(name), 0, 0, 0, 0, "").unwrap();
//! ```
//!
//! A locator is not an integer, and an integer is not a locator, without the
//! explicit crossing:
//!
//! ```compile_fail,E0308
//! use leyline_schema::{create_schema, node_path};
//! let conn = rusqlite::Connection::open_in_memory().unwrap();
//! create_schema(&conn).unwrap();
//! let _ = node_path(&conn, -1_i64);
//! ```
//!
//! ```compile_fail,E0277
//! let _: leyline_schema::Nid = 5_i64.into();
//! ```
//!
//! A content address (32 bytes) is not a locator:
//!
//! ```compile_fail,E0277
//! let _: leyline_schema::Nid = [0u8; 32].into();
//! ```
//!
//! # Locators do not reach the wire (compile-fail)
//!
//! No locator implements `serde::Serialize`, so none can be encoded into a
//! JSON (or any serde) wire value:
//!
//! ```compile_fail,E0277
//! use leyline_schema::{create_schema, ensure_file_id, file_nid};
//! let conn = rusqlite::Connection::open_in_memory().unwrap();
//! create_schema(&conn).unwrap();
//! let nid = file_nid(ensure_file_id(&conn, "a.rs").unwrap(), 0);
//! let _ = serde_json::to_vec(&nid);
//! ```
//!
//! ```compile_fail,E0277
//! use leyline_schema::{create_schema, ensure_file_id};
//! let conn = rusqlite::Connection::open_in_memory().unwrap();
//! create_schema(&conn).unwrap();
//! let _ = serde_json::to_vec(&ensure_file_id(&conn, "a.rs").unwrap());
//! ```

use std::fmt;

use rusqlite::ToSql;
use rusqlite::types::{FromSql, FromSqlResult, ToSqlOutput, ValueRef};

macro_rules! locator {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        ///
        /// Arena-local: valid only with the arena generation it was read
        /// from. SQL impls only; no `serde` or Cap'n Proto impls (see the
        /// [module docs](self)).
        #[repr(transparent)]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $name(i64);

        impl $name {
            /// The locator stored as `raw` in this arena. The explicit
            /// crossing for an integer the type cannot see: a column read
            /// in SQL text, a literal in a fixture.
            #[inline]
            pub const fn from_raw(raw: i64) -> Self {
                Self(raw)
            }

            /// The stored integer, for arithmetic or SQL text the type
            /// cannot see into. Leaving the type is explicit.
            #[inline]
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        /// Diagnostics only (error messages, logs). A wire field names a
        /// node by its display path, never by this rendering.
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl ToSql for $name {
            #[inline]
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                self.0.to_sql()
            }
        }

        impl FromSql for $name {
            #[inline]
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                i64::column_result(value).map(Self)
            }
        }
    };
}

locator! {
    /// A `nodes` row: `(file_id << 24) | ordinal` for files and their AST
    /// nodes, `-dir_id` for directories ([`crate::file_nid`],
    /// [`crate::dir_nid`]).
    Nid
}

locator! {
    /// An interned `files` row (`files.file_id`). Its nid range is
    /// [`crate::file_nid_range`].
    FileId
}

locator! {
    /// An interned `dirs` row (`dirs.dir_id`); [`DirId::ROOT`] is the tree
    /// root.
    DirId
}

locator! {
    /// An interned `names` row (`names.name_id`): one path component.
    NameId
}

locator! {
    /// An interned `kinds` row (`kinds.kind_id`): one tree-sitter kind
    /// under its language.
    KindId
}

locator! {
    /// A `node_content` row (`node_content.content_id`, projection-v10):
    /// the arena-local handle of a deduplicated subtree. It is a row key,
    /// not the subtree's content address: the address is the row's
    /// `node_hash` column.
    ContentId
}

impl DirId {
    /// `dirs.dir_id = 1`: the tree root, seeded by every schema pass.
    pub const ROOT: DirId = DirId(1);
}

/// `file_nid(f, o) + k == file_nid(f, o + k)`: a step within one file's
/// nid range, by pre-order ordinal. The result is still a [`Nid`]; there is
/// no arithmetic between locators of different kinds.
impl std::ops::Add<i64> for Nid {
    type Output = Nid;

    #[inline]
    fn add(self, ordinals: i64) -> Nid {
        Nid(self.0 + ordinals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn every_locator_binds_and_reads_as_the_same_sqlite_integer() {
        let conn = Connection::open_in_memory().unwrap();
        for raw in [i64::MIN, -7, 0, 1, (3 << 24) | 41, i64::MAX] {
            let (nid, file, dir, name, kind, content): (
                Nid,
                FileId,
                DirId,
                NameId,
                KindId,
                ContentId,
            ) = conn
                .query_row(
                    "SELECT ?1, ?2, ?3, ?4, ?5, ?6",
                    rusqlite::params![
                        Nid::from_raw(raw),
                        FileId::from_raw(raw),
                        DirId::from_raw(raw),
                        NameId::from_raw(raw),
                        KindId::from_raw(raw),
                        ContentId::from_raw(raw),
                    ],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(
                [
                    nid.get(),
                    file.get(),
                    dir.get(),
                    name.get(),
                    kind.get(),
                    content.get()
                ],
                [raw; 6]
            );
            let ty: String = conn
                .query_row("SELECT typeof(?1)", [Nid::from_raw(raw)], |r| r.get(0))
                .unwrap();
            assert_eq!(
                ty, "integer",
                "a locator binds as INTEGER, as the raw i64 did"
            );
        }
    }

    #[test]
    fn a_non_integer_column_is_refused_not_coerced() {
        let conn = Connection::open_in_memory().unwrap();
        let got: rusqlite::Result<Nid> = conn.query_row("SELECT 'x'", [], |r| r.get(0));
        assert!(got.is_err(), "a TEXT value is not a locator");
        let got: rusqlite::Result<Option<FileId>> = conn.query_row("SELECT NULL", [], |r| r.get(0));
        assert_eq!(got.unwrap(), None, "NULL reads as an absent locator");
    }

    #[test]
    fn adding_ordinals_steps_within_a_file_range() {
        let base = crate::file_nid(FileId::from_raw(3), 0);
        assert_eq!(base + 0, base);
        assert_eq!(base + 5, crate::file_nid(FileId::from_raw(3), 5));
        assert_eq!((base + 5).get() - base.get(), 5);
    }

    #[test]
    fn display_is_the_stored_integer() {
        assert_eq!(Nid::from_raw(-1).to_string(), "-1");
        assert_eq!(FileId::from_raw(42).to_string(), "42");
        assert_eq!(DirId::ROOT.get(), 1);
    }
}
