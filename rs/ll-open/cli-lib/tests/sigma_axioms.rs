//! Σ axiom falsification suite — the `leyline-cli-lib` slice (ADR-0047 §5,
//! bead `ley-line-open-10d71a`).
//!
//! The constructions and stores this crate owns: `source_blobs` (ρ over
//! file bytes), `parserId`, `storeRoot`, `node_hash` and the parse that
//! produces them. Tests named as ADR-0047 §5 names them; a name that also
//! appears in `leyline-core` or `leyline-fs` is the same test over the
//! stores those crates own.
//!
//! No claim here is known RED. The two that were run in every `task ci`
//! since the steps that turned them green:
//!
//! - `merkle_one_leaf_edit_rehashes_one_path`, since ADR-0047 M1
//!   (`ley-line-open-73c6ee`) replaced the flat `treeRoot` with the
//!   directory Merkle `storeRoot`;
//! - `crafted_preimage_does_not_alias_across_constructions`, since ADR-0047
//!   M2 (`ley-line-open-73add7`) keyed `node_hash` under
//!   `leyline/ast-node/v3` (merkle-ast-v3): before it, a file whose bytes
//!   were a leaf's prefixed preimage had `blob_hash == node_hash`.
//!
//! `run_id`'s half of law (c) is in `leyline-runtime`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use leyline_cli_lib::cmd_parse;
use leyline_cli_lib::cmd_parse::{AstNode, AstNodeWitness};
use leyline_cli_lib::store_root::{
    DerivedFrom, Mode, Parser, ParserInputs, STORE_TREE_SCHEME, StoreLeaf, StoreRoot, StoreTree,
    store_root_of,
};
use leyline_core::{ContentAddressed, Domain, Entry, Hash, PartitionSpec, Ref};
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

/// `enc(x)` of a `node_hash` leaf, as the `astNode` vectors pin it: the
/// bytes merkle-ast-v3 keys under `leyline/ast-node/v3`.
fn leaf_preimage(kind: &str, token: &str) -> Vec<u8> {
    let mut p = vec![0u8];
    uvarint(&mut p, kind.len() as u64);
    p.extend_from_slice(kind.as_bytes());
    uvarint(&mut p, token.len() as u64);
    p.extend_from_slice(token.as_bytes());
    p
}

/// The same leaf behind merkle-ast-v2's in-band prefix: the plain-σ
/// preimage `73add7` reproduced as a file.
fn legacy_leaf_preimage(kind: &str, token: &str) -> Vec<u8> {
    [b"llo/ast/v1\0".as_slice(), &leaf_preimage(kind, token)].concat()
}

