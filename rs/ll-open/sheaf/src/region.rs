//! The region-hash contract for sheaf stalks (bead `ley-line-open-1c5d28`).
//!
//! The daemon's cascade gates every restriction edge on an XOR pre-filter:
//! when `hash(a) ⊕ hash(b)` still equals the edge's `boundary_hash`, the
//! boundary is unchanged and the cascade stops there. That is sound only if a
//! region's hash moves whenever its content moves, and is computed the same
//! way at topology time and at invalidate time. A hash of member *names*
//! (mache's former SHA-256 of sorted node ids) stays put across a content
//! edit, so sending it on invalidate would under-evict.
//!
//! The contract: a region's stalk hash is a keyed fold over its members'
//! `node_hash` values, ADR-0032 D2's [`PartitionSpec`] with
//!
//! ```text
//! regionHash(R) = A({ domain: RowSet, scheme: "leyline/sheaf-region/v1",
//!                     params: parserId (32 bytes), canon_version: 1 },
//!                   { (node_hash(m), a = 0, b = 0) : m ∈ members(R) })
//! boundaryHash(a, b) = regionHash(a) ⊕ regionHash(b)    (bytewise XOR)
//! ```
//!
//! * `node_hash` is content-keyed under ADR-0047 (κ kind, terminal bytes and
//!   child hashes), so an equal fold means equal member content.
//! * The member set is a multiset: `RowSet` sorts entries, so enumeration
//!   order is erased and a duplicated `node_hash` still counts twice.
//! * `parserId` (the `Head.parserId` the arena was parsed under; `_meta`
//!   `derived_from` field 2) is bound in `params`, so two extraction epochs
//!   never produce comparable region hashes.
//! * The fold is domain-separated from every other Σ address by
//!   `PARTITION_CONTEXT` and the scheme string.
//!
//! An edge's `boundary_hash` must be the XOR of the two regions' hashes at
//! topology time, or the pre-filter never short-circuits (sound, but every
//! cascade then falls through to δ⁰ or the depth bound).
//!
//! Test vectors for other runtimes:
//! `clients/go/leyline-schema/vectors/sheaf-region-v1.json`, pinned by this
//! crate's tests.

use leyline_core::{Domain, Entry, Hash, PartitionSpec};

/// Scheme of the region fold. Protocol-visible: a change is a `v2`.
pub const REGION_SCHEME: &str = "leyline/sheaf-region/v1";

/// The content-derived stalk hash of one region: the `RowSet` fold of its
/// members' `node_hash` values under `parser_id`.
pub fn region_hash(parser_id: Hash, member_node_hashes: &[Hash]) -> Hash {
    let entries: Vec<Entry> = member_node_hashes
        .iter()
        .map(|&addr| Entry { addr, a: 0, b: 0 })
        .collect();
    PartitionSpec {
        domain: Domain::RowSet,
        scheme: REGION_SCHEME.to_string(),
        params: parser_id.as_bytes().to_vec(),
        canon_version: 1,
    }
    .address(&entries)
}

