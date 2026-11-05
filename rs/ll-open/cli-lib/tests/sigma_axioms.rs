//! Σ axiom falsification suite — the `leyline-cli-lib` slice (ADR-0047 §5,
//! bead `ley-line-open-10d71a`).
//!
//! The constructions and stores this crate owns: `source_blobs` (ρ over
//! file bytes), `parserId`, `treeRoot`, `node_hash` and the parse that
//! produces them. Tests named as ADR-0047 §5 names them; a name that also
//! appears in `leyline-core` or `leyline-fs` is the same test over the
//! stores those crates own.
//!
//! **Known RED.** Two claims fail on shipped code. Each has a strict test,
//! `#[ignore]`d with the bead whose PR turns it green, and a `*_measured`
//! test that runs in every `task ci`, prints the measurement, and pins
//! today's failing value, so the red is visible and the fix cannot land
//! without flipping both:
//!
//! - `merkle_one_leaf_edit_rehashes_one_path` — `treeRoot` is one flat
//!   fold, so one edit refolds 2n entries (ADR-0047 M1, `ley-line-open-73c6ee`);
//! - `crafted_preimage_does_not_alias_across_constructions` — `node_hash`
//!   is plain σ over a prefixed preimage, so a file whose bytes are a leaf's
//!   preimage has `blob_hash == node_hash` (ADR-0047 M2, `ley-line-open-73add7`).
//!
//! `run_id`'s half of law (c) is in `leyline-runtime`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use leyline_cli_lib::cmd_parse;
use leyline_cli_lib::tree_root::{
    DerivedFrom, GitMode, ParserInputs, TREE_ROOT_SCHEME, TreeLeaf, tree_root_of,
};
use leyline_core::{ContentAddressed, Domain, Entry, Hash, PartitionSpec};
use proptest::prelude::*;
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn vectors() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/sigma/v1/test-vectors/constructions.json");
    serde_json::from_slice(&fs::read(&path).unwrap()).expect("Σ vectors are JSON")
}

fn hash_of(v: &Value) -> Hash {
    let bytes: [u8; 32] = hex::decode(v.as_str().expect("hex string"))
        .unwrap()
        .try_into()
        .expect("32 bytes");
    Hash::from_bytes(bytes)
}

fn cases<'v>(v: &'v Value, key: &str) -> &'v [Value] {
    let cases = v[key].as_array().unwrap();
    assert!(!cases.is_empty(), "{key:?} has no cases: it gates nothing");
    cases
}

fn write_tree(root: &Path, files: &[(&str, Vec<u8>)]) {
    for (rel, bytes) in files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }
}

/// A cold parse of `src` into a fresh file-backed arena (the `:memory:` path
/// writes no segments, so no `Head.rootHash`).
fn parse_cold(src: &Path) -> (TempDir, PathBuf) {
    let out = TempDir::new().unwrap();
    let db = out.path().join("arena.db");
    let conn = Connection::open(&db).unwrap();
    cmd_parse::parse_into_conn(&conn, src, None, None).unwrap();
    (out, db)
}

