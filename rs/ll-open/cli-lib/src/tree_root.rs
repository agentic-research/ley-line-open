//! ADR-0040 D1 — `treeRoot` and `parserId`: the tree identity and the
//! derivation identity, written into `Head` beside the run receipt
//! `rootHash` (bead `ley-line-open-f2df7f`).
//!
//! `Head.rootHash` folds over the capnp segments ONE RUN wrote. The segment
//! files are truncated per run and unchanged files emit no records, so after
//! an incremental parse the root covers only the re-parsed files (bead
//! `0c80c7`); the source record also carries the absolute path, mtime and
//! size, so the same bytes in another directory hash differently (bead
//! `143002`). It is a run receipt, and it stays one.
//!
//! `treeRoot` names the tree. It is a tagged fold (`PartitionSpec`, ADR-0032
//! D2) over every `_source` row, computed after COMMIT from the arena rather
//! than from what this run happened to write, with no path other than the
//! relative one and no timestamp. Four parses of the same bytes — a second
//! directory, touched mtimes, shuffled discovery, a no-op scoped reparse —
//! yield one `treeRoot`. One changed byte changes it.
//!
//! `parserId` names the function. Same source under a different grammar,
//! IR schema, extraction epoch or query-set override is a different
//! derivation; it is the spec's `params`, so `treeRoot` already differs
//! across derivations, and it is also stamped on its own so a consumer can
//! tell "same tree, different parser" from "different tree".
//!
//! `verify` recomputes both from an arena and compares them to its head.
//! A head with the fields unset is reported as such, not treated as valid.

use std::path::Path;

use anyhow::{Context, Result, bail};
use leyline_core::{ContentAddressed, Domain, Entry, Hash, PartitionSpec};
use rusqlite::Connection;

/// Scheme tag of the tree-root fold. Protocol-visible: changing the entry
/// shape, the ordering rule or the params layout is a `v2`, not an edit.
pub const TREE_ROOT_SCHEME: &str = "leyline/tree-root/v1";

/// Scheme tag of the parser-identity fold. Same rule as above: a new input
/// kind is a `v2`.
pub const PARSER_ID_SCHEME: &str = "leyline/parser-id/v1";

/// The inputs `parserId` folds over. Every field is something a change to
/// which would make the same source project differently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParserInputs {
    pub ir_schema_version: String,
    pub projection_schema_version: String,
    pub extraction_epoch: String,
    pub injection_epoch: String,
    /// `query_set_epoch`: the content address of the active per-arena query
    /// overrides (compiled defaults contribute a stable constant).
    pub query_set_epoch: String,
    /// `(language name, grammar digest)` for every compiled grammar, any
    /// order; the fold canonicalizes.
    pub grammars: Vec<(String, Hash)>,
}

/// Input kinds, as the `a` framing of each entry. Values are protocol-
/// visible — append, never renumber.
mod input_kind {
    pub const IR_SCHEMA_VERSION: u64 = 0;
    pub const PROJECTION_SCHEMA_VERSION: u64 = 1;
    pub const EXTRACTION_EPOCH: u64 = 2;
    pub const INJECTION_EPOCH: u64 = 3;
    pub const QUERY_SET_EPOCH: u64 = 4;
    pub const GRAMMAR: u64 = 5;
}

impl ParserInputs {
    /// The inputs of THIS binary against THIS arena's effective query set.
    pub fn current(query_set: &leyline_ts::query_engine::QuerySet) -> Self {
        Self {
            ir_schema_version: crate::daemon::version::IR_SCHEMA_VERSION.to_string(),
            projection_schema_version: crate::daemon::version::PROJECTION_SCHEMA_VERSION
                .to_string(),
            extraction_epoch: leyline_ts::refs::current_extraction_epoch().to_string(),
            injection_epoch: leyline_ts::injections::current_injection_epoch(),
            query_set_epoch: leyline_ts::query_engine::query_set_epoch(query_set),
            grammars: leyline_ts::languages::TsLanguage::all()
                .into_iter()
                .map(|l| (l.name().to_string(), l.grammar_digest()))
                .collect(),
        }
    }

