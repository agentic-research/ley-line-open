//! `execution/v1/test-vectors/connections-grant.json`: the pinned RunGrant
//! carrying connections (ADR-0046 Slice 1 S3, bead `ley-line-open-521deb`).
//!
//! The vector is the oracle a second signer (cloister) checks its bytes
//! against. It was produced with `capnp convert json:canonical`, which shares
//! no code with LLO's Rust; `leyline-runtime`'s authorization tests check that
//! `grant_signing_bytes` reaches the same bytes. This crate has no Cap'n Proto
//! codec, so here it checks what it can without one:
//!
//! - each `signingBytesDigest` is BLAKE3-256 over its `signingBytesHex`;
//! - the grant's `connectionsDocument` is the pinned canonical connections
//!   document, byte for byte, and `connectionsDigest` is its digest;
//! - the grant carries exactly the marker in `credentialBrokerRefs`;
//! - `withoutConnections` is the canonical-run.json grant, untouched;
//! - signing bytes differ from canonical bytes only by the cleared-signature
//!   pointer word (`0xfffffffc`, wire/run-grant.md), never by a null pointer.

use leyline_schema_spec::connections;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read_json(rel: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(root().join(rel)).unwrap()).unwrap()
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0);
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

const CLEARED_SIGNATURE_WORD: [u8; 8] = [0xfc, 0xff, 0xff, 0xff, 0, 0, 0, 0];

#[test]
fn the_connections_grant_vector_holds() {
    let vector = read_json("execution/v1/test-vectors/connections-grant.json");
    let run = read_json("execution/v1/test-vectors/canonical-run.json");
    let document =
        fs::read(root().join("connections/v1/test-vectors/connections-canonical.json")).unwrap();

    for key in ["connections", "withoutConnections"] {
        let entry = &vector[key];
        let signing = unhex(entry["signingBytesHex"].as_str().unwrap());
        let canonical = unhex(entry["canonicalHex"].as_str().unwrap());
        assert_eq!(
            entry["signingBytesDigest"].as_str().unwrap(),
            format!("blake3-256:{}", blake3::hash(&signing).to_hex()),
            "{key}: signingBytesDigest"
        );
        assert_eq!(signing.len() % 8, 0, "{key}: whole words");
        assert!(
            signing.chunks(8).any(|w| w == CLEARED_SIGNATURE_WORD),
            "{key}: signing bytes carry the cleared-signature pointer"
        );
        assert!(
            !canonical.chunks(8).any(|w| w == CLEARED_SIGNATURE_WORD),
            "{key}: the unsigned grant carries no signature pointer"
        );
        assert_ne!(signing, canonical, "{key}");
    }

    let base = &vector["withoutConnections"]["grant"];
    assert_eq!(
        base, &run["grant"],
        "withoutConnections is canonical-run.json's grant"
    );
    for absent in ["connectionsDigest", "connectionsDocument", "signature"] {
        assert!(base.get(absent).is_none(), "{absent} must be absent");
    }

    let grant = &vector["connections"]["grant"];
    assert_eq!(
        grant["credentialBrokerRefs"],
        serde_json::json!(["cloister/connections/v1"]),
        "exactly the marker"
    );
    let carried = grant["connectionsDocument"].as_str().unwrap();
    assert_eq!(
        carried.as_bytes(),
        document.as_slice(),
        "the pinned canonical document"
    );
    let parsed = connections::parse(carried.as_bytes()).unwrap();
    assert_eq!(
        parsed.canonical_bytes(),
        document,
        "carried bytes are canonical"
    );
    assert_eq!(grant["connectionsDigest"]["algorithm"], "blake3-256");
    assert_eq!(
        grant["connectionsDigest"]["value"].as_str().unwrap(),
        blake3::hash(&document).to_hex().as_str()
    );

    // Apart from the three fields, the grant is the base grant.
    let mut stripped = grant.clone();
    for key in ["connectionsDigest", "connectionsDocument"] {
        stripped.as_object_mut().unwrap().remove(key);
    }
    stripped["credentialBrokerRefs"] = base["credentialBrokerRefs"].clone();
    assert_eq!(&stripped, base);
}
