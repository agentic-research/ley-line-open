//! Σ axiom falsification suite — the `leyline-core` slice (ADR-0047 §5,
//! bead `ley-line-open-10d71a`).
//!
//! Decade T1.3 ("axiom → executable falsification", `substrate.rs` module
//! docs) is this suite. Each axiom of `docs/decades/2026-merkle-cas-substrate.md`
//! §1.2 is a test that goes red when the axiom fails on shipped code, named as
//! ADR-0047 §5 names it. A test whose name appears in several crates is one
//! test split by where the construction lives: this file covers the
//! constructions and stores `leyline-core` owns (σ_bytes, the `PartitionSpec`
//! fold, the head digest, `MemBlobStore`, `FsBlobStore`); `leyline-fs`,
//! `leyline-cli-lib`, `leyline-cache` and `leyline-runtime` hold the rest, and
//! `Controller`'s publish (IM) lives beside its private hooked seam in
//! `control.rs`.
//!
//! The known-answer vectors are one checked-in file,
//! `ll-core/schema-spec/sigma/v1/test-vectors/constructions.json`, so a Go or
//! TypeScript consumer checks the same bytes these tests do.

use std::path::PathBuf;

use leyline_core::Ref;
use leyline_core::construction::{ArenaImage, Blob, Chunk, HeadDigest};
use leyline_core::head_digest::HEAD_DIGEST_CONTEXT;
use leyline_core::partition::PARTITION_CONTEXT;
use leyline_core::{
    BlobStore, ContentAddressed, Domain, Entry, FsBlobStore, Hash, HeadClaim, MemBlobStore,
    PartitionSpec, head_digest,
};
use proptest::prelude::*;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Vector loading
// ---------------------------------------------------------------------------

fn vectors() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../schema-spec/sigma/v1/test-vectors/constructions.json");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "Σ vectors must ship with the schema: {}: {e}",
            path.display()
        )
    });
    serde_json::from_slice(&bytes).expect("Σ vectors are JSON")
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd-length hex {s:?}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digit"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_of(v: &Value) -> Hash {
    let bytes: [u8; 32] = unhex(v.as_str().expect("hash is a hex string"))
        .try_into()
        .expect("hash is 32 bytes");
    Hash::from_bytes(bytes)
}

fn cases<'v>(v: &'v Value, key: &str) -> &'v [Value] {
    let cases = v[key]
        .as_array()
        .unwrap_or_else(|| panic!("vectors carry a {key:?} array"));
    assert!(!cases.is_empty(), "{key:?} has no cases: it gates nothing");
    cases
}

/// The input bytes of a σ_bytes case: `inputHex`, or `repeatByte` × `len`.
fn sigma_input(case: &Value) -> Vec<u8> {
    if let Some(h) = case["inputHex"].as_str() {
        return unhex(h);
    }
    let byte = unhex(case["repeatByte"].as_str().expect("repeatByte"))[0];
    vec![byte; case["len"].as_u64().expect("len") as usize]
}

fn domain_of(name: &str) -> Domain {
    match name {
        "ByteStream" => Domain::ByteStream,
        "ChunkSet" => Domain::ChunkSet,
        "RowSet" => Domain::RowSet,
        other => panic!("unknown domain {other:?}"),
    }
}

fn domain_tag(d: Domain) -> u8 {
    match d {
        Domain::ByteStream => 1,
        Domain::ChunkSet => 2,
        Domain::RowSet => 3,
    }
}

fn partition_case(case: &Value) -> (PartitionSpec, Vec<Entry>) {
    let spec = PartitionSpec {
        domain: domain_of(case["domain"].as_str().expect("domain")),
        scheme: case["scheme"].as_str().expect("scheme").to_string(),
        params: unhex(case["paramsHex"].as_str().expect("paramsHex")),
        canon_version: case["canonVersion"].as_u64().expect("canonVersion") as u32,
    };
    let entries = case["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| Entry {
            addr: hash_of(&e["addr"]),
            a: e["a"].as_u64().expect("a"),
            b: e["b"].as_u64().expect("b"),
        })
        .collect();
    (spec, entries)
}