    /// `parserId`: the tagged fold over the inputs.
    pub fn parser_id(&self) -> Hash {
        let scalar = |kind: u64, value: &str| Entry {
            addr: value.as_bytes().hash(),
            a: kind,
            b: value.len() as u64,
        };
        let mut entries = vec![
            scalar(input_kind::IR_SCHEMA_VERSION, &self.ir_schema_version),
            scalar(
                input_kind::PROJECTION_SCHEMA_VERSION,
                &self.projection_schema_version,
            ),
            scalar(input_kind::EXTRACTION_EPOCH, &self.extraction_epoch),
            scalar(input_kind::INJECTION_EPOCH, &self.injection_epoch),
            scalar(input_kind::QUERY_SET_EPOCH, &self.query_set_epoch),
        ];
        for (name, digest) in &self.grammars {
            // The grammar digest already covers the name; `b` carries the
            // name's length so two grammars with equal tables and different
            // names cannot collapse to one entry even if a digest ever did.
            entries.push(Entry {
                addr: *digest,
                a: input_kind::GRAMMAR,
                b: name.len() as u64,
            });
        }
        PartitionSpec {
            domain: Domain::RowSet,
            scheme: PARSER_ID_SCHEME.to_string(),
            params: Vec::new(),
            canon_version: 1,
        }
        .address(&entries)
    }
}

/// One `_source` row as the fold sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLeaf {
    /// `_source.id`: the path relative to the parse root.
    pub relative_path: String,
    /// `_source.content_hash`: BLAKE3 of the file bytes.
    pub content_hash: Hash,
}

/// `treeRoot` over an explicit leaf set. Pure, so a receiver holding only a
/// manifest (ADR-0029) recomputes it without an arena.
pub fn tree_root_of(leaves: &[SourceLeaf], parser_id: Hash) -> Hash {
    let mut sorted: Vec<&SourceLeaf> = leaves.iter().collect();
    sorted.sort_by(|x, y| x.relative_path.as_bytes().cmp(y.relative_path.as_bytes()));
    let mut entries = Vec::with_capacity(sorted.len() * 2);
    for (i, leaf) in sorted.iter().enumerate() {
        let i = i as u64;
        entries.push(Entry {
            addr: leaf.relative_path.as_bytes().hash(),
            a: 2 * i,
            b: leaf.relative_path.len() as u64,
        });
        entries.push(Entry {
            addr: leaf.content_hash,
            a: 2 * i + 1,
            b: 0,
        });
    }
    PartitionSpec {
        domain: Domain::RowSet,
        scheme: TREE_ROOT_SCHEME.to_string(),
        params: parser_id.as_bytes().to_vec(),
        canon_version: 1,
    }
    .address(&entries)
}

/// Read every `_source` leaf from an arena. Fails closed on a row without a
/// 32-byte content hash: a tree root over an unknown file is not a tree root.
pub fn source_leaves(conn: &Connection) -> Result<Vec<SourceLeaf>> {
    let mut stmt = conn
        .prepare("SELECT id, content_hash FROM _source ORDER BY id")
        .context("prepare _source scan for treeRoot")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
    })?;
    let mut leaves = Vec::new();
    for row in rows {
        let (id, hash) = row?;
        let Some(bytes) = hash else {
            bail!("treeRoot: _source row {id:?} has no content_hash");
        };
        let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "treeRoot: _source row {id:?} content_hash is {} bytes, expected 32",
                bytes.len()
            )
        })?;
        leaves.push(SourceLeaf {
            relative_path: id,
            content_hash: Hash::from_bytes(bytes),
        });
    }
    Ok(leaves)
}

/// `treeRoot` of an arena under `parser_id`.
pub fn tree_root(conn: &Connection, parser_id: Hash) -> Result<Hash> {
    Ok(tree_root_of(&source_leaves(conn)?, parser_id))
}

/// What `verify` found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verification {
    /// Both stamped roots match a recomputation from the arena.
    Valid { tree_root: Hash, parser_id: Hash },
    /// The head predates ADR-0040 and carries neither field.
    NotStamped,
    /// The arena's source set does not fold to the stamped `treeRoot`
    /// under the stamped `parserId`: the tree changed after the head was
    /// written, or the head is not this arena's.
    TreeRootMismatch { stamped: Hash, recomputed: Hash },
    /// The stamped `parserId` is not this binary's against this arena's
    /// query overrides: same tree, different derivation. Reported before
    /// the tree root, because the tree root is only comparable under one
    /// `parserId`.
    ParserIdMismatch { stamped: Hash, recomputed: Hash },
}

