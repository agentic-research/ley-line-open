//! notme's ADR-010 canonical-encoding fixtures, consumed from LLO (bead
//! `ley-line-open-0466c0`; fixtures from notme-e9d2b8).
//!
//! notme (TypeScript), signet (Go) and LLO agree on a CABundle canonical
//! encoding (RFC 8949 §4.2 deterministic CBOR, integer-keyed map
//! `1=Epoch 2=Seqno 3=Keys 4=KeyID 5=PrevKeyID 6=IssuedAt`, signature
//! excluded) and on the ADR-012 `kid`. notme owns the fixtures. LLO vendors
//! them byte-for-byte under `tests/fixtures/notme/`, pinned by `PROVENANCE`.
//!
//! What this file asserts, and what it does not:
//!
//! - The vendored bytes are the pinned bytes (`PROVENANCE` SHA-256s), and each
//!   fixture's `.bin` is exactly `hex_decode(.expected.hex)`.
//! - An independent reference encoder, local to this test, turns each `.json`
//!   into the `.bin` bytes. LLO ships no CABundle encoder, so this does not gate
//!   a shipped LLO code path. It makes LLO a third independent implementation
//!   that checks the fixtures themselves are canonical.
//! - The multikey fixture separates sorted from unsorted output: encoding its
//!   keys in source order does not reproduce the fixture.
//! - Every fixture key that is an Ed25519 key (32 bytes) sits under the map key
//!   `canonical_kid(key)` (ADR-012, `leyline_sign::kid`). A key of any other
//!   length must not sit under a kid-shaped map key. The current fixtures carry
//!   only 1- and 2-byte keys, so the kid clause gates no key today. It applies
//!   to any Ed25519 fixture vendored later (notme-32270b).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::VerifyingKey;
use leyline_sign::kid::{canonical_kid, is_canonical_kid_shape};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Fixtures the bead requires. Discovery picks up any further vendored ones.
const REQUIRED_FIXTURES: &[&str] = &["cabundle-basic", "cabundle-multikey"];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/notme")
}

fn read(name: &str) -> Vec<u8> {
    let path = fixture_dir().join(name);
    fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every vendored fixture name (`<name>.json`), sorted.
fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fixture_dir())
        .expect("fixture dir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .into_string()
                .expect("utf-8 name")
        })
        .filter_map(|f| f.strip_suffix(".json").map(str::to_owned))
        .collect();
    names.sort();
    for required in REQUIRED_FIXTURES {
        assert!(
            names.iter().any(|n| n == required),
            "required fixture {required} is not vendored"
        );
    }
    names
}

fn fixture_json(name: &str) -> Map<String, Value> {
    match serde_json::from_slice(&read(&format!("{name}.json"))).expect("fixture json parses") {
        Value::Object(map) => map,
        other => panic!("{name}.json is not an object: {other}"),
    }
}

/// `.expected.hex` is one line of lowercase hex. Decoding it is the only way
/// expected bytes enter this file: there is no inline canonical hex.
fn expected_bytes(name: &str) -> Vec<u8> {
    let raw = String::from_utf8(read(&format!("{name}.expected.hex"))).expect("hex is utf-8");
    let line = raw
        .strip_suffix('\n')
        .unwrap_or_else(|| panic!("{name}.expected.hex must be one newline-terminated line"));
    assert!(
        !line.contains('\n'),
        "{name}.expected.hex has more than one line"
    );
    assert_eq!(
        line,
        line.to_lowercase(),
        "{name}.expected.hex is not lowercase"
    );
    hex::decode(line).unwrap_or_else(|e| panic!("{name}.expected.hex: {e}"))
}

// ---- Reference encoder: RFC 8949 §4.2.1 core deterministic encoding --------

enum Item {
    Uint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Map(Vec<(Item, Item)>),
}

#[derive(Clone, Copy)]
enum MapOrder {
    /// §4.2.1: keys sorted by the bytewise order of their encodings. For a map
    /// whose keys share one major type (every map in a CABundle) this equals
    /// the length-then-bytewise order notme's README states.
    Canonical,
    /// Keys in the order given: the mutation the multikey fixture must catch.
    AsGiven,
}