/// `enc_D(x)` of the partition fold, rebuilt from ADR-0032 D2's documented
/// encoding rather than by calling the fold: what a second implementation
/// hashes under [`PARTITION_CONTEXT`].
fn partition_preimage(spec: &PartitionSpec, entries: &[Entry]) -> Vec<u8> {
    let mut p = vec![domain_tag(spec.domain)];
    p.extend_from_slice(&spec.canon_version.to_le_bytes());
    p.extend_from_slice(&(spec.scheme.len() as u64).to_le_bytes());
    p.extend_from_slice(spec.scheme.as_bytes());
    p.extend_from_slice(&(spec.params.len() as u64).to_le_bytes());
    p.extend_from_slice(&spec.params);
    p.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    let mut ordered: Vec<&Entry> = entries.iter().collect();
    if spec.domain != Domain::ByteStream {
        ordered.sort_by_key(|e| (e.addr, e.a, e.b));
    }
    for e in ordered {
        p.extend_from_slice(e.addr.as_bytes());
        p.extend_from_slice(&e.a.to_le_bytes());
        p.extend_from_slice(&e.b.to_le_bytes());
    }
    p
}

fn opt_hash(v: &Value) -> Option<Hash> {
    (!v.is_null()).then(|| hash_of(v))
}

fn head_case(case: &Value) -> HeadClaim {
    HeadClaim::new(
        case["generation"].as_u64().expect("generation"),
        hash_of(&case["rootHash"]),
        hash_of(&case["parentHash"]),
        opt_hash(&case["parentDigest"]),
        hash_of(&case["treeRoot"]),
        hash_of(&case["parserId"]),
    )
    .expect("vector claims are well-formed")
}

/// `enc_D(x)` of the head digest (`head_digest.rs` module docs).
fn head_preimage(case: &Value) -> Vec<u8> {
    let mut p = case["generation"]
        .as_u64()
        .expect("generation")
        .to_le_bytes()
        .to_vec();
    for k in ["rootHash", "parentHash", "treeRoot", "parserId"] {
        p.extend_from_slice(hash_of(&case[k]).as_bytes());
    }
    match opt_hash(&case["parentDigest"]) {
        None => p.push(0),
        Some(d) => {
            p.push(1);
            p.extend_from_slice(d.as_bytes());
        }
    }
    p
}

fn derive_key(context: &str, preimage: &[u8]) -> Hash {
    let mut h = blake3::Hasher::new_derive_key(context);
    h.update(preimage);
    Hash::from_bytes(*h.finalize().as_bytes())
}

// ---------------------------------------------------------------------------
// (DET) — known-answer vectors
// ---------------------------------------------------------------------------

/// (DET) σ is deterministic across hosts and runs: every construction
/// `leyline-core` owns reproduces its checked-in vector, and every pinned
/// preimage is the one the construction hashes, rebuilt from its documented
/// encoding. Runs on every CI host (Linux and macOS), so a host-dependent
/// encoding (endianness, `usize` width) fails here.
#[test]
fn det_known_answer_vectors() {
    let v = vectors();

    for case in cases(&v, "sigmaBytes") {
        let input = sigma_input(case);
        assert_eq!(
            hex(input.as_slice().hash().as_bytes()),
            case["address"].as_str().unwrap(),
            "σ_bytes drifted for {}",
            case["name"]
        );
        // The typed constructions over σ_bytes name the same bytes.
        let pinned = hash_of(&case["address"]);
        assert_eq!(*Ref::<Blob>::derive(&input).as_hash(), pinned);
        assert_eq!(*Ref::<Chunk>::derive(&input).as_hash(), pinned);
        assert_eq!(*Ref::<ArenaImage>::derive(&input).as_hash(), pinned);
    }

    for case in cases(&v, "partition") {
        let (spec, entries) = partition_case(case);
        let preimage = partition_preimage(&spec, &entries);
        assert_eq!(
            hex(&preimage),
            case["preimageHex"].as_str().unwrap(),
            "partition preimage drifted for {}",
            case["name"]
        );
        let address = spec.address(&entries);
        assert_eq!(
            hex(address.as_bytes()),
            case["address"].as_str().unwrap(),
            "partition fold drifted for {}",
            case["name"]
        );
        assert_eq!(
            derive_key(PARTITION_CONTEXT, &preimage),
            address,
            "the pinned address is derive_key(PARTITION_CONTEXT, preimage) for {}",
            case["name"]
        );
    }

    for case in cases(&v, "headDigest") {
        let claim = head_case(case);
        let digest = head_digest(&claim);
        assert_eq!(
            Ref::<HeadDigest>::verified(hash_of(&case["digest"]), &claim),
            Some(Ref::<HeadDigest>::derive(&claim)),
            "the typed head digest names the pinned value"
        );
        let preimage = head_preimage(case);
        assert_eq!(hex(&preimage), case["preimageHex"].as_str().unwrap());
        assert_eq!(
            hex(digest.as_bytes()),
            case["digest"].as_str().unwrap(),
            "head digest drifted for {}",
            case["name"]
        );
        assert_eq!(derive_key(HEAD_DIGEST_CONTEXT, &preimage), digest);
    }
}

