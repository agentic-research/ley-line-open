//! Σ axiom falsification suite — the `leyline-cache` slice (ADR-0047 §5,
//! bead `ley-line-open-10d71a`): the restriction key's known-answer
//! vectors. Its law (a) property is `restriction.rs`'s `key_tests`.

use leyline_cache::restriction::{
    Provenance, ROW_CONTEXT, Restriction, RestrictionWitness, Row, RowKind, restriction_key,
};
use leyline_core::{Hash, Ref};
use serde_json::Value;

fn unhex(v: &Value) -> Vec<u8> {
    let s = v.as_str().expect("hex string");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hash_of(v: &Value) -> Hash {
    Hash::from_bytes(unhex(v).try_into().expect("32 bytes"))
}

fn kind(k: u64) -> RowKind {
    [
        RowKind::Container,
        RowKind::Ref,
        RowKind::Import,
        RowKind::Def,
        RowKind::Api,
        RowKind::Qualifier,
        RowKind::Visibility,
        RowKind::Signature,
        RowKind::SigType,
        RowKind::TypeDef,
    ]
    .into_iter()
    .find(|r| *r as u64 == k)
    .expect("a known row kind")
}

/// (DET) the restriction key and every row's leaf address reproduce the
/// checked-in vectors; each row address is `derive_key(ROW_CONTEXT, n ‖
/// (|fᵢ| ‖ fᵢ)…)` rebuilt here from the documented encoding.
#[test]
fn det_known_answer_vectors() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/sigma/v1/test-vectors/constructions.json");
    let v: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let cases = v["restrictionKey"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let rows: Vec<Row> = case["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let fields: Vec<Vec<u8>> = r["fieldsHex"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(unhex)
                    .collect();
                let row = Row {
                    kind: kind(r["kind"].as_u64().unwrap()),
                    fields: fields.clone(),
                    provenance: Provenance::Query,
                };
                let mut h = blake3::Hasher::new_derive_key(ROW_CONTEXT);
                h.update(&(fields.len() as u64).to_le_bytes());
                for f in &fields {
                    h.update(&(f.len() as u64).to_le_bytes());
                    h.update(f);
                }
                assert_eq!(row.address(), hash_of(&r["address"]), "row address drifted");
                assert_eq!(row.address().as_bytes(), h.finalize().as_bytes());
                row
            })
            .collect();
        let key = restriction_key(
            case["scheme"].as_str().unwrap(),
            hash_of(&case["parserId"]),
            &rows,
        );
        let typed = Ref::<Restriction>::derive(RestrictionWitness {
            scheme: case["scheme"].as_str().unwrap(),
            parser_id: hash_of(&case["parserId"]),
            rows: &rows,
        });
        assert_eq!(*typed.as_hash(), key, "the typed key names the same value");
        assert_eq!(
            key,
            hash_of(&case["address"]),
            "restriction key drifted for {}",
            case["name"]
        );
    }
}
