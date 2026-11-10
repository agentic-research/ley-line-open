//! ADR-0047 D-ii (step M1, bead `ley-line-open-73c6ee`) — `storeRoot` and
//! `parserId`: the store identity and the derivation identity, written into
//! `Head` beside the run receipt `rootHash`.
//!
//! `Head.rootHash` folds over the capnp segments ONE RUN wrote. The segment
//! files are truncated per run and unchanged files emit no records, so after
//! an incremental parse the root covers only the re-parsed files (bead
//! `0c80c7`); the source record also carries the absolute path, mtime and
//! size, so the same bytes in another directory hash differently (bead
//! `143002`). It is a run receipt, and it stays one.
//!
//! `storeRoot` names the store. It is a directory Merkle tree over every
//! `_tree` entry — (relative path, git mode, blob hash), with directories
//! as entries (ADR-0045 T1/T2) — computed after COMMIT from the arena
//! rather than from what this run happened to write, with no path other
//! than the relative one, no timestamp and no parser input. Each directory
//! node is a tagged fold (`PartitionSpec`, ADR-0032 D2, scheme
//! `leyline/store-tree/v1`, params empty) over its direct children sorted
//! by name bytes: child `i` contributes `(σ_bytes(name), 2i, |name|)` and
//! `(child, 2i + 1, git mode)`, where `child` is the file's blob hash or
//! the child directory's node address. An empty directory is the fold of
//! no entries; `storeRoot` is the root directory's address, and a
//! subtree's address is the `storeRoot` of a store holding exactly that
//! subtree. A file of no grammar has a `_tree` entry and no `_source` row,
//! and still moves the root. Four parses of the same bytes — a second
//! directory, touched mtimes, shuffled discovery, a no-op scoped reparse —
//! yield one `storeRoot`. One changed byte changes it, and refolds only the
//! directory nodes on that file's path (Σ §1.3).
//!
//! `parserId` names the function. Same source under a different grammar,
//! IR schema, extraction epoch or query-set override is a different
//! derivation of the SAME store: `storeRoot` does not move (ADR-0047 law
//! C7, parameter independence), and the parser-qualified identity is the
//! pair, bound by the head digest v3 and by the `derived_from` triple
//! `(storeRoot, parserId, projection_schema_version)`. The retired
//! `leyline/tree-root/v1` folded `parserId` into its params, so a grammar
//! bump renamed an unchanged store; it is removed, not kept beside this.
//!
//! `verify` recomputes both from an arena and compares them to its head.
//! A head with the fields unset is reported as such, not treated as valid;
//! a head digest v2 head is refused with
//! [`crate::head_claim::HeadFormatError::RetiredDigestV2`].

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use leyline_core::{Construction, ContentAddressed, Domain, Entry, Hash, PartitionSpec, Ref};
use rusqlite::Connection;

pub use crate::tree::{GitMode, Mode};

/// Scheme tag of a store-tree directory node. Protocol-visible: changing
/// the entry shape, the ordering rule, the name rule or the params layout
/// is a `v2`, not an edit.
pub const STORE_TREE_SCHEME: &str = "leyline/store-tree/v1";

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

    /// `parserId` as a [`Ref<Parser>`]: the same fold, typed.
    pub fn parser_ref(&self) -> Ref<Parser> {
        Ref::derive(self)
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

/// One leaf of the store's manifest as the tree sees it (ADR-0045 T2): a
/// file entry, or an empty directory. A directory with an entry beneath it
/// is implied by its children and is not a leaf.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreLeaf {
    /// A regular, executable or symlink file; a symlink's blob is its
    /// target. A mode git does not store is unrepresentable (bead
    /// `ley-line-open-80f037` F8).
    File {
        path: String,
        mode: Mode,
        blob: Hash,
    },
    /// A directory with nothing beneath it. It has no content: its node is
    /// the fold of no entries.
    EmptyDir { path: String },
}

impl StoreLeaf {
    /// A regular (`0o100644`) file.
    pub fn regular(path: impl Into<String>, blob: Hash) -> Self {
        StoreLeaf::File {
            path: path.into(),
            mode: Mode::Regular,
            blob,
        }
    }

    /// The path relative to the store root.
    pub fn path(&self) -> &str {
        match self {
            StoreLeaf::File { path, .. } | StoreLeaf::EmptyDir { path } => path,
        }
    }
}

/// Why a leaf set is not a store (ADR-0045 T1). A store root over a set
/// that is not a tree is not a store root, so the build fails closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreTreeError {
    /// A path that is not a plain relative path: empty, or with an empty,
    /// `.` or `..` component (a leading, trailing or doubled `/`).
    MalformedPath { path: String },
    /// Two leaves at one path, or a leaf under a file.
    Conflict { path: String },
}

impl std::fmt::Display for StoreTreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreTreeError::MalformedPath { path } => write!(
                f,
                "store path {path:?} is not a plain relative path (empty, or an empty, `.` or `..` component)"
            ),
            StoreTreeError::Conflict { path } => write!(
                f,
                "store path {path:?} is held twice, or lies under a file: the leaves are not a tree"
            ),
        }
    }
}

