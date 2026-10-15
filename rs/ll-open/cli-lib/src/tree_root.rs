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

/// One leaf of the store's manifest as the fold sees it (ADR-0045 T2): a
/// file entry, or an empty directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeLeaf {
    /// The path relative to the tree root.
    pub path: String,
    /// The entry's git mode (`0o100644`, `0o100755`, `0o120000`, or
    /// `0o040000` for an empty directory).
    pub mode: u32,
    /// The file's blob hash; BLAKE3 of no bytes for an empty directory.
    pub content: Hash,
}

impl TreeLeaf {
    /// A regular (`0o100644`) file: the only leaf a pre-ADR-0045 tree held.
    pub fn regular(path: impl Into<String>, content: Hash) -> Self {
        TreeLeaf {
            path: path.into(),
            mode: 0o100644,
            content,
        }
    }
}

/// `b` of a leaf's content entry: `0` for a regular file, so every tree a
/// pre-ADR-0045 producer folded keeps its root, and the git mode otherwise.
fn mode_tag(mode: u32) -> u64 {
    if mode == 0o100644 { 0 } else { u64::from(mode) }
}

/// `treeRoot` over an explicit leaf set. Pure, so a receiver holding only a
/// manifest (ADR-0029) recomputes it without an arena.
pub fn tree_root_of(leaves: &[TreeLeaf], parser_id: Hash) -> Hash {
    let mut sorted: Vec<&TreeLeaf> = leaves.iter().collect();
    sorted.sort_by(|x, y| x.path.as_bytes().cmp(y.path.as_bytes()));
    let mut entries = Vec::with_capacity(sorted.len() * 2);
    for (i, leaf) in sorted.iter().enumerate() {
        let i = i as u64;
        entries.push(Entry {
            addr: leaf.path.as_bytes().hash(),
            a: 2 * i,
            b: leaf.path.len() as u64,
        });
        entries.push(Entry {
            addr: leaf.content,
            a: 2 * i + 1,
            b: mode_tag(leaf.mode),
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

/// The fold's leaves of a manifest: every file entry, and every directory
/// with no entry beneath it.
pub fn leaves_of(
    entries: &std::collections::BTreeMap<String, crate::tree::Entry>,
) -> Vec<TreeLeaf> {
    use crate::tree::Entry as E;
    let mut leaves = Vec::with_capacity(entries.len());
    for (path, entry) in entries {
        match entry {
            E::File { mode, blob } => leaves.push(TreeLeaf {
                path: path.clone(),
                mode: mode.git(),
                content: *blob,
            }),
            E::Dir => {
                let prefix = format!("{path}/");
                let empty = !entries
                    .range(prefix.clone()..)
                    .next()
                    .is_some_and(|(k, _)| k.starts_with(&prefix));
                if empty {
                    leaves.push(TreeLeaf {
                        path: path.clone(),
                        mode: crate::tree::DIRECTORY_MODE,
                        content: b"".hash(),
                    });
                }
            }
        }
    }
    leaves
}

/// Read every leaf of the arena's manifest (`_tree`). Fails closed on an
/// entry git does not store or a file without a 32-byte blob hash: a tree
/// root over an unknown file is not a tree root.
pub fn tree_leaves(conn: &Connection) -> Result<Vec<TreeLeaf>> {
    let entries = crate::tree::entries(conn).context("read _tree for treeRoot")?;
    Ok(leaves_of(&entries))
}

/// `treeRoot` of an arena under `parser_id`.
pub fn tree_root(conn: &Connection, parser_id: Hash) -> Result<Hash> {
    Ok(tree_root_of(&tree_leaves(conn)?, parser_id))
}

/// The `_meta` key [`DerivedFrom`] is stored under.
pub const DERIVED_FROM_KEY: &str = "derived_from";

/// ADR-0040 D6: what an arena's projection was derived from. Every parse
/// stamps it into `_meta` beside the head; a consumer that trusts a
/// transported arena compares it to the `treeRoot` it holds, and `leyline
/// rebuild` reproduces the projection from it.
///
/// Stored as one `_meta` value, `<treeRoot hex> <parserId hex>
/// <projection_schema_version>`, so the three fields are never read from
/// different parses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedFrom {
    pub tree_root: Hash,
    pub parser_id: Hash,
    pub projection_schema_version: String,
}

impl DerivedFrom {
    pub fn to_meta(&self) -> String {
        format!(
            "{} {} {}",
            self.tree_root, self.parser_id, self.projection_schema_version
        )
    }

    pub fn from_meta(value: &str) -> Result<Self> {
        let fields: Vec<&str> = value.split(' ').collect();
        let [tree_root, parser_id, projection_schema_version] = fields[..] else {
            bail!("_meta.{DERIVED_FROM_KEY} {value:?} is not three space-separated fields");
        };
        let hash = |name: &str, hex_str: &str| -> Result<Hash> {
            let bytes: [u8; 32] = hex::decode(hex_str)
                .ok()
                .and_then(|b| b.try_into().ok())
                .with_context(|| {
                    format!("_meta.{DERIVED_FROM_KEY} {name} {hex_str:?} is not 32 hex bytes")
                })?;
            Ok(Hash::from_bytes(bytes))
        };
        Ok(Self {
            tree_root: hash("treeRoot", tree_root)?,
            parser_id: hash("parserId", parser_id)?,
            projection_schema_version: projection_schema_version.to_string(),
        })
    }

    /// The arena's stamp, or `None` for an arena parsed before ADR-0040 D6.
    pub fn read(conn: &Connection) -> Result<Option<Self>> {
        leyline_ts::schema::get_meta(conn, DERIVED_FROM_KEY)?
            .map(|v| Self::from_meta(&v))
            .transpose()
    }
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

    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
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

    fn leaf(path: &str, byte: u8) -> TreeLeaf {
        TreeLeaf::regular(path, Hash::from_bytes([byte; 32]))
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
    fn tree_leaves_fail_closed_on_an_entry_without_a_hash() {
        let conn = Connection::open_in_memory().unwrap();
        crate::tree::ensure_tables(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO _tree (path, mode, blob_hash) VALUES ('a.go', 33188, NULL);",
        )
        .unwrap();
        let err = format!("{:#}", tree_leaves(&conn).unwrap_err());
        assert!(err.contains("no blob_hash"), "{err}");
        conn.execute_batch("UPDATE _tree SET blob_hash = X'0102' WHERE path = 'a.go';")
            .unwrap();
        let err = format!("{:#}", tree_leaves(&conn).unwrap_err());
        assert!(err.contains("expected 32"), "{err}");
    }

    /// ADR-0045 T2: a leaf's content entry carries its git mode in `b`,
    /// except a regular file, which carries the `0` every pre-ADR leaf
    /// carried; an empty directory's content is BLAKE3 of no bytes.
    #[test]
    fn modes_and_empty_dirs_move_the_tree_root() {
        let p = inputs().parser_id();
        let leaves = [
            TreeLeaf {
                path: "run.sh".into(),
                mode: 0o100755,
                content: Hash::from_bytes([1; 32]),
            },
            TreeLeaf {
                path: "link".into(),
                mode: 0o120000,
                content: b"run.sh".hash(),
            },
            TreeLeaf {
                path: "empty".into(),
                mode: 0o040000,
                content: b"".hash(),
            },
        ];
        let entry = |addr: Hash, a: u64, b: u64| Entry { addr, a, b };
        let expected = PartitionSpec {
            domain: Domain::RowSet,
            scheme: "leyline/tree-root/v1".to_string(),
            params: p.as_bytes().to_vec(),
            canon_version: 1,
        }
        .address(&[
            entry(b"empty".hash(), 0, 5),
            entry(b"".hash(), 1, 16384),
            entry(b"link".hash(), 2, 4),
            entry(b"run.sh".hash(), 3, 40960),
            entry(b"run.sh".hash(), 4, 6),
            entry(Hash::from_bytes([1; 32]), 5, 33261),
        ]);
        assert_eq!(tree_root_of(&leaves, p), expected);
        // The same bytes as a regular file fold differently.
        let regular = TreeLeaf::regular("run.sh", Hash::from_bytes([1; 32]));
        let executable = leaves[0].clone();
        assert_ne!(tree_root_of(&[regular], p), tree_root_of(&[executable], p));
    }

    /// The leaves of a manifest are its files and its empty directories; a
    /// directory with an entry beneath it is implied, and a tree of regular
    /// files with no empty directory folds exactly as `_source` folded.
    #[test]
    fn a_default_tree_folds_as_before_modes_and_dirs() {
        use crate::tree::{Entry as E, Mode};
        let h = |b: u8| Hash::from_bytes([b; 32]);
        let manifest = std::collections::BTreeMap::from([
            ("a".to_string(), E::Dir),
            (
                "a/b.go".to_string(),
                E::File {
                    mode: Mode::Regular,
                    blob: h(1),
                },
            ),
            ("a-c".to_string(), E::Dir),
            (
                "c.go".to_string(),
                E::File {
                    mode: Mode::Regular,
                    blob: h(2),
                },
            ),
        ]);
        let p = inputs().parser_id();
        let leaves = leaves_of(&manifest);
        assert_eq!(
            leaves,
            vec![
                TreeLeaf {
                    path: "a-c".into(),
                    mode: 0o040000,
                    content: b"".hash(),
                },
                TreeLeaf::regular("a/b.go", h(1)),
                TreeLeaf::regular("c.go", h(2)),
            ]
        );
        let mut without_empty = manifest.clone();
        without_empty.remove("a-c");
        assert_eq!(
            tree_root_of(&leaves_of(&without_empty), p),
            tree_root_of(
                &[
                    TreeLeaf::regular("a/b.go", h(1)),
                    TreeLeaf::regular("c.go", h(2))
                ],
                p
            ),
            "directories with files beneath them are not leaves"
        );
    }
}