fn node_hashes(conn: &Connection) -> BTreeSet<Vec<u8>> {
    let mut stmt = conn.prepare("SELECT node_hash FROM node_content").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn blob_hashes(conn: &Connection) -> BTreeSet<Vec<u8>> {
    let mut stmt = conn.prepare("SELECT blob_hash FROM source_blobs").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// `Head.rootHash` from the arena's sibling `head.capnp`.
fn root_hash(db: &Path) -> [u8; 32] {
    use leyline_schema_capnp::head_capnp::head;
    let bytes =
        fs::read(db.with_extension("head.capnp")).expect("a file-backed parse writes a head");
    let mut slice: &[u8] = &bytes;
    let msg =
        capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new()).unwrap();
    let h: head::Reader = msg.get_root().unwrap();
    h.get_root_hash()
        .unwrap()
        .get_bytes()
        .unwrap()
        .try_into()
        .unwrap()
}

/// LEB128, as `node_hash`'s preimage frames its fields.
fn uvarint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// `enc(x)` of a `node_hash` leaf, as the `astNode` vectors pin it.
fn leaf_preimage(kind: &str, token: &str) -> Vec<u8> {
    let mut p = b"llo/ast/v1\0\0".to_vec();
    uvarint(&mut p, kind.len() as u64);
    p.extend_from_slice(kind.as_bytes());
    uvarint(&mut p, token.len() as u64);
    p.extend_from_slice(token.as_bytes());
    p
}

/// `enc(x)` of a `node_hash` internal node.
fn internal_preimage(kind: &str, children: &[Vec<u8>]) -> Vec<u8> {
    let mut p = b"llo/ast/v1\0\x01".to_vec();
    uvarint(&mut p, kind.len() as u64);
    p.extend_from_slice(kind.as_bytes());
    uvarint(&mut p, children.len() as u64);
    for c in children {
        p.extend_from_slice(c);
    }
    p
}

/// A Go and a Rust file with nested bodies, so trees have real depth.
fn fixture_files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "go/a.go",
            b"package a\n\nfunc Answer(x int) int {\n\tif x > 0 {\n\t\treturn x + 41\n\t}\n\treturn 0\n}\n"
                .to_vec(),
        ),
        (
            "go/sub/b.go",
            b"package sub\n\nfunc B(s string) string {\n\treturn s + \"b\"\n}\n".to_vec(),
        ),
        (
            "rs/lib.rs",
            b"pub fn add(a: u32, b: u32) -> u32 {\n    if a > b { a - b } else { a + b }\n}\n"
                .to_vec(),
        ),
        ("docs/readme.md", b"# Title\n\nSome *text*.\n".to_vec()),
    ]
}

// ---------------------------------------------------------------------------
// (CA)
// ---------------------------------------------------------------------------

/// (CA) ρ(σ(v)) = v for `source_blobs`, the store `leyline read_source`,
/// `storefs` reads and `leyline rebuild` resolve through
/// `tree::blob_bytes`: every file a parse stored reads back byte-identical
/// under its `_tree` blob hash, at the boundary sizes (empty, sub-floor,
/// past `MAX_CHUNK`) and over arbitrary bytes.
#[test]
fn ca_round_trip_every_store() {
    let src = TempDir::new().unwrap();
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("empty.txt", Vec::new()),
        ("one.txt", b"x".to_vec()),
        ("sub_floor.go", b"package p\n".repeat(800)),
        (
            "past_max_chunk.txt",
            (0..140_000u32).map(|i| (i * 7 % 251) as u8).collect(),
        ),
    ];
    write_tree(src.path(), &files);
    let (_out, db) = parse_cold(src.path());
    let conn = Connection::open(&db).unwrap();
    let entries = leyline_cli_lib::tree::entries(&conn).unwrap();
    for (rel, bytes) in &files {
        let Some(leyline_cli_lib::tree::Entry::File { blob, .. }) = entries.get(*rel) else {
            panic!("{rel} is in the store's manifest");
        };
        assert_eq!(
            *blob,
            bytes.as_slice().hash(),
            "{rel}: _tree names σ(bytes)"
        );
        assert_eq!(
            leyline_cli_lib::tree::blob_bytes(&conn, *blob)
                .unwrap()
                .as_deref(),
            Some(bytes.as_slice()),
            "(CA) {rel} reads back whole"
        );
    }

    // Arbitrary bytes through the same table and the same reader. The rows
    // are written with the parse's own statement shape (`INSERT OR IGNORE`
    // keyed by σ), so the property is over ρ, not over a parse.
    let mem = Connection::open_in_memory().unwrap();
    leyline_ts::schema::create_source_blobs_table(&mem).unwrap();
    proptest!(ProptestConfig::with_cases(64), |(bytes in proptest::collection::vec(any::<u8>(), 0..4096))| {
        let h = bytes.as_slice().hash();
        mem.execute(
            "INSERT OR IGNORE INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
            params![h.as_bytes().as_slice(), bytes],
        ).unwrap();
        prop_assert_eq!(leyline_cli_lib::tree::blob_bytes(&mem, h).unwrap(), Some(bytes));
    });
}