/// Shortest-form head (§4.2.1: preferred serialization of the argument).
fn head(major: u8, n: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if let Ok(n) = u8::try_from(n) {
        out.extend([m | 24, n]);
    } else if let Ok(n) = u16::try_from(n) {
        out.push(m | 25);
        out.extend(n.to_be_bytes());
    } else if let Ok(n) = u32::try_from(n) {
        out.push(m | 26);
        out.extend(n.to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend(n.to_be_bytes());
    }
}

fn encode(item: &Item, order: MapOrder) -> Vec<u8> {
    let mut out = Vec::new();
    match item {
        Item::Uint(n) => head(0, *n, &mut out),
        Item::Bytes(b) => {
            head(2, b.len() as u64, &mut out);
            out.extend(b);
        }
        Item::Text(s) => {
            head(3, s.len() as u64, &mut out);
            out.extend(s.as_bytes());
        }
        Item::Map(entries) => {
            let mut encoded: Vec<(Vec<u8>, Vec<u8>)> = entries
                .iter()
                .map(|(k, v)| (encode(k, order), encode(v, order)))
                .collect();
            if let MapOrder::Canonical = order {
                encoded.sort_by(|a, b| a.0.cmp(&b.0));
                assert!(
                    encoded.windows(2).all(|w| w[0].0 != w[1].0),
                    "duplicate map key"
                );
            }
            head(5, encoded.len() as u64, &mut out);
            for (k, v) in encoded {
                out.extend(k);
                out.extend(v);
            }
        }
    }
    out
}

fn uint(bundle: &Map<String, Value>, field: &str) -> Result<Item, String> {
    bundle
        .get(field)
        .and_then(Value::as_u64)
        .map(Item::Uint)
        .ok_or_else(|| format!("{field}: missing or not an unsigned integer"))
}

fn text(bundle: &Map<String, Value>, field: &str) -> Result<Item, String> {
    bundle
        .get(field)
        .and_then(Value::as_str)
        .map(|s| Item::Text(s.to_owned()))
        .ok_or_else(|| format!("{field}: missing or not a string"))
}

/// `keys` as (kid, raw key bytes), in the order the JSON parser yields them.
fn keys(bundle: &Map<String, Value>) -> Result<Vec<(String, Vec<u8>)>, String> {
    let Some(Value::Object(keys)) = bundle.get("keys") else {
        return Err("keys: missing or not an object".into());
    };
    keys.iter()
        .map(|(kid, b64)| {
            let b64 = b64
                .as_str()
                .ok_or_else(|| format!("keys.{kid}: not a string"))?;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("keys.{kid}: {e}"))?;
            Ok((kid.clone(), raw))
        })
        .collect()
}

/// The CABundle signing input. `signature` is excluded; any other field the
/// encoding does not name is an error, so a field notme adds later cannot be
/// dropped from the bytes without this test noticing.
fn signing_input(bundle: &Map<String, Value>) -> Result<Item, String> {
    const FIELDS: [&str; 7] = [
        "epoch",
        "seqno",
        "keys",
        "keyId",
        "prevKeyId",
        "issuedAt",
        "signature",
    ];
    if let Some(unknown) = bundle.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(format!("unknown CABundle field {unknown:?}"));
    }
    let keys = keys(bundle)?
        .into_iter()
        .map(|(kid, raw)| (Item::Text(kid), Item::Bytes(raw)))
        .collect();
    Ok(Item::Map(vec![
        (Item::Uint(1), uint(bundle, "epoch")?),
        (Item::Uint(2), uint(bundle, "seqno")?),
        (Item::Uint(3), Item::Map(keys)),
        (Item::Uint(4), text(bundle, "keyId")?),
        (Item::Uint(5), text(bundle, "prevKeyId")?),
        (Item::Uint(6), uint(bundle, "issuedAt")?),
    ]))
}

fn canonical(bundle: &Map<String, Value>) -> Vec<u8> {
    encode(
        &signing_input(bundle).expect("fixture is a CABundle"),
        MapOrder::Canonical,
    )
}

// ---- Vendoring integrity -----------------------------------------------------