/// The `boundary_hash` an edge between two regions carries at topology time:
/// the bytewise XOR the cascade's pre-filter compares against.
pub fn boundary_hash(a: Hash, b: Hash) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (o, (x, y)) in out.iter_mut().zip(a.as_bytes().iter().zip(b.as_bytes())) {
        *o = x ^ y;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{RestrictionEdge, SheafCache, StalkDigest};

    fn h(byte: u8) -> Hash {
        Hash::from_bytes([byte; 32])
    }

    /// The preimage the fold hashes, re-derived independently of
    /// `PartitionSpec::address` so the vector file states every byte.
    fn preimage(parser_id: Hash, members: &[Hash]) -> Vec<u8> {
        let mut sorted: Vec<Hash> = members.to_vec();
        sorted.sort();
        let mut out = vec![3u8]; // Domain::RowSet
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(REGION_SCHEME.len() as u64).to_le_bytes());
        out.extend_from_slice(REGION_SCHEME.as_bytes());
        out.extend_from_slice(&32u64.to_le_bytes());
        out.extend_from_slice(parser_id.as_bytes());
        out.extend_from_slice(&(sorted.len() as u64).to_le_bytes());
        for m in sorted {
            out.extend_from_slice(m.as_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
        }
        out
    }

    #[test]
    fn the_region_hash_is_the_declared_fold() {
        let parser = h(0x5a);
        let members = [h(0x01), h(0x02)];
        let expected = blake3::derive_key(
            leyline_core::partition::PARTITION_CONTEXT,
            &preimage(parser, &members),
        );
        assert_eq!(region_hash(parser, &members).as_bytes(), &expected);
    }

    #[test]
    fn the_region_hash_is_a_function_of_the_member_multiset_and_parser() {
        let parser = h(0x5a);
        let base = region_hash(parser, &[h(1), h(2)]);
        assert_eq!(base, region_hash(parser, &[h(2), h(1)]), "order is erased");
        assert_ne!(base, region_hash(parser, &[h(1), h(3)]), "content moves it");
        assert_ne!(
            base,
            region_hash(parser, &[h(1)]),
            "a removed member moves it"
        );
        assert_ne!(
            region_hash(parser, &[h(1)]),
            region_hash(parser, &[h(1), h(1)]),
            "a multiset: a duplicate counts"
        );
        assert_ne!(
            base,
            region_hash(h(0x5b), &[h(1), h(2)]),
            "parserId is bound"
        );
    }

    /// The contract's purpose, end to end on the cache: an unchanged
    /// content-derived hash holds its boundaries and a changed member
    /// `node_hash` fires them.
    #[test]
    fn content_derived_region_hashes_cascade_exactly_when_content_moves() {
        let parser = h(0x5a);
        let a = region_hash(parser, &[h(1), h(2)]);
        let b = region_hash(parser, &[h(3)]);
        let topology = || {
            let mut cache: SheafCache<StalkDigest, ()> = SheafCache::new();
            cache.set_stalk(0, StalkDigest::Hash(*a.as_bytes()));
            cache.set_stalk(1, StalkDigest::Hash(*b.as_bytes()));
            cache.set_restriction(
                0,
                1,
                RestrictionEdge {
                    weights: vec![1.0],
                    boundary_hash: boundary_hash(a, b),
                    co_change_rate: 0.5,
                    revert_rate: 0.0,
                },
            );
            cache.put(0, ());
            cache.put(1, ());
            cache
        };

        let mut cache = topology();
        cache.set_stalk(
            0,
            StalkDigest::Hash(*region_hash(parser, &[h(2), h(1)]).as_bytes()),
        );
        assert_eq!(cache.on_change(&[0]), vec![0], "unchanged content holds");

        let mut cache = topology();
        cache.set_stalk(
            0,
            StalkDigest::Hash(*region_hash(parser, &[h(1), h(9)]).as_bytes()),
        );
        assert_eq!(cache.on_change(&[0]), vec![0, 1], "a moved member fires");
    }

    fn vector_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../clients/go/leyline-schema/vectors/sheaf-region-v1.json")
    }

    fn vectors() -> serde_json::Value {
        let parser = h(0x5a);
        let other_parser = h(0x5b);
        let cases: [(&str, Hash, Vec<Hash>); 6] = [
            ("two members", parser, vec![h(0x01), h(0x02)]),
            (
                "two members, reversed: same hash",
                parser,
                vec![h(0x02), h(0x01)],
            ),
            (
                "one member's node_hash changed",
                parser,
                vec![h(0x01), h(0x03)],
            ),
            (
                "duplicate node_hash counts twice",
                parser,
                vec![h(0x01), h(0x01)],
            ),
            ("no members", parser, vec![]),
            ("other parserId", other_parser, vec![h(0x01), h(0x02)]),
        ];
        let rendered: Vec<serde_json::Value> = cases
            .iter()
            .map(|(name, parser_id, members)| {
                serde_json::json!({
                    "name": name,
                    "parser_id": hex::encode(parser_id.as_bytes()),
                    "member_node_hashes": members.iter().map(|m| hex::encode(m.as_bytes())).collect::<Vec<_>>(),
                    "preimage": hex::encode(preimage(*parser_id, members)),
                    "region_hash": hex::encode(region_hash(*parser_id, members).as_bytes()),
                })
            })
            .collect();
        let a = region_hash(parser, &[h(0x01), h(0x02)]);
        let b = region_hash(parser, &[h(0x03)]);
        serde_json::json!({
            "_doc": "Region-hash test vectors (bead ley-line-open-1c5d28). regionHash = BLAKE3 derive_key(derive_key_context, preimage); preimage = domain tag 0x03 (RowSet) || canon_version u32 LE (1) || len(scheme) u64 LE || scheme || len(params) u64 LE (32) || parserId || n u64 LE || for each member sorted ascending by node_hash bytes: node_hash || a u64 LE (0) || b u64 LE (0). boundary_hash = regionHash(a) XOR regionHash(b). Generated and pinned by rs/ll-open/sheaf/src/region.rs; regenerate with LEYLINE_REGEN_SHEAF_REGION_VECTORS=1 cargo test -p leyline-sheaf region.",
            "derive_key_context": leyline_core::partition::PARTITION_CONTEXT,
            "scheme": REGION_SCHEME,
            "canon_version": 1,
            "cases": rendered,
            "boundary": {
                "a_region_hash": hex::encode(a.as_bytes()),
                "b_region_hash": hex::encode(b.as_bytes()),
                "boundary_hash": hex::encode(boundary_hash(a, b)),
            },
        })
    }

    /// The published vector file is exactly what this implementation
    /// computes, and each case's preimage re-derives its hash.
    #[test]
    fn the_published_vectors_match() {
        let expected = vectors();
        let rendered = serde_json::to_string_pretty(&expected).unwrap() + "\n";
        if std::env::var_os("LEYLINE_REGEN_SHEAF_REGION_VECTORS").is_some() {
            std::fs::create_dir_all(vector_path().parent().unwrap()).unwrap();
            std::fs::write(vector_path(), &rendered).unwrap();
        }
        // The file lives in the Go module, outside `rs/`. A scratch copy of
        // `rs/` alone (the mutation gate's build) has no `clients/` tree at
        // all; there the file comparison is skipped and the self-checks
        // below still run. In a checkout, a missing file fails.
        let go_module = vector_path()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        if go_module.exists() {
            let published = std::fs::read_to_string(vector_path()).expect("read the vector file");
            // Compared as JSON values: key order depends on whether a
            // workspace build unifies serde_json's `preserve_order` feature.
            let published: serde_json::Value =
                serde_json::from_str(&published).expect("the vector file is JSON");
            assert_eq!(published, expected, "regenerate: see the file's _doc");
        }

        for case in expected["cases"].as_array().unwrap() {
            let preimage = hex::decode(case["preimage"].as_str().unwrap()).unwrap();
            let derived = blake3::derive_key(leyline_core::partition::PARTITION_CONTEXT, &preimage);
            assert_eq!(hex::encode(derived), case["region_hash"], "{case}");
        }
        let cases = expected["cases"].as_array().unwrap();
        assert_eq!(cases[0]["region_hash"], cases[1]["region_hash"]);
        assert_ne!(cases[0]["region_hash"], cases[2]["region_hash"]);
        assert_ne!(cases[0]["region_hash"], cases[5]["region_hash"]);
    }
}