/// (CA) as verify-on-read, decade F3: a `source_blobs` row whose bytes were
/// changed under its key is refused by `tree::blob_bytes`, the reader behind
/// `read_source`, `storefs` and `rebuild`, rather than served as the file
/// the key names.
#[test]
fn ca_tampered_payload_is_refused() {
    let src = TempDir::new().unwrap();
    write_tree(src.path(), &fixture_files());
    let (_out, db) = parse_cold(src.path());
    let conn = Connection::open(&db).unwrap();
    let blobs = blob_hashes(&conn);
    assert!(!blobs.is_empty());
    for blob in blobs {
        let original: Vec<u8> = conn
            .query_row(
                "SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1",
                [&blob],
                |r| r.get(0),
            )
            .unwrap();
        let mut tampered = original.clone();
        match tampered.len() {
            0 => tampered.push(0),
            n => tampered[n / 2] ^= 0x01,
        }
        conn.execute(
            "UPDATE source_blobs SET blob_bytes = ?2 WHERE blob_hash = ?1",
            params![blob, tampered],
        )
        .unwrap();
        let key = Hash::from_bytes(blob.clone().try_into().unwrap());
        assert!(
            leyline_cli_lib::tree::blob_bytes(&conn, key).is_err(),
            "a tampered source blob must be refused, not served"
        );
        conn.execute(
            "UPDATE source_blobs SET blob_bytes = ?2 WHERE blob_hash = ?1",
            params![blob, original],
        )
        .unwrap();
        assert!(
            leyline_cli_lib::tree::blob_bytes(&conn, key)
                .unwrap()
                .is_some()
        );
    }
}

// ---------------------------------------------------------------------------
// (DET)
// ---------------------------------------------------------------------------

/// (DET) `parserId` and `treeRoot` reproduce their checked-in vectors on
/// fixed inputs, and each `treeRoot` vector is the documented `RowSet` fold
/// rebuilt here from its leaves (ADR-0045 T2): `(σ(path), 2i, |path|)` and
/// `(content, 2i + 1, mode_tag)` per leaf in path-byte order, `parserId` as
/// params. `node_hash`'s vectors are checked against its private fold in
/// `cmd_parse.rs`'s unit tests; the core constructions in `leyline-core`.
#[test]
fn det_known_answer_vectors() {
    let v = vectors();
    for case in cases(&v, "parserId") {
        let inputs = ParserInputs {
            ir_schema_version: case["irSchemaVersion"].as_str().unwrap().into(),
            projection_schema_version: case["projectionSchemaVersion"].as_str().unwrap().into(),
            extraction_epoch: case["extractionEpoch"].as_str().unwrap().into(),
            injection_epoch: case["injectionEpoch"].as_str().unwrap().into(),
            query_set_epoch: case["querySetEpoch"].as_str().unwrap().into(),
            grammars: case["grammars"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| (g["name"].as_str().unwrap().into(), hash_of(&g["digest"])))
                .collect(),
        };
        assert_eq!(
            inputs.parser_id(),
            hash_of(&case["address"]),
            "parserId drifted"
        );
    }
    for case in cases(&v, "treeRoot") {
        assert_eq!(case["scheme"].as_str(), Some(TREE_ROOT_SCHEME));
        let pid = hash_of(&case["parserId"]);
        let leaves: Vec<TreeLeaf> = case["leaves"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| TreeLeaf {
                path: l["path"].as_str().unwrap().into(),
                mode: GitMode::from_git(l["mode"].as_u64().unwrap() as u32).unwrap(),
                content: hash_of(&l["content"]),
            })
            .collect();
        let address = tree_root_of(&leaves, pid);
        assert_eq!(
            address,
            hash_of(&case["address"]),
            "treeRoot drifted for {}",
            case["name"]
        );
        assert_eq!(address, documented_tree_root(&leaves, pid));
    }
}

fn mode_tag(mode: GitMode) -> u64 {
    match mode {
        GitMode::Regular => 0,
        other => u64::from(other.git()),
    }
}

/// `tree-root/v1` rebuilt from its documented encoding, entry by entry.
fn documented_tree_root(leaves: &[TreeLeaf], pid: Hash) -> Hash {
    PartitionSpec {
        domain: Domain::RowSet,
        scheme: TREE_ROOT_SCHEME.to_string(),
        params: pid.as_bytes().to_vec(),
        canon_version: 1,
    }
    .address(&tree_root_entries(leaves))
}