impl std::error::Error for StoreTreeError {}

/// A directory of the store: its children by name. `BTreeMap<String, _>`
/// iterates in name-byte order, the fold's order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Dir {
    children: BTreeMap<String, Node>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Node {
    File { mode: Mode, blob: Hash },
    Dir(Dir),
}

/// The store as a directory tree, built from a leaf set that is known to be
/// a tree. Parse, don't validate: every [`StoreTree`] has a root, so
/// [`StoreTree::root`] is total.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StoreTree {
    root: Dir,
}

/// One directory node of a [`StoreTree`]: its path (`""` for the root), its
/// address, and how many entries its fold reads (two per child).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreNode {
    pub path: String,
    pub address: Hash,
    pub entries: usize,
}

fn store_tree_spec() -> PartitionSpec {
    PartitionSpec {
        domain: Domain::RowSet,
        scheme: STORE_TREE_SCHEME.to_string(),
        params: Vec::new(),
        canon_version: 1,
    }
}

/// The components of a plain relative path, or `MalformedPath`.
fn components(path: &str) -> std::result::Result<Vec<&str>, StoreTreeError> {
    let parts: Vec<&str> = path.split('/').collect();
    if path.is_empty()
        || parts
            .iter()
            .any(|c| c.is_empty() || *c == "." || *c == "..")
    {
        return Err(StoreTreeError::MalformedPath {
            path: path.to_string(),
        });
    }
    Ok(parts)
}

impl StoreTree {
    /// Build the tree a leaf set describes. Any enumeration order gives one
    /// tree. Fails closed on a malformed path, on two leaves at one path, and
    /// on a leaf under a file. An [`StoreLeaf::EmptyDir`] whose path another
    /// leaf lies under names the same directory, so it adds nothing.
    pub fn from_leaves(leaves: &[StoreLeaf]) -> std::result::Result<Self, StoreTreeError> {
        let mut tree = StoreTree::default();
        for leaf in leaves {
            let path = leaf.path();
            let conflict = || StoreTreeError::Conflict {
                path: path.to_string(),
            };
            let parts = components(path)?;
            let (last, dirs) = parts.split_last().expect("components is never empty");
            let mut dir = &mut tree.root;
            for name in dirs {
                let node = dir
                    .children
                    .entry((*name).to_string())
                    .or_insert_with(|| Node::Dir(Dir::default()));
                dir = match node {
                    Node::Dir(d) => d,
                    Node::File { .. } => return Err(conflict()),
                };
            }
            match leaf {
                StoreLeaf::File { mode, blob, .. } => {
                    if dir.children.contains_key(*last) {
                        return Err(conflict());
                    }
                    dir.children.insert(
                        (*last).to_string(),
                        Node::File {
                            mode: *mode,
                            blob: *blob,
                        },
                    );
                }
                StoreLeaf::EmptyDir { .. } => {
                    match dir
                        .children
                        .entry((*last).to_string())
                        .or_insert_with(|| Node::Dir(Dir::default()))
                    {
                        Node::Dir(_) => {}
                        Node::File { .. } => return Err(conflict()),
                    }
                }
            }
        }
        Ok(tree)
    }

    /// `storeRoot`: the root directory's node address.
    pub fn root(&self) -> Hash {
        self.fold(|_| {})
    }

    /// Every directory node, root included, with its address and entry
    /// count: the units an edit refolds. A Contains proof (ADR-0047 §2.1)
    /// walks these nodes but also needs each node's sibling entries; no
    /// proof type or verifier is built yet.
    pub fn nodes(&self) -> Vec<StoreNode> {
        let mut out = Vec::new();
        self.fold(|n| out.push(n));
        out
    }

    /// Fold every directory bottom-up with an explicit stack, so a deep
    /// path cannot exhaust the thread's stack; `visit` sees each node once,
    /// children before parents. Returns the root's address.
    fn fold(&self, mut visit: impl FnMut(StoreNode)) -> Hash {
        struct Frame<'a> {
            path: String,
            children: std::collections::btree_map::Iter<'a, String, Node>,
            entries: Vec<Entry>,
            /// The child directory this frame is waiting on.
            pending: Option<&'a str>,
        }
        fn push_child(entries: &mut Vec<Entry>, name: &str, child: Hash, mode: u32) {
            let i = (entries.len() / 2) as u64;
            entries.push(Entry {
                addr: name.as_bytes().hash(),
                a: 2 * i,
                b: name.len() as u64,
            });
            entries.push(Entry {
                addr: child,
                a: 2 * i + 1,
                b: u64::from(mode),
            });
        }
        let spec = store_tree_spec();
        let mut stack = vec![Frame {
            path: String::new(),
            children: self.root.children.iter(),
            entries: Vec::with_capacity(2 * self.root.children.len()),
            pending: None,
        }];
        loop {
            let top = stack
                .last_mut()
                .expect("the stack holds the root until it returns");
            match top.children.next() {
                Some((name, Node::File { mode, blob })) => {
                    push_child(&mut top.entries, name, *blob, mode.git());
                }
                Some((name, Node::Dir(dir))) => {
                    top.pending = Some(name);
                    let path = if top.path.is_empty() {
                        name.clone()
                    } else {
                        format!("{}/{name}", top.path)
                    };
                    stack.push(Frame {
                        path,
                        children: dir.children.iter(),
                        entries: Vec::with_capacity(2 * dir.children.len()),
                        pending: None,
                    });
                }
                None => {
                    let done = stack.pop().expect("a frame was on top");
                    let address = spec.address(&done.entries);
                    visit(StoreNode {
                        path: done.path,
                        address,
                        entries: done.entries.len(),
                    });
                    let Some(parent) = stack.last_mut() else {
                        return address;
                    };
                    let name = parent
                        .pending
                        .take()
                        .expect("the parent waits on this child");
                    push_child(
                        &mut parent.entries,
                        name,
                        address,
                        crate::tree::DIRECTORY_MODE,
                    );
                }
            }
        }
    }
}