/// Recompute `treeRoot` and `parserId` from `db_path` and compare them to the
/// sibling `head.capnp`. Read-only.
pub fn verify(db_path: &Path) -> Result<Verification> {
    use leyline_schema_capnp::head_capnp::head;
    let head_path = crate::cmd_parse::head_path_for(db_path);
    let bytes =
        std::fs::read(&head_path).with_context(|| format!("read head {}", head_path.display()))?;
    let mut slice: &[u8] = &bytes;
    let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
        .with_context(|| format!("parse head {}", head_path.display()))?;
    let h: head::Reader = msg
        .get_root()
        .with_context(|| format!("read head root {}", head_path.display()))?;
    let stamped_tree = hash_field(h.get_tree_root().ok().map(|x| x.get_bytes()))?;
    let stamped_parser = hash_field(h.get_parser_id().ok().map(|x| x.get_bytes()))?;
    let (Some(stamped_tree), Some(stamped_parser)) = (stamped_tree, stamped_parser) else {
        return Ok(Verification::NotStamped);
    };

    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {} read-only", db_path.display()))?;
    let trusted = crate::cmd_parse::trusted_query_hashes_from_env();
    let resolution = leyline_ts::query_engine::resolve_query_set(&conn, &trusted)?;
    let recomputed_parser = ParserInputs::current(&resolution.query_set).parser_id();
    if recomputed_parser != stamped_parser {
        return Ok(Verification::ParserIdMismatch {
            stamped: stamped_parser,
            recomputed: recomputed_parser,
        });
    }
    let recomputed_tree = tree_root(&conn, stamped_parser)?;
    if recomputed_tree != stamped_tree {
        return Ok(Verification::TreeRootMismatch {
            stamped: stamped_tree,
            recomputed: recomputed_tree,
        });
    }
    Ok(Verification::Valid {
        tree_root: stamped_tree,
        parser_id: stamped_parser,
    })
}

/// A `Common.Hash` field: absent or all-zero means "not stamped"; any other
/// length than 32 is a malformed head and an error, never a silent default.
fn hash_field(field: Option<capnp::Result<&[u8]>>) -> Result<Option<Hash>> {
    let Some(field) = field else {
        return Ok(None);
    };
    let bytes = field.context("read head hash field")?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("head hash field is {} bytes, expected 32", bytes.len()))?;
    let h = Hash::from_bytes(arr);
    Ok(if h == Hash::ZERO { None } else { Some(h) })
}