fn tree_root_entries(leaves: &[TreeLeaf]) -> Vec<Entry> {
    let mut sorted: Vec<&TreeLeaf> = leaves.iter().collect();
    sorted.sort_by(|x, y| x.path.as_bytes().cmp(y.path.as_bytes()));
    let mut entries = Vec::with_capacity(2 * sorted.len());
    for (i, l) in sorted.iter().enumerate() {
        let i = i as u64;
        entries.push(Entry {
            addr: l.path.as_bytes().hash(),
            a: 2 * i,
            b: l.path.len() as u64,
        });
        entries.push(Entry {
            addr: l.content,
            a: 2 * i + 1,
            b: mode_tag(l.mode),
        });
    }
    entries
}

/// What a parse names, for comparing two parses of one tree.
#[derive(Debug, PartialEq, Eq)]
struct Identities {
    node_hashes: BTreeSet<Vec<u8>>,
    derived_from: DerivedFrom,
    root_hash: [u8; 32],
}

fn parse_with_threads(src: &Path, threads: usize) -> Identities {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();
    let (_out, db) = pool.install(|| parse_cold(src));
    let conn = Connection::open(&db).unwrap();
    Identities {
        node_hashes: node_hashes(&conn),
        derived_from: DerivedFrom::read(&conn)
            .unwrap()
            .expect("a parse stamps derived_from"),
        root_hash: root_hash(&db),
    }
}

/// (DET) across schedules: one parse on one rayon thread and one on eight
/// name the same `node_hash` set, the same `treeRoot`/`parserId` stamp, and
/// the same `Head.rootHash`. Both parse one source directory, so the
/// absolute paths and mtimes `rootHash`'s segments carry are equal and the
/// comparison needs no masking. (ADR-0047 §5 also names `storeRoot`; it
/// does not exist before M1, `ley-line-open-73c6ee`.)
#[test]
fn det_parse_is_thread_count_invariant() {
    let src = TempDir::new().unwrap();
    let mut files = fixture_files();
    for i in 0..24 {
        files.push((
            Box::leak(format!("gen/f{i:02}.go").into_boxed_str()),
            format!("package gen\n\nfunc F{i}(x int) int {{\n\treturn x * {i}\n}}\n").into_bytes(),
        ));
    }
    write_tree(src.path(), &files);
    let one = parse_with_threads(src.path(), 1);
    let many = parse_with_threads(src.path(), 8);
    assert!(
        one.node_hashes.len() > 50,
        "a corpus, not a vacuous comparison"
    );
    assert_eq!(one, many);
}

// ---------------------------------------------------------------------------
// (IM)
// ---------------------------------------------------------------------------

type NodeRow = (i64, String, String, String, Option<String>, i64);

fn node_rows(conn: &Connection) -> BTreeMap<Vec<u8>, NodeRow> {
    let mut stmt = conn
        .prepare("SELECT node_hash, node_tag, kind, raw_kind, lang, token, arity FROM node_content")
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            (
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ),
        ))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn blob_rows(conn: &Connection) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut stmt = conn
        .prepare("SELECT blob_hash, blob_bytes FROM source_blobs")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// Children of every internal `node_content` row, by hash, in ordinal order.
fn children(conn: &Connection) -> HashMap<Vec<u8>, Vec<Vec<u8>>> {
    let mut stmt = conn
        .prepare(
            "SELECT p.node_hash, c.node_hash FROM node_edge e \
               JOIN node_content p ON p.content_id = e.parent_id \
               JOIN node_content c ON c.content_id = e.child_id \
              ORDER BY e.parent_id, e.ordinal",
        )
        .unwrap();
    let mut out: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    for row in stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap() {
        let (p, c): (Vec<u8>, Vec<u8>) = row.unwrap();
        out.entry(p).or_default().push(c);
    }
    out
}