/// The stored `node_hash` as a `Ref<AstNode>`, when the row re-derives it
/// from its witness: a leaf from its kind and token, an internal node from
/// its kind and its `node_edge` children, each of which is verified the same
/// way first. `None` when any row on the way does not re-derive.
fn verify_node(
    hash: &[u8],
    rows: &BTreeMap<Vec<u8>, NodeRow>,
    kids: &HashMap<Vec<u8>, Vec<Vec<u8>>>,
    memo: &mut HashMap<Vec<u8>, Option<Ref<AstNode>>>,
) -> Option<Ref<AstNode>> {
    if let Some(done) = memo.get(hash) {
        return *done;
    }
    let (tag, kind, token, arity) = rows.get(hash)?;
    let claimed = Hash::from_bytes(hash.try_into().ok()?);
    // Every column is part of the witness: a leaf has a token and no
    // children, an internal node no token and `arity` children.
    let verified = if *tag == 0 {
        match token.as_deref() {
            Some(token) if *arity == 0 => {
                Ref::verified(claimed, AstNodeWitness::Leaf { kind, token })
            }
            _ => None,
        }
    } else if *tag != 1 || token.is_some() {
        None
    } else {
        let edges = kids.get(hash).cloned().unwrap_or_default();
        let children: Option<Vec<Ref<AstNode>>> = edges
            .iter()
            .map(|c| verify_node(c, rows, kids, memo))
            .collect();
        children
            .filter(|c| c.len() as i64 == *arity)
            .and_then(|children| {
                Ref::verified(
                    claimed,
                    AstNodeWitness::Internal {
                        kind,
                        children: &children,
                    },
                )
            })
    };
    memo.insert(hash.to_vec(), verified);
    verified
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

/// (DET) `parserId` and `storeRoot` reproduce their checked-in vectors on
/// fixed inputs, and each `storeRoot` vector is the documented directory
/// fold rebuilt here from its leaves by an independent recursive
/// implementation (ADR-0047 D-ii): per directory, its children in
/// name-byte order, `(σ(name), 2i, |name|)` and `(child, 2i + 1, git mode)`,
/// params empty. Each case also pins its directory nodes, so a second
/// runtime checks the nodes before the root. `node_hash`'s vectors are
/// checked against its private fold in `cmd_parse.rs`'s unit tests; the core
/// constructions in `leyline-core`.
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
        let typed = Ref::<Parser>::verified(hash_of(&case["address"]), &inputs)
            .expect("the typed parserId names the pinned value");
        assert_eq!(typed, inputs.parser_ref());
    }
    for case in cases(&v, "storeRoot") {
        assert_eq!(case["scheme"].as_str(), Some(STORE_TREE_SCHEME));
        assert!(
            case.get("parserId").is_none(),
            "storeRoot takes no parser input (law C7)"
        );
        let leaves = vector_leaves(case);
        let tree = StoreTree::from_leaves(&leaves).expect("vector leaves are a tree");
        let address = tree.root();
        assert_eq!(
            address,
            hash_of(&case["address"]),
            "storeRoot drifted for {}",
            case["name"]
        );
        assert_eq!(address, documented_store_root(&leaves));
        assert_eq!(store_root_of(&leaves), Ok(address));
        // Every directory node is pinned too, by path.
        let pinned: BTreeMap<String, (Hash, u64)> = case["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .map(|n| {
                (
                    n["path"].as_str().unwrap().to_string(),
                    (hash_of(&n["address"]), n["entries"].as_u64().unwrap()),
                )
            })
            .collect();
        let computed: BTreeMap<String, (Hash, u64)> = tree
            .nodes()
            .into_iter()
            .map(|n| (n.path, (n.address, n.entries as u64)))
            .collect();
        assert_eq!(
            computed, pinned,
            "storeRoot nodes drifted for {}",
            case["name"]
        );
        assert!(Ref::<StoreRoot>::verified(address, &tree).is_some());
    }
}

/// A vector's leaves: `{path, mode, content}` for a file, `{path, mode:
/// 16384}` (no content) for an empty directory.
fn vector_leaves(case: &Value) -> Vec<StoreLeaf> {
    case["leaves"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let path = l["path"].as_str().unwrap().to_string();
            let mode = l["mode"].as_u64().unwrap() as u32;
            match Mode::from_git(mode) {
                Some(mode) => StoreLeaf::File {
                    path,
                    mode,
                    blob: hash_of(&l["content"]),
                },
                None => {
                    assert_eq!(mode, 0o040000, "a leaf is a file or an empty directory");
                    assert!(l.get("content").is_none(), "a directory has no content");
                    StoreLeaf::EmptyDir { path }
                }
            }
        })
        .collect()
}