/// `leyline verify-head <db>`: print the verdict; exit non-zero unless valid.
pub fn cmd_verify_head(db: &Path) -> Result<()> {
    match verify(db)? {
        Verification::Valid {
            tree_root,
            parser_id,
        } => {
            println!("valid treeRoot={tree_root} parserId={parser_id}");
            Ok(())
        }
        Verification::NotStamped => bail!(
            "head beside {} carries no treeRoot/parserId (written before ADR-0040); reparse to stamp it",
            db.display()
        ),
        Verification::ParserIdMismatch {
            stamped,
            recomputed,
        } => bail!(
            "parserId mismatch: head stamped {stamped}, this binary against this arena's overrides computes {recomputed}; same tree, different derivation"
        ),
        Verification::TreeRootMismatch {
            stamped,
            recomputed,
        } => bail!(
            "treeRoot mismatch: head stamped {stamped}, the arena's _source set folds to {recomputed}; the tree changed after the head was written or the head is not this arena's"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(path: &str, byte: u8) -> SourceLeaf {
        SourceLeaf {
            relative_path: path.to_string(),
            content_hash: Hash::from_bytes([byte; 32]),
        }
    }

    fn inputs() -> ParserInputs {
        ParserInputs {
            ir_schema_version: "merkle-ast-v2".into(),
            projection_schema_version: "projection-v6".into(),
            extraction_epoch: "5".into(),
            injection_epoch: "inj".into(),
            query_set_epoch: "qse".into(),
            grammars: vec![
                ("go".into(), Hash::from_bytes([1; 32])),
                ("rust".into(), Hash::from_bytes([2; 32])),
            ],
        }
    }

    /// `treeRoot` is exactly the declared fold: an independent reconstruction
    /// with literal framing (`2i` for the path entry, `2i+1` for the content
    /// entry, `i` the rank in path order, `parserId` as params) reproduces it.
    #[test]
    fn tree_root_is_the_declared_fold() {
        let p = inputs().parser_id();
        let leaves = [leaf("b.go", 2), leaf("a.go", 1), leaf("c/d.go", 3)];
        let expected = PartitionSpec {
            domain: Domain::RowSet,
            scheme: "leyline/tree-root/v1".to_string(),
            params: p.as_bytes().to_vec(),
            canon_version: 1,
        }
        .address(&[
            Entry {
                addr: b"a.go".hash(),
                a: 0,
                b: 4,
            },
            Entry {
                addr: Hash::from_bytes([1; 32]),
                a: 1,
                b: 0,
            },
            Entry {
                addr: b"b.go".hash(),
                a: 2,
                b: 4,
            },
            Entry {
                addr: Hash::from_bytes([2; 32]),
                a: 3,
                b: 0,
            },
            Entry {
                addr: b"c/d.go".hash(),
                a: 4,
                b: 6,
            },
            Entry {
                addr: Hash::from_bytes([3; 32]),
                a: 5,
                b: 0,
            },
        ]);
        assert_eq!(tree_root_of(&leaves, p), expected);
    }

    /// `parserId` is exactly the declared fold over its six input kinds.
    #[test]
    fn parser_id_is_the_declared_fold() {
        let i = inputs();
        let scalar = |kind: u64, v: &str| Entry {
            addr: v.as_bytes().hash(),
            a: kind,
            b: v.len() as u64,
        };
        let expected = PartitionSpec {
            domain: Domain::RowSet,
            scheme: "leyline/parser-id/v1".to_string(),
            params: Vec::new(),
            canon_version: 1,
        }
        .address(&[
            scalar(0, "merkle-ast-v2"),
            scalar(1, "projection-v6"),
            scalar(2, "5"),
            scalar(3, "inj"),
            scalar(4, "qse"),
            Entry {
                addr: Hash::from_bytes([1; 32]),
                a: 5,
                b: 2,
            },
            Entry {
                addr: Hash::from_bytes([2; 32]),
                a: 5,
                b: 4,
            },
        ]);
        assert_eq!(i.parser_id(), expected);
    }

    #[test]
    fn tree_root_is_independent_of_leaf_enumeration_order() {
        let p = inputs().parser_id();
        let a = tree_root_of(&[leaf("a.go", 1), leaf("b.go", 2)], p);
        let b = tree_root_of(&[leaf("b.go", 2), leaf("a.go", 1)], p);
        assert_eq!(a, b);
    }

    #[test]
    fn tree_root_binds_path_to_content() {
        let p = inputs().parser_id();
        let straight = tree_root_of(&[leaf("a.go", 1), leaf("b.go", 2)], p);
        let swapped = tree_root_of(&[leaf("a.go", 2), leaf("b.go", 1)], p);
        assert_ne!(
            straight, swapped,
            "same multiset of hashes, different mapping"
        );
    }

    #[test]
    fn tree_root_moves_with_one_byte_one_path_or_one_parser_input() {
        let p = inputs().parser_id();
        let base = tree_root_of(&[leaf("a.go", 1), leaf("b.go", 2)], p);
        assert_ne!(base, tree_root_of(&[leaf("a.go", 1), leaf("b.go", 3)], p));
        assert_ne!(base, tree_root_of(&[leaf("a.go", 1), leaf("c.go", 2)], p));
        let mut other = inputs();
        other.extraction_epoch = "6".into();
        assert_ne!(
            base,
            tree_root_of(&[leaf("a.go", 1), leaf("b.go", 2)], other.parser_id())
        );
    }

    #[test]
    fn tree_root_distinguishes_a_path_prefix_split() {
        // The relative path is length-framed, so "ab"+"c" and "a"+"bc" as
        // two different trees cannot collide through concatenation.
        let p = inputs().parser_id();
        assert_ne!(
            tree_root_of(&[leaf("ab", 1), leaf("c", 1)], p),
            tree_root_of(&[leaf("a", 1), leaf("bc", 1)], p)
        );
    }

    #[test]
    fn parser_id_moves_when_any_input_moves_and_ignores_grammar_order() {
        let base = inputs().parser_id();
        let mut m = inputs();
        m.ir_schema_version = "merkle-ast-v3".into();
        assert_ne!(base, m.parser_id());
        let mut m = inputs();
        m.projection_schema_version = "projection-v7".into();
        assert_ne!(base, m.parser_id());
        let mut m = inputs();
        m.injection_epoch = "inj2".into();
        assert_ne!(base, m.parser_id());
        let mut m = inputs();
        m.query_set_epoch = "qse2".into();
        assert_ne!(base, m.parser_id());
        let mut m = inputs();
        m.grammars[0].1 = Hash::from_bytes([9; 32]);
        assert_ne!(base, m.parser_id());
        let mut m = inputs();
        m.grammars.reverse();
        assert_eq!(
            base,
            m.parser_id(),
            "grammar enumeration order is not identity"
        );
    }

    #[test]
    fn parser_id_and_tree_root_are_domain_separated() {
        // The two schemes never collide even over degenerate inputs.
        let p = inputs().parser_id();
        assert_ne!(p, tree_root_of(&[], p));
    }

    #[test]
    fn source_leaves_fail_closed_on_a_row_without_a_hash() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _source (id TEXT PRIMARY KEY, content_hash BLOB);
             INSERT INTO _source VALUES ('a.go', NULL);",
        )
        .unwrap();
        let err = source_leaves(&conn).unwrap_err().to_string();
        assert!(err.contains("no content_hash"), "{err}");
        conn.execute_batch("UPDATE _source SET content_hash = X'0102' WHERE id = 'a.go';")
            .unwrap();
        let err = source_leaves(&conn).unwrap_err().to_string();
        assert!(err.contains("expected 32"), "{err}");
    }
}