fn sources(variant: u8) -> Vec<(&'static str, Vec<u8>)> {
    let n = variant as u32;
    vec![
        (
            "a.go",
            format!(
                "package a\n\nfunc A(x int) int {{\n\treturn x + {}\n}}\n",
                n % 3
            )
            .into_bytes(),
        ),
        (
            "b.go",
            format!(
                "package a\n\nfunc B() string {{\n\treturn \"{}\"\n}}\n",
                n % 2
            )
            .into_bytes(),
        ),
        (
            "c.rs",
            format!("fn c() -> u8 {{ {} }}\n", n % 4).into_bytes(),
        ),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    /// (IM) over write sequences: a tree is edited and re-parsed into one
    /// arena step by step (variants repeat, so earlier bytes and subtrees
    /// return). For `source_blobs` and `node_content`, the address-keyed
    /// tables a parse writes, whatever was first stored under an address is
    /// what every later step holds under it, and it still satisfies
    /// σ = address: `source_blobs` bytes hash to their key, and every
    /// `node_content` row re-derives its `node_hash` from its kind, token and
    /// `node_edge` children. (`capnp_blobs` left the projection at
    /// projection-v8; `content_chunks` is `leyline-fs`'s test of this name.)
    #[test]
    fn im_address_is_never_rebound(steps in proptest::collection::vec(0u8..12, 1..5)) {
        let src = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        let conn = Connection::open(out.path().join("arena.db")).unwrap();
        let mut first_blobs: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut first_nodes: BTreeMap<Vec<u8>, NodeRow> = BTreeMap::new();
        for variant in steps {
            write_tree(src.path(), &sources(variant));
            cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();

            for (hash, bytes) in blob_rows(&conn) {
                let sigma = bytes.as_slice().hash();
                prop_assert_eq!(sigma.as_bytes().as_slice(), hash.as_slice());
                let seen = first_blobs.entry(hash).or_insert_with(|| bytes.clone());
                prop_assert_eq!(&*seen, &bytes, "a source blob was rebound");
            }
            let kids = children(&conn);
            for (hash, row) in node_rows(&conn) {
                let (tag, kind, _, _, token, arity) = &row;
                let preimage = match tag {
                    0 => leaf_preimage(kind, token.as_deref().unwrap_or("")),
                    _ => {
                        let c = kids.get(&hash).cloned().unwrap_or_default();
                        prop_assert_eq!(c.len() as i64, *arity, "an internal row's edges are its arity");
                        internal_preimage(kind, &c)
                    }
                };
                let sigma = preimage.as_slice().hash();
                prop_assert_eq!(sigma.as_bytes().as_slice(), hash.as_slice(), "node row does not re-derive its address");
                let seen = first_nodes.entry(hash).or_insert_with(|| row.clone());
                prop_assert_eq!(&*seen, &row, "a node_content row was rebound");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// §1.3 Merkle
// ---------------------------------------------------------------------------

/// Decade F4's n.
const N_FILES: usize = 50_000;

/// `d{a}/d{b}/f{i}.rs`: 20 top directories × 50 subdirectories × 50 files,
/// so depth 3 and fanout at most 50.
fn store_leaves() -> Vec<TreeLeaf> {
    (0..N_FILES)
        .map(|i| {
            let path = format!("d{:02}/d{:02}/f{i:05}.rs", i / 2500, (i / 50) % 50);
            TreeLeaf::regular(path, format!("file {i}").as_bytes().hash())
        })
        .collect()
}

const DEPTH: usize = 3;
const FANOUT: usize = 50;
/// A directory node folds two entries per child (name, then child), so a
/// Merkle store refolds at most `2 · depth · fanout` entries per edit.
const MERKLE_BOUND: usize = 2 * DEPTH * FANOUT;

/// The fold nodes the shipped `treeRoot` construction computes, each with
/// the number of entries it folds. `tree-root/v1` is one `RowSet` fold over
/// every leaf (`tree_root_of`): its only node is the root, with `2n`
/// entries. That this is the shipped fold, and not a model of it, is
/// checked: the root rebuilt here from exactly those entries equals
/// `tree_root_of`'s.
fn shipped_fold_nodes(leaves: &[TreeLeaf], pid: Hash) -> Vec<(Hash, usize)> {
    let entries = tree_root_entries(leaves);
    let root = tree_root_of(leaves, pid);
    assert_eq!(
        documented_tree_root(leaves, pid),
        root,
        "the node set is the shipped fold"
    );
    vec![(root, entries.len())]
}

/// Entries refolded by one leaf edit: the entry counts of every fold node
/// whose address changed. That is the least work any implementation of the
/// construction can do, memoized or not, since an unchanged node needs no
/// refold and a changed one needs all of its entries.
fn one_leaf_edit_cost() -> (usize, Hash, Hash) {
    let pid = Hash::from_bytes([0x5a; 32]);
    let mut leaves = store_leaves();
    let before = shipped_fold_nodes(&leaves, pid);
    leaves[N_FILES / 2].content = b"edited".hash();
    let after = shipped_fold_nodes(&leaves, pid);
    let unchanged: BTreeSet<Hash> = before.iter().map(|(h, _)| *h).collect();
    let cost = after
        .iter()
        .filter(|(h, _)| !unchanged.contains(h))
        .map(|(_, n)| n)
        .sum();
    (cost, before[0].0, after[0].0)
}

/// Σ §1.3 / decade F4 on the shipped store identity: one leaf edit in a
/// 50,000-file store refolds at most `c · depth · fanout` entries.
///
/// RED today: `treeRoot` is one flat fold, so the edit refolds all
/// 100,000 entries. ADR-0047 M1 (`ley-line-open-73c6ee`) replaces it with the
/// directory-Merkle `storeRoot`; that PR points `shipped_fold_nodes` at the
/// directory nodes and removes this `ignore`.
#[test]
#[ignore = "RED until ADR-0047 M1 (73c6ee): treeRoot is one flat fold over 2n entries"]
fn merkle_one_leaf_edit_rehashes_one_path() {
    let (cost, before, after) = one_leaf_edit_cost();
    assert_ne!(before, after, "the edit must reach the root");
    assert!(
        cost <= MERKLE_BOUND,
        "one leaf edit refolded {cost} entries; Σ §1.3 bounds it by {MERKLE_BOUND}"
    );
}

/// The measured form of [`merkle_one_leaf_edit_rehashes_one_path`], run in
/// every `task ci`: prints the cost and pins today's flat-fold value, 2n.
/// M1 (`ley-line-open-73c6ee`) fails this pin and must delete it along with
/// the strict test's `ignore`.
#[test]
fn merkle_one_leaf_edit_rehashes_one_path_measured() {
    let (cost, before, after) = one_leaf_edit_cost();
    assert_ne!(before, after);
    eprintln!(
        "merkle_one_leaf_edit_rehashes_one_path: {cost} entries refolded for one edit \
         in {N_FILES} files (Σ §1.3 bound {MERKLE_BOUND}): RED until 73c6ee"
    );
    assert_eq!(
        cost,
        2 * N_FILES,
        "the flat tree-root/v1 refolds every entry"
    );
}

/// Longest root-to-leaf path in a `node_content` / `node_edge` DAG, in
/// edges.
fn height(kids: &HashMap<Vec<u8>, Vec<Vec<u8>>>) -> usize {
    fn h(
        n: &[u8],
        kids: &HashMap<Vec<u8>, Vec<Vec<u8>>>,
        memo: &mut HashMap<Vec<u8>, usize>,
    ) -> usize {
        if let Some(&d) = memo.get(n) {
            return d;
        }
        let d = kids
            .get(n)
            .map(|cs| 1 + cs.iter().map(|c| h(c, kids, memo)).max().unwrap_or(0))
            .unwrap_or(0);
        memo.insert(n.to_vec(), d);
        d
    }
    let mut memo = HashMap::new();
    kids.keys()
        .map(|n| h(n, kids, &mut memo))
        .max()
        .unwrap_or(0)
}

/// Σ §1.3 for `node_hash`: one leaf edit adds at most depth + 1
/// `node_content` rows (the leaf and its ancestors), and those rows form a
/// single path. Passes today: `node_hash` is a bottom-up Merkle fold.
#[test]
fn node_hash_edit_touches_depth_plus_one_rows() {
    let src = TempDir::new().unwrap();
    let body = |n: u32| {
        format!(
            "package a\n\nfunc Answer(x int) int {{\n\tif x > 0 {{\n\t\tfor i := 0; i < x; i++ {{\n\t\t\tx = x + {n}\n\t\t}}\n\t}}\n\treturn x\n}}\n"
        )
        .into_bytes()
    };
    write_tree(src.path(), &[("a.go", body(41))]);
    let out = TempDir::new().unwrap();
    let conn = Connection::open(out.path().join("arena.db")).unwrap();
    cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    let before = node_hashes(&conn);
    let depth = height(&children(&conn));

    write_tree(src.path(), &[("a.go", body(42))]);
    cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    let added: BTreeSet<Vec<u8>> = node_hashes(&conn).difference(&before).cloned().collect();

    assert!(
        depth >= 5,
        "a nested body, so the bound is not vacuous (height {depth})"
    );
    assert!(
        added.len() >= 2,
        "the edit must add the leaf and its parent"
    );
    assert!(
        added.len() <= depth + 1,
        "one leaf edit added {} node_content rows; the bound is depth + 1 = {}",
        added.len(),
        depth + 1
    );
    // A single path: every added internal row has exactly one added child.
    let kids = children(&conn);
    for row in &added {
        if let Some(cs) = kids.get(row) {
            assert_eq!(cs.iter().filter(|c| added.contains(*c)).count(), 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Law (c): construction separation
// ---------------------------------------------------------------------------

/// Leaves of the arena: `(kind, token)` of every leaf row.
fn leaves(conn: &Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare("SELECT kind, token FROM node_content WHERE node_tag = 0 ORDER BY node_hash")
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
        ))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

/// The `ley-line-open-73add7` reproduction, generalised: parse a tree, then
/// add a file whose bytes are the preimage of one of its leaves, and parse
/// again. Returns the addresses that name both a blob and an AST node.
fn crafted_aliases() -> BTreeSet<Vec<u8>> {
    let src = TempDir::new().unwrap();
    write_tree(src.path(), &fixture_files());
    let out = TempDir::new().unwrap();
    let conn = Connection::open(out.path().join("arena.db")).unwrap();
    cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    let crafted: Vec<(&str, Vec<u8>)> = leaves(&conn)
        .into_iter()
        .take(4)
        .enumerate()
        .map(|(i, (kind, token))| {
            (
                Box::leak(format!("crafted/{i}.bin").into_boxed_str()) as &str,
                leaf_preimage(&kind, &token),
            )
        })
        .chain(std::iter::once((
            "crafted/fn.bin",
            leaf_preimage("fn", "fn"),
        )))
        .collect();
    write_tree(src.path(), &crafted);
    cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    node_hashes(&conn)
        .intersection(&blob_hashes(&conn))
        .cloned()
        .collect()
}

/// Law (c) for `node_hash` (ADR-0047 §1.2): no file's `blob_hash` is the
/// `node_hash` of an AST node, so writing a construction's encoding as a
/// file does not mint its address. The fixed case is `73add7`'s 18-byte
/// file `llo/ast/v1\0\0\x02fn\x02fn`, the preimage of the Rust leaf `fn`.
///
/// RED today: `node_hash` is plain σ over an in-band-prefixed preimage, so
/// σ_bytes reaches it. ADR-0047 M2 (`ley-line-open-73add7`, `merkle-ast-v3`
/// under `derive_key`) turns it green and removes this `ignore`. The keyed
/// constructions (`PartitionSpec`, head digest) pass law (c) in
/// `leyline-core`'s test of this name.
#[test]
#[ignore = "RED until ADR-0047 M2 (73add7): node_hash is plain σ over a prefixed preimage"]
fn crafted_preimage_does_not_alias_across_constructions() {
    let aliases = crafted_aliases();
    assert!(
        aliases.is_empty(),
        "{} addresses name both a blob and an AST node",
        aliases.len()
    );
}

/// The measured form, run in every `task ci`: every crafted file aliases a
/// node today. M2 (`ley-line-open-73add7`) fails this pin and must delete it
/// along with the strict test's `ignore`.
#[test]
fn crafted_preimage_does_not_alias_across_constructions_measured() {
    let aliases = crafted_aliases();
    let fixed = leaf_preimage("fn", "fn");
    assert_eq!(fixed.len(), 18, "the 73add7 file is 18 bytes");
    eprintln!(
        "crafted_preimage_does_not_alias_across_constructions: {} blob/node aliases: RED until 73add7",
        aliases.len()
    );
    assert!(
        aliases.contains(fixed.as_slice().hash().as_bytes().as_slice()),
        "the 73add7 file's blob_hash is the Rust `fn` leaf's node_hash today"
    );
    assert_eq!(aliases.len(), 5, "every crafted preimage aliases its leaf");
}