/// `store-tree/v1` rebuilt from its documented encoding by an independent
/// implementation: group the leaves by first path component, recurse, and
/// fold each directory's children in name-byte order.
fn documented_store_root(leaves: &[StoreLeaf]) -> Hash {
    enum Item {
        File(u32, Hash),
        Dir(Vec<(String, Option<(u32, Hash)>)>),
    }
    fn fold(items: &[(String, Option<(u32, Hash)>)]) -> Hash {
        // `items`: (path relative to this directory, Some(file) | None for
        // an empty directory).
        let mut children: BTreeMap<String, Item> = BTreeMap::new();
        for (path, leaf) in items {
            match path.split_once('/') {
                None => match leaf {
                    Some((mode, blob)) => {
                        children.insert(path.clone(), Item::File(*mode, *blob));
                    }
                    None => {
                        children
                            .entry(path.clone())
                            .or_insert(Item::Dir(Vec::new()));
                    }
                },
                Some((head, rest)) => {
                    let entry = children
                        .entry(head.to_string())
                        .or_insert(Item::Dir(Vec::new()));
                    let Item::Dir(sub) = entry else {
                        panic!("a leaf under a file")
                    };
                    sub.push((rest.to_string(), *leaf));
                }
            }
        }
        let mut entries = Vec::new();
        for (i, (name, item)) in children.iter().enumerate() {
            let i = i as u64;
            let (child, mode) = match item {
                Item::File(mode, blob) => (*blob, *mode),
                Item::Dir(sub) => (fold(sub), 0o040000),
            };
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
        PartitionSpec {
            domain: Domain::RowSet,
            scheme: "leyline/store-tree/v1".to_string(),
            params: Vec::new(),
            canon_version: 1,
        }
        .address(&entries)
    }
    let items: Vec<(String, Option<(u32, Hash)>)> = leaves
        .iter()
        .map(|l| match l {
            StoreLeaf::File { path, mode, blob } => (path.clone(), Some((mode.git(), *blob))),
            StoreLeaf::EmptyDir { path } => (path.clone(), None),
        })
        .collect();
    fold(&items)
}

/// Law C7 (ADR-0047 D-ii), end to end: one store parsed by two binaries
/// whose `parserId`s differ lands one `storeRoot`, and two `derived_from`
/// stamps that differ only in `parserId`. The second derivation is
/// simulated by restamping `derived_from` under another `ParserInputs`;
/// the store root is recomputed from the arena, which has no parser input
/// to vary.
#[test]
fn store_root_is_unchanged_when_only_parser_id_changes() {
    let src = TempDir::new().unwrap();
    write_tree(src.path(), &fixture_files());
    let (_out, db) = parse_cold(src.path());
    let conn = Connection::open(&db).unwrap();
    let stamp = DerivedFrom::read(&conn).unwrap().unwrap();
    let mut other = ParserInputs {
        ir_schema_version: "merkle-ast-v3".into(),
        projection_schema_version: "projection-v12".into(),
        extraction_epoch: "99".into(),
        injection_epoch: "inj".into(),
        query_set_epoch: "qse".into(),
        grammars: vec![("rust".into(), Hash::from_bytes([9; 32]))],
    };
    assert_ne!(other.parser_id(), stamp.parser_id);
    let store = leyline_cli_lib::store_root::store_root(&conn).unwrap();
    assert_eq!(store, stamp.store_root, "the arena folds to its stamp");
    // Every parser input moves parserId and none moves storeRoot.
    for bump in 0..3 {
        match bump {
            0 => other.extraction_epoch.push('1'),
            1 => other.grammars[0].1 = Hash::from_bytes([bump as u8; 32]),
            _ => other.query_set_epoch.push('x'),
        }
        let restamped = DerivedFrom {
            parser_id: other.parser_id(),
            ..stamp.clone()
        };
        assert_ne!(restamped.parser_id, stamp.parser_id);
        assert_eq!(restamped.store_root, store);
        assert_eq!(
            leyline_cli_lib::store_root::store_root(&conn).unwrap(),
            store
        );
    }
    // And one stored byte moves it.
    conn.execute(
        "UPDATE _tree SET blob_hash = ?1 WHERE path = (SELECT min(path) FROM _tree WHERE blob_hash IS NOT NULL)",
        params![b"changed".as_slice().hash().as_bytes().as_slice()],
    )
    .unwrap();
    assert_ne!(
        leyline_cli_lib::store_root::store_root(&conn).unwrap(),
        store
    );
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
/// name the same `node_hash` set, the same `storeRoot`/`parserId` stamp, and
/// the same `Head.rootHash`. Both parse one source directory, so the
/// absolute paths and mtimes `rootHash`'s segments carry are equal and the
/// comparison needs no masking.
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

/// `(node_tag, kind, token, arity)`: every `node_content` column but the
/// arena-local `content_id` (projection-v12).
type NodeRow = (i64, String, Option<String>, i64);

fn node_rows(conn: &Connection) -> BTreeMap<Vec<u8>, NodeRow> {
    let mut stmt = conn
        .prepare("SELECT node_hash, node_tag, kind, token, arity FROM node_content")
        .unwrap();
    stmt.query_map([], |r| {
        Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
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
            let rows = node_rows(&conn);
            let mut memo = HashMap::new();
            for (hash, row) in &rows {
                prop_assert!(
                    verify_node(hash, &rows, &kids, &mut memo).is_some(),
                    "a node_content row does not re-derive its address from its witness"
                );
                let seen = first_nodes.entry(hash.clone()).or_insert_with(|| row.clone());
                prop_assert_eq!(&*seen, row, "a node_content row was rebound");
            }
        }
    }
}

/// `verify_node` checks every `node_content` column, not just the hashed
/// ones (projection-v12: every column is a function of `node_hash`). A row
/// whose `arity` or `token` disagrees with its tag is refused, so the
/// (IM) test above cannot pass a row that is not its key's function.
#[test]
fn verify_node_refuses_a_row_whose_columns_disagree_with_its_key() {
    let src = TempDir::new().unwrap();
    write_tree(src.path(), &fixture_files());
    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    let all_verify = |conn: &Connection| {
        let (rows, kids) = (node_rows(conn), children(conn));
        let mut memo = HashMap::new();
        rows.keys()
            .all(|h| verify_node(h, &rows, &kids, &mut memo).is_some())
    };
    assert!(all_verify(&conn), "a fresh parse verifies");
    for (mutate, undo) in [
        (
            "UPDATE node_content SET arity = 7 WHERE content_id = \
             (SELECT min(content_id) FROM node_content WHERE node_tag = 0)",
            "UPDATE node_content SET arity = 0 WHERE node_tag = 0",
        ),
        (
            "UPDATE node_content SET token = 'x' WHERE content_id = \
             (SELECT min(content_id) FROM node_content WHERE node_tag = 1)",
            "UPDATE node_content SET token = NULL WHERE node_tag = 1",
        ),
    ] {
        conn.execute(mutate, []).unwrap();
        assert!(!all_verify(&conn), "`{mutate}` still verifies");
        conn.execute(undo, []).unwrap();
        assert!(all_verify(&conn));
    }
}

// ---------------------------------------------------------------------------
// §1.3 Merkle
// ---------------------------------------------------------------------------

/// Decade F4's n.
const N_FILES: usize = 50_000;

/// `d{a}/d{b}/f{i}.rs`: 20 top directories × 50 subdirectories × 50 files,
/// so depth 3 and fanout at most 50.
fn store_leaves() -> Vec<StoreLeaf> {
    (0..N_FILES)
        .map(|i| {
            let path = format!("d{:02}/d{:02}/f{i:05}.rs", i / 2500, (i / 50) % 50);
            StoreLeaf::regular(path, format!("file {i}").as_bytes().hash())
        })
        .collect()
}

const DEPTH: usize = 3;
const FANOUT: usize = 50;
/// A directory node folds two entries per child (name, then child), so a
/// Merkle store refolds at most `2 · depth · fanout` entries per edit.
const MERKLE_BOUND: usize = 2 * DEPTH * FANOUT;

/// The fold nodes the shipped `storeRoot` construction computes, each with
/// the number of entries it folds: every directory node of the
/// `store-tree/v1` tree. That these are the shipped fold's nodes, and not a
/// model of them, is checked: the root among them equals both
/// `store_root_of` and the independent `documented_store_root`.
fn shipped_fold_nodes(leaves: &[StoreLeaf]) -> Vec<(Hash, usize)> {
    let nodes = StoreTree::from_leaves(leaves).unwrap().nodes();
    let root = nodes.iter().find(|n| n.path.is_empty()).unwrap().address;
    assert_eq!(store_root_of(leaves), Ok(root));
    assert_eq!(
        documented_store_root(leaves),
        root,
        "the node set is the shipped fold"
    );
    nodes.into_iter().map(|n| (n.address, n.entries)).collect()
}

/// Entries refolded by one leaf edit: the entry counts of every fold node
/// whose address changed. That is the least work any implementation of the
/// construction can do, memoized or not, since an unchanged node needs no
/// refold and a changed one needs all of its entries. Returns the cost and
/// the roots before and after.
fn one_leaf_edit_cost() -> (usize, Hash, Hash) {
    let mut leaves = store_leaves();
    let root = |l: &[StoreLeaf]| store_root_of(l).unwrap();
    let before = shipped_fold_nodes(&leaves);
    let before_root = root(&leaves);
    let StoreLeaf::File { blob, .. } = &mut leaves[N_FILES / 2] else {
        unreachable!("store_leaves holds files")
    };
    *blob = b"edited".hash();
    let after = shipped_fold_nodes(&leaves);
    let unchanged: BTreeSet<Hash> = before.iter().map(|(h, _)| *h).collect();
    let cost = after
        .iter()
        .filter(|(h, _)| !unchanged.contains(h))
        .map(|(_, n)| n)
        .sum();
    (cost, before_root, root(&leaves))
}

/// Σ §1.3 / decade F4 on the shipped store identity: one leaf edit in a
/// 50,000-file store refolds at most `c · depth · fanout` entries. ADR-0047
/// M1's closing test (`ley-line-open-73c6ee`): the flat `tree-root/v1`
/// refolded all 100,000 entries; the directory Merkle `store-tree/v1`
/// refolds the three nodes on the edited path (40 + 100 + 100 = 240).
#[test]
fn merkle_one_leaf_edit_rehashes_one_path() {
    let (cost, before, after) = one_leaf_edit_cost();
    assert_ne!(before, after, "the edit must reach the root");
    eprintln!(
        "merkle_one_leaf_edit_rehashes_one_path: {cost} entries refolded for one edit \
         in {N_FILES} files (Σ §1.3 bound {MERKLE_BOUND})"
    );
    assert!(
        cost <= MERKLE_BOUND,
        "one leaf edit refolded {cost} entries; Σ §1.3 bounds it by {MERKLE_BOUND}"
    );
    assert_eq!(cost, 2 * (20 + 50 + 50), "root, d{{a}}, d{{a}}/d{{b}}");
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
/// add files whose bytes are the encoding of one of its leaves, both as
/// merkle-ast-v3 keys it and behind merkle-ast-v2's in-band prefix, and
/// parse again. Returns the addresses that name both a blob and an AST
/// node, and the arena's blob hashes so a caller can check the crafted
/// files were stored.
fn crafted_aliases() -> (BTreeSet<Vec<u8>>, BTreeSet<Vec<u8>>) {
    let src = TempDir::new().unwrap();
    write_tree(src.path(), &fixture_files());
    let out = TempDir::new().unwrap();
    let conn = Connection::open(out.path().join("arena.db")).unwrap();
    cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    let crafted: Vec<(&str, Vec<u8>)> = leaves(&conn)
        .into_iter()
        .take(4)
        .chain(std::iter::once(("fn".to_string(), "fn".to_string())))
        .enumerate()
        .flat_map(|(i, (kind, token))| {
            [
                (
                    Box::leak(format!("crafted/{i}.v3.bin").into_boxed_str()) as &str,
                    leaf_preimage(&kind, &token),
                ),
                (
                    Box::leak(format!("crafted/{i}.v2.bin").into_boxed_str()) as &str,
                    legacy_leaf_preimage(&kind, &token),
                ),
            ]
        })
        .collect();
    write_tree(src.path(), &crafted);
    cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    let blobs = blob_hashes(&conn);
    (
        node_hashes(&conn).intersection(&blobs).cloned().collect(),
        blobs,
    )
}

/// Law (c) for `node_hash` (ADR-0047 §1.2): no file's `blob_hash` is the
/// `node_hash` of an AST node, so writing a construction's encoding as a
/// file does not mint its address. The fixed case is `73add7`'s 18-byte
/// file `llo/ast/v1\0\0\x02fn\x02fn`, the merkle-ast-v2 preimage of the
/// Rust leaf `fn`, whose `blob_hash` equalled that leaf's `node_hash` until
/// merkle-ast-v3 keyed the construction under `leyline/ast-node/v3`
/// (ADR-0047 M2). The keyed constructions (`PartitionSpec`, head digest)
/// pass law (c) in `leyline-core`'s test of this name.
#[test]
fn crafted_preimage_does_not_alias_across_constructions() {
    let (aliases, blobs) = crafted_aliases();
    assert!(
        aliases.is_empty(),
        "{} addresses name both a blob and an AST node",
        aliases.len()
    );
    let fixed = legacy_leaf_preimage("fn", "fn");
    assert_eq!(fixed.len(), 18, "the 73add7 file is 18 bytes");
    assert!(
        blobs.contains(fixed.as_slice().hash().as_bytes().as_slice()),
        "the 73add7 file was stored, so its blob_hash was compared"
    );
    // The typed refs agree with the bytes: the leaf and the file that spells
    // either encoding of it have different addresses.
    let node = Ref::<AstNode>::derive(AstNodeWitness::Leaf {
        kind: "fn",
        token: "fn",
    });
    for file in [fixed, leaf_preimage("fn", "fn")] {
        let blob = Ref::<leyline_core::construction::Blob>::derive(&file);
        assert_ne!(node.as_hash(), blob.as_hash());
    }
}