/// `storeRoot` over an explicit leaf set. Pure, so a receiver holding only a
/// manifest (ADR-0029) recomputes it without an arena.
pub fn store_root_of(leaves: &[StoreLeaf]) -> std::result::Result<Hash, StoreTreeError> {
    Ok(StoreTree::from_leaves(leaves)?.root())
}

/// The tree's leaves of a manifest: every file entry, and every directory
/// with no entry beneath it.
pub fn leaves_of(entries: &BTreeMap<String, crate::tree::Entry>) -> Vec<StoreLeaf> {
    use crate::tree::Entry as E;
    let mut leaves = Vec::with_capacity(entries.len());
    for (path, entry) in entries {
        match entry {
            E::File { mode, blob } => leaves.push(StoreLeaf::File {
                path: path.clone(),
                mode: *mode,
                blob: *blob,
            }),
            E::Dir => {
                let prefix = format!("{path}/");
                let empty = !entries
                    .range(prefix.clone()..)
                    .next()
                    .is_some_and(|(k, _)| k.starts_with(&prefix));
                if empty {
                    leaves.push(StoreLeaf::EmptyDir { path: path.clone() });
                }
            }
        }
    }
    leaves
}

/// Read every leaf of the arena's manifest (`_tree`). Fails closed on an
/// entry git does not store, a file without a 32-byte blob hash, and a
/// manifest whose directory rows are not closed under parents (ADR-0045
/// T1): a store root over an unknown file is not a store root, and the leaf
/// set implies every non-empty directory, so a missing parent row would
/// otherwise fold to the same root as the closed manifest (bead
/// `ley-line-open-835aad`).
pub fn store_leaves(conn: &Connection) -> Result<Vec<StoreLeaf>> {
    let entries = crate::tree::entries(conn).context("read _tree for storeRoot")?;
    closed_under_parents(&entries)?;
    Ok(leaves_of(&entries))
}

/// Every entry's parent directories are directory rows of the manifest.
fn closed_under_parents(entries: &BTreeMap<String, crate::tree::Entry>) -> Result<()> {
    for path in entries.keys() {
        for parent in crate::tree::parents(path) {
            if !matches!(entries.get(parent), Some(crate::tree::Entry::Dir)) {
                bail!(
                    "_tree entry {path:?} has no directory row for its parent {parent:?}: the \
                     manifest is not closed under parents (ADR-0045 T1)"
                );
            }
        }
    }
    Ok(())
}

/// The arena's manifest as a [`StoreTree`].
pub fn store_tree(conn: &Connection) -> Result<StoreTree> {
    StoreTree::from_leaves(&store_leaves(conn)?).context("the arena's _tree is not a tree")
}

/// `storeRoot` of an arena.
pub fn store_root(conn: &Connection) -> Result<Hash> {
    Ok(store_tree(conn)?.root())
}

/// The `parserId` construction (`leyline/parser-id/v1`): its witness is the
/// [`ParserInputs`] (ADR-0047 §2.1).
pub enum Parser {}

impl Construction for Parser {
    const NAME: &'static str = "parser-id";
    type Witness<'w> = &'w ParserInputs;
    fn fold(inputs: &ParserInputs) -> Hash {
        inputs.parser_id()
    }
}

/// The `storeRoot` construction (`leyline/store-tree/v1`, ADR-0047 D-ii).
/// Its witness is the store alone: no parser, so a `Ref<Parser>` cannot
/// enter it and a parser upgrade cannot move it (law C7).
pub enum StoreRoot {}

impl Construction for StoreRoot {
    const NAME: &'static str = "store-root";
    type Witness<'w> = &'w StoreTree;
    fn fold(tree: &StoreTree) -> Hash {
        tree.root()
    }
}

/// [`store_root`], typed.
pub fn store_root_ref(conn: &Connection) -> Result<Ref<StoreRoot>> {
    Ok(Ref::derive(&store_tree(conn)?))
}

/// The `_meta` key [`DerivedFrom`] is stored under.
pub const DERIVED_FROM_KEY: &str = "derived_from";

/// The version discriminator of the `_meta.derived_from` value. v1 (three
/// fields, `treeRoot` first) is retired with `leyline/tree-root/v1`.
pub const DERIVED_FROM_VERSION: &str = "v2";