/// Every vendored file is listed in `PROVENANCE` and hashes to its pinned
/// SHA-256. A file edited to make an implementation pass fails here.
#[test]
fn vendored_files_match_the_pinned_provenance() {
    let provenance = String::from_utf8(read("PROVENANCE")).expect("PROVENANCE is utf-8");
    let commit = provenance
        .lines()
        .find_map(|l| l.strip_prefix("# source-commit:"))
        .map(str::trim)
        .expect("PROVENANCE names source-commit");
    assert!(
        commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "source-commit is not a full commit hash: {commit:?}"
    );

    let mut pinned = BTreeSet::new();
    for line in provenance
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let mut fields = line.split_whitespace();
        let (Some(sum), Some(file), None) = (fields.next(), fields.next(), fields.next()) else {
            panic!("malformed PROVENANCE line {line:?}");
        };
        assert_eq!(
            hex::encode(Sha256::digest(read(file))),
            sum,
            "{file} differs from the pinned bytes"
        );
        pinned.insert(file.to_owned());
    }

    let on_disk: BTreeSet<String> = fs::read_dir(fixture_dir())
        .expect("fixture dir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .into_string()
                .expect("utf-8 name")
        })
        .filter(|f| f != "PROVENANCE")
        .collect();
    assert_eq!(
        pinned, on_disk,
        "PROVENANCE must list exactly the vendored files"
    );
}

/// notme-1629fe: `.bin` was never checked against `.expected.hex`.
#[test]
fn bin_is_the_decoded_expected_hex() {
    for name in fixture_names() {
        assert_eq!(
            read(&format!("{name}.bin")),
            expected_bytes(&name),
            "{name}"
        );
    }
}

// ---- Canonical encoding ------------------------------------------------------

#[test]
fn reference_encoder_reproduces_every_fixture() {
    for name in fixture_names() {
        assert_eq!(
            hex::encode(canonical(&fixture_json(&name))),
            hex::encode(expected_bytes(&name)),
            "{name}"
        );
    }
}

/// The multikey fixture lists `keys` in non-canonical order. Encoding them in
/// that order must not reproduce the fixture, so a pass of the test above
/// proves §4.2 sorting rather than insertion order.
#[test]
fn multikey_fixture_rejects_unsorted_keys() {
    let bundle = fixture_json("cabundle-multikey");

    let source_order: Vec<String> = keys(&bundle).unwrap().into_iter().map(|(k, _)| k).collect();
    let mut canonical_order = source_order.clone();
    canonical_order.sort_by_key(|k| encode(&Item::Text(k.clone()), MapOrder::Canonical));
    assert_ne!(
        source_order, canonical_order,
        "precondition: the multikey fixture's keys must start out of canonical order"
    );

    let unsorted = encode(&signing_input(&bundle).unwrap(), MapOrder::AsGiven);
    assert_ne!(unsorted, expected_bytes("cabundle-multikey"));
    assert_eq!(canonical(&bundle), expected_bytes("cabundle-multikey"));
}

/// notme's README: tests supply a decoy signature to prove it is excluded.
#[test]
fn signature_is_excluded_from_the_signing_input() {
    for name in fixture_names() {
        let mut bundle = fixture_json(&name);
        bundle.insert("signature".into(), Value::String("ZGVjb3k=".into()));
        assert_eq!(canonical(&bundle), expected_bytes(&name), "{name}");
    }
}

#[test]
fn an_unknown_field_is_refused() {
    let mut bundle = fixture_json("cabundle-basic");
    bundle.insert("extra".into(), Value::from(1));
    let err = signing_input(&bundle)
        .err()
        .expect("unknown field must be refused");
    assert!(err.contains("extra"), "{err}");
}

// ---- ADR-012 kid over the fixtures' keys -------------------------------------

/// A 32-byte fixture key is an Ed25519 key and must be listed under its
/// canonical kid. Any other key must not be listed under a kid-shaped name, so
/// a toy key cannot pose as an ADR-012 kid.
#[test]
fn fixture_keys_are_listed_under_their_adr012_kid() {
    let mut ed25519 = 0;
    for name in fixture_names() {
        for (kid, raw) in keys(&fixture_json(&name)).unwrap() {
            match <[u8; 32]>::try_from(raw.as_slice()) {
                Ok(raw) => {
                    let key = VerifyingKey::from_bytes(&raw)
                        .unwrap_or_else(|e| panic!("{name}: keys.{kid} is not Ed25519: {e}"));
                    assert_eq!(kid, canonical_kid(&key), "{name}: kid is not ADR-012");
                    ed25519 += 1;
                }
                Err(_) => assert!(
                    !is_canonical_kid_shape(kid.as_bytes()),
                    "{name}: {}-byte key listed under a kid-shaped name {kid}",
                    raw.len()
                ),
            }
        }
    }
    eprintln!("ADR-012 kid checked over {ed25519} Ed25519 fixture key(s)");
}