/// The input of a `deriveKey` case: BLAKE3's test-vector pattern, byte `i`
/// is `i mod 251`, for `inputLen` bytes.
fn derive_key_input(case: &Value) -> Vec<u8> {
    let len = case["inputLen"].as_u64().expect("inputLen") as usize;
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// (DET) for the keyed primitive: `substrate::derive_key`, the one
/// `derive_key` entry point non-Rust consumers reach through
/// `leyline-cas-ffi` (ADR-0047 D-i, the M3 prerequisite; bead
/// `ley-line-open-12e252`), reproduces BLAKE3's own published vectors, and
/// is not σ_bytes of the same input (the keyed mode's domain flags differ).
#[test]
fn derive_key_reproduces_blake3_test_vectors() {
    let v = vectors();
    for case in cases(&v, "deriveKey") {
        let context = case["context"].as_str().expect("context");
        let input = derive_key_input(case);
        let key = leyline_core::substrate::derive_key(context, &input);
        assert_eq!(
            hex(key.as_bytes()),
            case["key"].as_str().unwrap(),
            "derive_key drifted for {}",
            case["name"]
        );
        assert_eq!(
            key,
            derive_key(context, &input),
            "agrees with the reference hasher"
        );
        assert_ne!(
            key,
            input.as_slice().hash(),
            "derive_key is not σ_bytes for {}",
            case["name"]
        );
    }
}

// ---------------------------------------------------------------------------
// Construction laws (ADR-0047 §1.2) — the slices core owns
// ---------------------------------------------------------------------------

type ClaimParts = (u64, Hash, Hash, Option<Hash>, Hash, Hash);

fn claim(p: &ClaimParts) -> HeadClaim {
    HeadClaim::new(p.0, p.1, p.2, p.3, p.4, p.5).expect("strategy filters unstamped claims")
}

fn claim_strategy() -> impl Strategy<Value = ClaimParts> {
    let h = any::<[u8; 32]>().prop_map(Hash::from_bytes);
    (
        any::<u64>(),
        h.clone(),
        h.clone(),
        proptest::option::of(h.clone()),
        h.clone(),
        h,
    )
        .prop_filter("a claim needs a stamped tree and parser", |t| {
            HeadClaim::new(t.0, t.1, t.2, t.3, t.4, t.5).is_some()
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Law (a) for σ_bytes: distinct byte strings get distinct addresses and
    /// one byte string one address (presentation — `Vec`, slice — is erased).
    /// The `PartitionSpec` slice of law (a) is `partition.rs`'s
    /// `ordered_entry_lists_are_injective` and
    /// `set_addresses_are_equal_exactly_for_equal_multisets`; `treeRoot`'s is
    /// `tree_root.rs`'s fold-law proptests.
    #[test]
    fn construction_equality_is_value_equality(
        x in proptest::collection::vec(any::<u8>(), 0..256),
        y in proptest::collection::vec(any::<u8>(), 0..256),
        c in claim_strategy(),
        d in claim_strategy(),
    ) {
        prop_assert_eq!(x.as_slice().hash() == y.as_slice().hash(), x == y);
        prop_assert_eq!(x.clone().hash(), x.as_slice().hash());
        prop_assert_eq!(head_digest(&claim(&c)) == head_digest(&claim(&d)), c == d);
    }

    /// Law (c), construction separation, for the keyed constructions core
    /// owns: σ_bytes of a construction's own encoding never lands on that
    /// construction's address, so an author who writes `enc_D(x)` as a file
    /// does not mint `σ_D(x)`. Non-vacuity: the same preimage under the
    /// construction's derive-key context IS the address, so the encoding
    /// rebuilt here is the one the fold hashes.
    #[test]
    fn crafted_preimage_does_not_alias_across_constructions(
        domain in prop_oneof![
            Just(Domain::ByteStream),
            Just(Domain::ChunkSet),
            Just(Domain::RowSet),
        ],
        scheme in "[a-z/]{0,24}",
        params in proptest::collection::vec(any::<u8>(), 0..40),
        raw in proptest::collection::vec((any::<[u8; 32]>(), any::<u64>(), any::<u64>()), 0..8),
        c in claim_strategy(),
    ) {
        let spec = PartitionSpec { domain, scheme, params, canon_version: 1 };
        let entries: Vec<Entry> = raw
            .into_iter()
            .map(|(addr, a, b)| Entry { addr: Hash::from_bytes(addr), a, b })
            .collect();
        let address = spec.address(&entries);
        let preimage = partition_preimage(&spec, &entries);
        prop_assert_eq!(derive_key(PARTITION_CONTEXT, &preimage), address);
        prop_assert_ne!(preimage.as_slice().hash(), address);

        let digest = head_digest(&claim(&c));
        let (g, r, p, pd, t, i) = c;
        let mut hp = g.to_le_bytes().to_vec();
        for x in [r, p, t, i] {
            hp.extend_from_slice(x.as_bytes());
        }
        match pd {
            None => hp.push(0),
            Some(d) => { hp.push(1); hp.extend_from_slice(d.as_bytes()); }
        }
        prop_assert_eq!(derive_key(HEAD_DIGEST_CONTEXT, &hp), digest);
        prop_assert_ne!(hp.as_slice().hash(), digest);
    }
}

// ---------------------------------------------------------------------------
// (CA) — round trip and verify-on-read, for the stores core ships
// ---------------------------------------------------------------------------

/// Payloads at the boundaries the ADR names: empty, sub-floor (below CDC's
/// 8 KiB `MIN_CHUNK`), and above CDC's 128 KiB `MAX_CHUNK`.
fn boundary_payloads() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        b"x".to_vec(),
        (0..8 * 1024 - 1).map(|i| (i % 251) as u8).collect(),
        (0..128 * 1024 + 7).map(|i| (i * 31 % 253) as u8).collect(),
    ]
}

fn round_trip(store: &mut dyn BlobStore, bytes: &[u8]) {
    let h = store.put(bytes).expect("put");
    assert_eq!(h, bytes.hash(), "a store returns σ(v) from put(v)");
    assert_eq!(
        store.get(h).expect("get").as_deref(),
        Some(bytes),
        "(CA) ρ(σ(v)) = v"
    );
    assert!(store.contains(h).expect("contains"));
}

/// (CA) ρ(σ(v)) = v for every `BlobStore` core ships, at the boundary sizes
/// and over arbitrary bytes. `leyline-fs`'s test of the same name covers the
/// chunk pool and blob-manifest reconstruction; `leyline-cli-lib`'s covers
/// `source_blobs`.
#[test]
fn ca_round_trip_every_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = FsBlobStore::new(dir.path()).unwrap();
    let mut mem = MemBlobStore::new();
    for bytes in boundary_payloads() {
        round_trip(&mut fs, &bytes);
        round_trip(&mut mem, &bytes);
    }
    let stores = std::cell::RefCell::new((fs, mem));
    proptest!(ProptestConfig::with_cases(64), |(bytes in proptest::collection::vec(any::<u8>(), 0..4096))| {
        let (fs, mem) = &mut *stores.borrow_mut();
        round_trip(fs, &bytes);
        round_trip(mem, &bytes);
    });
}

/// (CA) as verify-on-read, decade F3: flip one stored byte and ρ fails
/// closed — an error, never the tampered bytes under the original address.
/// `MemBlobStore` holds its bytes privately, so the store with a physical
/// payload a writer outside the API can reach is the one tampered here.
#[test]
fn ca_tampered_payload_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = FsBlobStore::new(dir.path()).unwrap();
    for bytes in boundary_payloads().into_iter().filter(|b| !b.is_empty()) {
        let h = fs.put(&bytes).unwrap();
        let path = fs.path_for(&h);
        let mut stored = std::fs::read(&path).unwrap();
        let i = stored.len() / 2;
        stored[i] ^= 0x01;
        std::fs::write(&path, &stored).unwrap();
        assert!(
            fs.get(h).is_err(),
            "a tampered {}-byte payload must be refused, not served",
            bytes.len()
        );
    }
}