/// ADR-0040 D6, amended by ADR-0047 D-ii: what an arena's projection was
/// derived from, the triple `(storeRoot, parserId,
/// projection_schema_version)`. Every parse stamps it into `_meta` beside
/// the head; a consumer that trusts a transported arena compares it to the
/// `storeRoot` it holds, and `leyline rebuild` reproduces the projection
/// from it.
///
/// Stored as one `_meta` value, `v2 <storeRoot hex> <parserId hex>
/// <projection_schema_version>`, so the three fields are never read from
/// different parses and a v1 value is refused, never read as v2.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedFrom {
    pub store_root: Hash,
    pub parser_id: Hash,
    pub projection_schema_version: String,
}

impl DerivedFrom {
    pub fn to_meta(&self) -> String {
        format!(
            "{DERIVED_FROM_VERSION} {} {} {}",
            self.store_root, self.parser_id, self.projection_schema_version
        )
    }

    pub fn from_meta(value: &str) -> Result<Self> {
        let fields: Vec<&str> = value.split(' ').collect();
        let [version, store_root, parser_id, projection_schema_version] = fields[..] else {
            if fields.len() == 3 {
                bail!(
                    "_meta.{DERIVED_FROM_KEY} {value:?} is a retired v1 stamp (treeRoot, parserId, \
                     projection): treeRoot was removed by ADR-0047 M1; delete the .db and its \
                     .head.capnp and reparse cold"
                );
            }
            bail!("_meta.{DERIVED_FROM_KEY} {value:?} is not four space-separated fields");
        };
        if version != DERIVED_FROM_VERSION {
            bail!(
                "_meta.{DERIVED_FROM_KEY} {value:?} is version {version:?}; this binary reads {DERIVED_FROM_VERSION}"
            );
        }
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
            store_root: hash("storeRoot", store_root)?,
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
    /// Both stamps match a recomputation from the arena. `signature` says
    /// what the signature check established, so a valid verdict never hides
    /// whether the stamp was attested.
    Valid {
        store_root: Hash,
        parser_id: Hash,
        signature: crate::head_claim::SignatureCheck,
    },
    /// The head carries neither `storeRoot` nor `parserId`.
    NotStamped,
    /// The arena's `_tree` does not fold to the stamped `storeRoot`: the
    /// store changed after the head was written, or the head is not this
    /// arena's. Checked first: `storeRoot` needs no parser, so it is
    /// comparable whatever binary reads it.
    StoreRootMismatch { stamped: Hash, recomputed: Hash },
    /// The store matches, and the stamped `parserId` is not this binary's
    /// against this arena's query overrides: same store, different
    /// derivation.
    ParserIdMismatch { stamped: Hash, recomputed: Hash },
}

/// Recompute `storeRoot` and `parserId` from `db_path` and compare them to
/// the sibling `head.capnp`, under the head trust policy the environment
/// configures. Read-only.
pub fn verify(db_path: &Path) -> Result<Verification> {
    verify_with(db_path, &crate::head_claim::HeadPolicy::from_env()?)
}

/// [`verify`] under an explicit head trust policy.
pub fn verify_with(db_path: &Path, policy: &crate::head_claim::HeadPolicy) -> Result<Verification> {
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
    // The stamped storeRoot is what the signature attests (bead b68fa6), so
    // a configured trust set is checked before the stamp is compared with
    // the arena. Without this, an arena and a head rewritten together agree
    // with each other and pass. With a trust set configured an unsigned head
    // is refused here, so stripping the signature is not a way around it. A
    // head digest v2 head is refused here too, typed, before any digest.
    let signature = crate::head_claim::enforce_signature(
        h,
        &head_path,
        crate::head_claim::Reader::VerifyHead,
        policy,
    )?;
    // A half-stamped head is malformed, not unstamped (bead 80f037 F3).
    let Some((stamped_store, stamped_parser)) = crate::head_claim::stamp_of(h)
        .with_context(|| format!("head {} is malformed", head_path.display()))?
    else {
        return Ok(Verification::NotStamped);
    };

    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {} read-only", db_path.display()))?;
    // The stamps become typed only by re-derivation from their witnesses
    // (ADR-0047 §2.3): the arena's store, then this binary's parser inputs.
    let tree = store_tree(&conn)?;
    if Ref::<StoreRoot>::verified(stamped_store, &tree).is_none() {
        return Ok(Verification::StoreRootMismatch {
            stamped: stamped_store,
            recomputed: tree.root(),
        });
    }
    let trusted = crate::cmd_parse::trusted_query_hashes_from_env();
    let resolution = leyline_ts::query_engine::resolve_query_set(&conn, &trusted)?;
    let inputs = ParserInputs::current(&resolution.query_set);
    if Ref::<Parser>::verified(stamped_parser, &inputs).is_none() {
        return Ok(Verification::ParserIdMismatch {
            stamped: stamped_parser,
            recomputed: *inputs.parser_ref().as_hash(),
        });
    }
    Ok(Verification::Valid {
        store_root: stamped_store,
        parser_id: stamped_parser,
        signature,
    })
}

/// A `Common.Hash` field: absent or all-zero means "not stamped"; any other
/// length than 32 is a malformed head and an error, never a silent default.
pub(crate) fn hash_field(field: Option<capnp::Result<&[u8]>>) -> Result<Option<Hash>> {
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
            store_root,
            parser_id,
            signature,
        } => {
            println!("valid storeRoot={store_root} parserId={parser_id} signature={signature}");
            Ok(())
        }
        Verification::NotStamped => bail!(
            "head beside {} carries no storeRoot/parserId; reparse to stamp it",
            db.display()
        ),
        Verification::StoreRootMismatch {
            stamped,
            recomputed,
        } => bail!(
            "storeRoot mismatch: head stamped {stamped}, the arena's _tree folds to {recomputed}; the store changed after the head was written or the head is not this arena's"
        ),
        Verification::ParserIdMismatch {
            stamped,
            recomputed,
        } => bail!(
            "parserId mismatch: head stamped {stamped}, this binary against this arena's overrides computes {recomputed}; same store, different derivation"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(path: &str, byte: u8) -> StoreLeaf {
        StoreLeaf::regular(path, Hash::from_bytes([byte; 32]))
    }

    fn root(leaves: &[StoreLeaf]) -> Hash {
        store_root_of(leaves).expect("the leaves are a tree")
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

    fn node(entries: &[Entry]) -> Hash {
        PartitionSpec {
            domain: Domain::RowSet,
            scheme: "leyline/store-tree/v1".to_string(),
            params: Vec::new(),
            canon_version: 1,
        }
        .address(entries)
    }

    fn e(addr: Hash, a: u64, b: u64) -> Entry {
        Entry { addr, a, b }
    }

    /// `storeRoot` is exactly the declared directory fold: an independent
    /// reconstruction with literal framing (`2i` for the name entry, `2i+1`
    /// for the child entry with its git mode, `i` the rank in name order,
    /// empty params) reproduces it, node by node.
    #[test]
    fn store_root_is_the_declared_directory_fold() {
        let leaves = [leaf("b.go", 2), leaf("a.go", 1), leaf("c/d.go", 3)];
        let c = node(&[
            e(b"d.go".hash(), 0, 4),
            e(Hash::from_bytes([3; 32]), 1, 0o100644),
        ]);
        let expected = node(&[
            e(b"a.go".hash(), 0, 4),
            e(Hash::from_bytes([1; 32]), 1, 0o100644),
            e(b"b.go".hash(), 2, 4),
            e(Hash::from_bytes([2; 32]), 3, 0o100644),
            e(b"c".hash(), 4, 1),
            e(c, 5, 0o040000),
        ]);
        assert_eq!(root(&leaves), expected);
        let tree = StoreTree::from_leaves(&leaves).unwrap();
        assert_eq!(
            tree.nodes(),
            vec![
                StoreNode {
                    path: "c".into(),
                    address: c,
                    entries: 2
                },
                StoreNode {
                    path: String::new(),
                    address: expected,
                    entries: 6
                },
            ]
        );
    }

    /// Law C7 (ADR-0047 D-ii): the store's identity is a function of the
    /// store alone. The construction takes no parser, and the same leaves
    /// under two different `parserId`s name one store.
    #[test]
    fn store_root_is_unchanged_when_only_parser_id_changes() {
        let leaves = [leaf("a.go", 1), leaf("b/c.go", 2)];
        let mut other = inputs();
        other.extraction_epoch = "6".into();
        assert_ne!(inputs().parser_id(), other.parser_id());
        let tree = StoreTree::from_leaves(&leaves).unwrap();
        let r: Ref<StoreRoot> = Ref::derive(&tree);
        assert_eq!(*r.as_hash(), root(&leaves));
        // The pair is what differs: the derivation stamps differ, the store
        // stamp does not.
        let a = DerivedFrom {
            store_root: root(&leaves),
            parser_id: inputs().parser_id(),
            projection_schema_version: "projection-v11".into(),
        };
        let b = DerivedFrom {
            parser_id: other.parser_id(),
            ..a.clone()
        };
        assert_eq!(a.store_root, b.store_root);
        assert_ne!(a, b);
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
    fn store_root_is_independent_of_leaf_enumeration_order() {
        let a = root(&[leaf("a.go", 1), leaf("b.go", 2), leaf("d/e.go", 3)]);
        let b = root(&[leaf("d/e.go", 3), leaf("b.go", 2), leaf("a.go", 1)]);
        assert_eq!(a, b);
    }

    #[test]
    fn store_root_binds_path_to_content() {
        let straight = root(&[leaf("a.go", 1), leaf("b.go", 2)]);
        let swapped = root(&[leaf("a.go", 2), leaf("b.go", 1)]);
        assert_ne!(
            straight, swapped,
            "same multiset of hashes, different mapping"
        );
    }

    #[test]
    fn store_root_moves_with_one_byte_or_one_path() {
        let base = root(&[leaf("a.go", 1), leaf("b.go", 2)]);
        assert_ne!(base, root(&[leaf("a.go", 1), leaf("b.go", 3)]));
        assert_ne!(base, root(&[leaf("a.go", 1), leaf("c.go", 2)]));
        assert_ne!(base, root(&[leaf("a.go", 1), leaf("x/b.go", 2)]));
    }

    /// Names are length-framed and directories are nodes, so `ab`+`c`,
    /// `a`+`bc` and `a/b` never collide through concatenation.
    #[test]
    fn store_root_distinguishes_name_splits_and_nesting() {
        let two = root(&[leaf("ab", 1), leaf("c", 1)]);
        assert_ne!(two, root(&[leaf("a", 1), leaf("bc", 1)]));
        assert_ne!(root(&[leaf("a/b", 1)]), root(&[leaf("ab", 1)]));
        assert_ne!(root(&[leaf("a/b", 1)]), root(&[leaf("a", 1), leaf("b", 1)]));
    }

    /// A subtree's node address is the `storeRoot` of a store holding
    /// exactly that subtree, so a sub-manifest (ADR-0040 D2) is named by its
    /// own root.
    #[test]
    fn a_subtree_is_named_by_its_own_store_root() {
        let whole = StoreTree::from_leaves(&[
            leaf("top.go", 9),
            leaf("pkg/a.go", 1),
            leaf("pkg/inner/b.go", 2),
            StoreLeaf::EmptyDir {
                path: "pkg/empty".into(),
            },
        ])
        .unwrap();
        let sub = root(&[
            leaf("a.go", 1),
            leaf("inner/b.go", 2),
            StoreLeaf::EmptyDir {
                path: "empty".into(),
            },
        ]);
        let pkg = whole.nodes().into_iter().find(|n| n.path == "pkg").unwrap();
        assert_eq!(pkg.address, sub);
    }

    /// One edit refolds only the nodes on its path: every other directory
    /// keeps its address.
    #[test]
    fn one_edit_moves_exactly_the_nodes_on_its_path() {
        let mut leaves = vec![
            leaf("x/a/1.go", 1),
            leaf("x/a/2.go", 2),
            leaf("x/b/3.go", 3),
            leaf("y/4.go", 4),
        ];
        let before = StoreTree::from_leaves(&leaves).unwrap().nodes();
        leaves[1] = leaf("x/a/2.go", 7);
        let after = StoreTree::from_leaves(&leaves).unwrap().nodes();
        let moved: Vec<&str> = before
            .iter()
            .zip(&after)
            .filter(|(b, a)| b.address != a.address)
            .map(|(b, _)| b.path.as_str())
            .collect();
        assert_eq!(moved, vec!["x/a", "x", ""]);
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
    fn parser_id_and_store_root_are_domain_separated() {
        // The two schemes never collide even over degenerate inputs.
        assert_ne!(inputs().parser_id(), root(&[]));
    }

    #[test]
    fn store_leaves_fail_closed_on_an_entry_without_a_hash() {
        let conn = Connection::open_in_memory().unwrap();
        crate::tree::ensure_tables(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO _tree (path, mode, blob_hash) VALUES ('a.go', 33188, NULL);",
        )
        .unwrap();
        let err = format!("{:#}", store_leaves(&conn).unwrap_err());
        assert!(err.contains("no blob_hash"), "{err}");
        conn.execute_batch("UPDATE _tree SET blob_hash = X'0102' WHERE path = 'a.go';")
            .unwrap();
        let err = format!("{:#}", store_leaves(&conn).unwrap_err());
        assert!(err.contains("expected 32"), "{err}");
    }

    /// Bead `ley-line-open-835aad`: a `_tree` missing a parent directory row
    /// folds to the same leaves as the closed manifest, so it is refused
    /// rather than given that manifest's root; a parent held by a file is
    /// refused the same way.
    #[test]
    fn a_manifest_not_closed_under_parents_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        crate::tree::ensure_tables(&conn).unwrap();
        crate::tree::put_file(
            &conn,
            "pkg/a.go",
            Mode::Regular,
            Hash::from_bytes([1; 32]),
            None,
        )
        .unwrap();
        let closed = store_root(&conn).unwrap();
        assert_eq!(closed, root(&[leaf("pkg/a.go", 1)]));
        conn.execute("DELETE FROM _tree WHERE path = 'pkg'", [])
            .unwrap();
        let err = format!("{:#}", store_root(&conn).unwrap_err());
        assert!(
            err.contains("not closed under parents") && err.contains("\"pkg\""),
            "{err}"
        );
        conn.execute(
            "INSERT INTO _tree (path, mode, blob_hash) VALUES ('pkg', 33188, ?1)",
            [vec![2u8; 32]],
        )
        .unwrap();
        let err = format!("{:#}", store_root(&conn).unwrap_err());
        assert!(err.contains("not closed under parents"), "{err}");
    }

    /// A leaf set that is not a tree has no store root.
    #[test]
    fn a_leaf_set_that_is_not_a_tree_is_refused() {
        for bad in ["", "/a", "a/", "a//b", "a/./b", "../a", "a/.."] {
            assert_eq!(
                store_root_of(&[leaf(bad, 1)]),
                Err(StoreTreeError::MalformedPath { path: bad.into() }),
                "{bad:?}"
            );
        }
        let conflict = |path: &str| {
            Err(StoreTreeError::Conflict {
                path: path.to_string(),
            })
        };
        assert_eq!(
            store_root_of(&[leaf("a", 1), leaf("a/b", 1)]),
            conflict("a/b")
        );
        assert_eq!(store_root_of(&[leaf("a", 1), leaf("a", 2)]), conflict("a"));
        assert_eq!(
            store_root_of(&[leaf("a/b", 1), leaf("a", 1)]),
            conflict("a")
        );
        assert_eq!(
            store_root_of(&[leaf("a", 1), StoreLeaf::EmptyDir { path: "a".into() }]),
            conflict("a")
        );
        // An empty directory another leaf lies under names that directory.
        assert_eq!(
            root(&[StoreLeaf::EmptyDir { path: "a".into() }, leaf("a/b", 1)]),
            root(&[leaf("a/b", 1)])
        );
        let msg = StoreTreeError::Conflict { path: "a".into() }.to_string();
        assert!(msg.contains("\"a\"") && msg.contains("not a tree"), "{msg}");
        let msg = StoreTreeError::MalformedPath { path: "a//".into() }.to_string();
        assert!(msg.contains("plain relative path"), "{msg}");
    }

    /// ADR-0045 T2: a file's child entry carries its git mode in `b`; an
    /// empty directory is a node with no entries, carried by its parent
    /// under the directory mode.
    #[test]
    fn modes_and_empty_dirs_move_the_store_root() {
        let leaves = [
            StoreLeaf::File {
                path: "run.sh".into(),
                mode: Mode::Executable,
                blob: Hash::from_bytes([1; 32]),
            },
            StoreLeaf::File {
                path: "link".into(),
                mode: Mode::Symlink,
                blob: b"run.sh".hash(),
            },
            StoreLeaf::EmptyDir {
                path: "empty".into(),
            },
        ];
        let expected = node(&[
            e(b"empty".hash(), 0, 5),
            e(node(&[]), 1, 0o040000),
            e(b"link".hash(), 2, 4),
            e(b"run.sh".hash(), 3, 0o120000),
            e(b"run.sh".hash(), 4, 6),
            e(Hash::from_bytes([1; 32]), 5, 0o100755),
        ]);
        assert_eq!(root(&leaves), expected);
        // The same bytes as a regular file fold differently.
        let regular = StoreLeaf::regular("run.sh", Hash::from_bytes([1; 32]));
        assert_ne!(root(&[regular]), root(&leaves[..1]));
        // An empty directory moves the root, and is not an empty file.
        assert_ne!(root(&leaves), root(&leaves[..2]));
    }

    /// The leaves of a manifest are its files and its empty directories; a
    /// directory with an entry beneath it is implied.
    #[test]
    fn a_manifest_folds_its_files_and_empty_directories() {
        use crate::tree::Entry as E;
        let h = |b: u8| Hash::from_bytes([b; 32]);
        let manifest = BTreeMap::from([
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
        let leaves = leaves_of(&manifest);
        assert_eq!(
            leaves,
            vec![
                StoreLeaf::EmptyDir { path: "a-c".into() },
                StoreLeaf::regular("a/b.go", h(1)),
                StoreLeaf::regular("c.go", h(2)),
            ]
        );
        let mut without_empty = manifest.clone();
        without_empty.remove("a-c");
        assert_eq!(
            root(&leaves_of(&without_empty)),
            root(&[
                StoreLeaf::regular("a/b.go", h(1)),
                StoreLeaf::regular("c.go", h(2))
            ]),
            "directories with files beneath them are not leaves"
        );
    }

    /// `derived_from` is versioned: a v2 value round-trips, and the retired
    /// v1 shape (three fields, treeRoot first) is refused by name.
    #[test]
    fn derived_from_is_versioned_and_v1_is_refused() {
        let d = DerivedFrom {
            store_root: Hash::from_bytes([1; 32]),
            parser_id: Hash::from_bytes([2; 32]),
            projection_schema_version: "projection-v11".into(),
        };
        let meta = d.to_meta();
        assert!(meta.starts_with("v2 "), "{meta}");
        assert_eq!(DerivedFrom::from_meta(&meta).unwrap(), d);
        let v1 = format!("{} {} projection-v10", d.store_root, d.parser_id);
        let err = format!("{:#}", DerivedFrom::from_meta(&v1).unwrap_err());
        assert!(err.contains("retired v1"), "{err}");
        let v9 = meta.replacen("v2", "v9", 1);
        let err = format!("{:#}", DerivedFrom::from_meta(&v9).unwrap_err());
        assert!(err.contains("\"v9\""), "{err}");
        let err = format!("{:#}", DerivedFrom::from_meta("v2 x").unwrap_err());
        assert!(err.contains("four"), "{err}");
        let bad = format!("v2 {} zz projection-v11", d.store_root);
        let err = format!("{:#}", DerivedFrom::from_meta(&bad).unwrap_err());
        assert!(err.contains("parserId"), "{err}");
    }

    /// The fold laws of the module header as for-all properties (bead
    /// `ley-line-open-80f037` F10). Paths are drawn from an alphabet holding
    /// `/`, the unit separator 0x1f and newline, kept to the plain relative
    /// paths a manifest holds, and short enough that shared prefixes and
    /// one-byte neighbours are common.
    mod laws {
        use super::*;
        use proptest::prelude::*;

        const MODES: [Option<Mode>; 4] = [
            Some(Mode::Regular),
            Some(Mode::Executable),
            Some(Mode::Symlink),
            None,
        ];

        fn path() -> impl Strategy<Value = String> {
            "[ab\u{1f}\n]{1,3}(/[ab\u{1f}\n]{1,3}){0,2}"
        }

        fn make(path: String, mode: usize, blob: u8) -> StoreLeaf {
            match MODES[mode] {
                Some(mode) => StoreLeaf::File {
                    path,
                    mode,
                    blob: Hash::from_bytes([blob; 32]),
                },
                None => StoreLeaf::EmptyDir { path },
            }
        }

        /// A leaf set that is a tree: distinct paths, none under another.
        fn leaves() -> impl Strategy<Value = Vec<StoreLeaf>> {
            proptest::collection::btree_map(path(), (0usize..4, 0u8..3), 0..6).prop_map(|m| {
                let paths: Vec<String> = m.keys().cloned().collect();
                m.into_iter()
                    .filter(|(p, _)| {
                        !paths.iter().any(|q| {
                            q != p
                                && (p.starts_with(&format!("{q}/"))
                                    || q.starts_with(&format!("{p}/")))
                        })
                    })
                    .map(|(path, (mode, blob))| make(path, mode, blob))
                    .collect()
            })
        }

        fn non_empty_leaves() -> impl Strategy<Value = Vec<StoreLeaf>> {
            leaves().prop_filter("one leaf to change", |l| !l.is_empty())
        }

        fn root_of(l: &[StoreLeaf]) -> Hash {
            store_root_of(l).expect("the strategy draws trees")
        }

        proptest! {
            /// Discovery order is not identity: every permutation of a leaf
            /// set folds to one root.
            #[test]
            fn store_root_is_invariant_under_permutation(
                l in leaves().prop_flat_map(|l| (Just(l.clone()), Just(l).prop_shuffle())),
            ) {
                prop_assert_eq!(root_of(&l.0), root_of(&l.1));
            }

            /// One changed path byte moves the root.
            #[test]
            fn one_path_byte_moves_the_store_root(
                l in non_empty_leaves(),
                which in any::<prop::sample::Index>(),
                at in any::<prop::sample::Index>(),
                byte in prop::sample::select(vec![b'a', b'b', 0x1f, b'\n']),
            ) {
                let i = which.index(l.len());
                let mut bytes = l[i].path().as_bytes().to_vec();
                let j = at.index(bytes.len());
                prop_assume!(bytes[j] != b'/' && bytes[j] != byte);
                bytes[j] = byte;
                let moved = String::from_utf8(bytes).unwrap();
                let mut changed = l.clone();
                match &mut changed[i] {
                    StoreLeaf::File { path, .. } | StoreLeaf::EmptyDir { path } => *path = moved,
                }
                let Ok(after) = store_root_of(&changed) else {
                    // The rename collided with another leaf: not a tree.
                    return Ok(());
                };
                prop_assert_ne!(root_of(&l), after);
            }

            /// One changed blob byte moves the root.
            #[test]
            fn one_content_byte_moves_the_store_root(
                l in non_empty_leaves(),
                which in any::<prop::sample::Index>(),
                at in 0usize..32,
                flip in 1u8..=255,
            ) {
                let i = which.index(l.len());
                let mut changed = l.clone();
                let StoreLeaf::File { blob, .. } = &mut changed[i] else {
                    return Ok(());
                };
                let mut bytes = *blob.as_bytes();
                bytes[at] ^= flip;
                *blob = Hash::from_bytes(bytes);
                prop_assert_ne!(root_of(&l), root_of(&changed));
            }

            /// One changed mode moves the root.
            #[test]
            fn one_mode_moves_the_store_root(
                l in non_empty_leaves(),
                which in any::<prop::sample::Index>(),
                to in 0usize..4,
            ) {
                let i = which.index(l.len());
                let path = l[i].path().to_string();
                let from = match &l[i] {
                    StoreLeaf::File { mode, .. } => Some(*mode),
                    StoreLeaf::EmptyDir { .. } => None,
                };
                prop_assume!(from != MODES[to]);
                let mut changed = l.clone();
                changed[i] = make(path, to, 0);
                if from.is_some() && MODES[to].is_some() {
                    // Keep the blob: only the mode moves.
                    if let (StoreLeaf::File { blob: old, .. }, StoreLeaf::File { blob, .. }) = (&l[i], &mut changed[i]) {
                        *blob = *old;
                    }
                }
                prop_assert_ne!(root_of(&l), root_of(&changed));
            }
        }
    }
}
