//! `cloister/connections/v1`: every vector reaches its pinned
//! `connectionsDigest`, every refusal is refused by name, and an independent
//! Python implementation agrees (ADR-0046 Slice 1 S2, bead
//! `ley-line-open-e9d940`).
//!
//! `connectionsDigest` is BLAKE3-256 over the canonical bytes of the
//! connections document. It names those bytes only: it is not
//! `confinementDigest`, a blob hash, or any arena or store root.
//!
//! Three oracles meet here:
//! - the pins: `CONNECTIONS_DIGESTS.blake3` (BLAKE3-256 of each canonical
//!   document file) and the `connectionsDigest` of every case in
//!   `test-vectors/connections-cases.json`;
//! - the Rust parser and canonicalizer, `leyline_schema_spec::connections`;
//! - the Python reference, `ref-impl-py/connections_v1.py`, standard library
//!   only, with its own reader, canonicalizer and BLAKE3. It shares no code
//!   with the Rust side, so agreement is evidence about the spec rather than
//!   about one implementation.

use leyline_schema_spec::connections::{self, Refusal};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("connections/v1")
}

fn cases() -> Value {
    let raw = fs::read_to_string(spec_dir().join("test-vectors/connections-cases.json"))
        .expect("connections/v1 ships test-vectors/connections-cases.json");
    serde_json::from_str(&raw).expect("cases file is JSON")
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// `(digest hex, path relative to connections/v1)` for every pin line.
fn pins() -> Vec<(String, String)> {
    let path = spec_dir().join("CONNECTIONS_DIGESTS.blake3");
    let text = fs::read_to_string(&path).expect("CONNECTIONS_DIGESTS.blake3");
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (hex, rel) = line
            .split_once(char::is_whitespace)
            .unwrap_or_else(|| panic!("malformed pin line {line:?}"));
        let hex = hex.trim();
        assert!(
            hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "malformed BLAKE3 pin {hex:?}"
        );
        out.push((hex.to_ascii_lowercase(), rel.trim().to_owned()));
    }
    out
}

/// The pinned canonical documents: the digest matches the bytes on disk, and
/// each file is a fixpoint of parse-then-canonicalize (it is canonical).
#[test]
fn pinned_documents_reach_their_digest_and_are_canonical() {
    let pins = pins();
    assert!(
        pins.len() >= 2,
        "expected the single and multi documents to be pinned"
    );
    for (hex, rel) in pins {
        let bytes = fs::read(spec_dir().join(&rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert_eq!(
            blake3_hex(&bytes),
            hex,
            "connectionsDigest of {rel} drifted from its pin"
        );
        let doc = connections::parse(&bytes).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert_eq!(
            String::from_utf8(doc.canonical_bytes()).unwrap(),
            String::from_utf8(bytes).unwrap(),
            "{rel} is not canonical"
        );
    }
}

/// The fenced block between `<!-- <marker>:begin -->` and `<!-- <marker>:end -->`.
fn readme_block(readme: &str, marker: &str) -> String {
    let begin = format!("<!-- {marker}:begin -->");
    let end = format!("<!-- {marker}:end -->");
    let start = readme
        .find(&begin)
        .unwrap_or_else(|| panic!("README lacks {begin}"))
        + begin.len();
    let stop = readme[start..]
        .find(&end)
        .unwrap_or_else(|| panic!("README lacks {end}"))
        + start;
    let block = readme[start..stop].trim();
    let block = block
        .strip_prefix("```text\n")
        .and_then(|b| b.strip_suffix("\n```"))
        .unwrap_or_else(|| panic!("{marker} is not one ```text block"));
    block.to_owned()
}

/// The README's literal template, filled from each parsed document, is the
/// pinned canonical file: a second implementation can reach the bytes from the
/// README alone.
#[test]
fn the_readme_template_is_the_canonical_form() {
    let readme = fs::read_to_string(spec_dir().join("README.md")).unwrap();
    let connection = readme_block(&readme, "canonical-connection");
    let document = readme_block(&readme, "canonical-document");
    let cases = cases();
    let mut texts: Vec<String> = pins()
        .into_iter()
        .map(|(_, rel)| fs::read_to_string(spec_dir().join(rel)).unwrap())
        .collect();
    texts.extend(
        cases["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["canonical"].as_str().unwrap().to_owned()),
    );
    for text in texts {
        let doc = connections::parse(text.as_bytes()).unwrap();
        let filled: Vec<String> = doc
            .connections()
            .iter()
            .map(|c| {
                connection
                    .replace("<unix>", &c.endpoint_unix)
                    .replace("<name>", &c.name)
                    .replace("<host>", &c.host_text())
                    .replace("<port>", &c.port.to_string())
            })
            .collect();
        let expected = document.replace("<connections>", &filled.join(",\n"));
        assert_eq!(
            expected, text,
            "the README template disagrees with a pinned canonical document"
        );
    }
}

/// Every accepted case canonicalizes to its pinned bytes and digest, and the
/// canonical bytes are a fixpoint.
#[test]
fn accepted_cases_reach_their_canonical_bytes_and_digest() {
    let cases = cases();
    let list = cases["cases"].as_array().expect("cases[]");
    assert!(!list.is_empty());
    for case in list {
        let name = case["name"].as_str().expect("name");
        let input = case["input"].as_str().expect("input is the document text");
        let canonical = case["canonical"].as_str().expect("canonical");
        let digest = case["connectionsDigest"]
            .as_str()
            .expect("connectionsDigest");
        let doc = connections::parse(input.as_bytes())
            .unwrap_or_else(|e| panic!("case {name} was refused: {e}"));
        let bytes = doc.canonical_bytes();
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            canonical,
            "case {name}"
        );
        assert_eq!(
            format!("blake3-256:{}", blake3_hex(&bytes)),
            digest,
            "case {name}: connectionsDigest"
        );
        let again = connections::parse(&bytes).expect("canonical bytes parse");
        assert_eq!(
            again.canonical_bytes(),
            bytes,
            "case {name}: not a fixpoint"
        );
        assert_eq!(
            again, doc,
            "case {name}: canonical bytes parse to another document"
        );
    }
}

/// A case that names a pinned file canonicalizes to exactly that file: the
/// non-sorted, re-spaced spellings reach the pinned digest.
#[test]
fn spelling_variants_reach_the_pinned_files() {
    let cases = cases();
    let mut checked = 0;
    for case in cases["cases"].as_array().unwrap() {
        let Some(file) = case.get("canonicalFile").and_then(Value::as_str) else {
            continue;
        };
        let pinned = fs::read_to_string(spec_dir().join(file)).unwrap();
        assert_eq!(
            case["canonical"].as_str().unwrap(),
            pinned,
            "{}",
            case["name"]
        );
        checked += 1;
    }
    assert!(checked >= 2, "expected variants of both pinned documents");
}

/// Every refusal is refused, naming the connection, the field and the reason
/// the vector pins.
#[test]
fn rejected_inputs_are_refused_by_name() {
    let cases = cases();
    for case in cases["rejects"].as_array().expect("rejects[]") {
        let name = case["name"].as_str().expect("name");
        let input = case["input"].as_str().expect("input is the document text");
        let expect = &case["refusal"];
        let err = match connections::parse(input.as_bytes()) {
            Ok(_) => panic!("reject {name} was accepted"),
            Err(e) => e,
        };
        assert_eq!(
            err.reason.code(),
            expect["reason"].as_str().unwrap(),
            "reject {name}: {err}"
        );
        assert_eq!(
            err.field,
            expect["field"].as_str().unwrap(),
            "reject {name}: {err}"
        );
        assert_eq!(
            err.connection.as_deref(),
            expect["connection"].as_str(),
            "reject {name}: {err}"
        );
    }
}

/// Every refusal the parser can return has at least one vector, so a code
/// the vectors do not pin cannot be added silently.
#[test]
fn every_refusal_has_a_vector() {
    let cases = cases();
    let pinned: Vec<&str> = cases["rejects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["refusal"]["reason"].as_str().unwrap())
        .collect();
    for refusal in Refusal::ALL {
        assert!(
            pinned.contains(&refusal.code()),
            "no reject vector pins {}",
            refusal.code()
        );
    }
}

/// The independent Python reference agrees on every pinned file, case and
/// refusal. It prints one line per vector: `ok <kind> <name> <result>`, where
/// the result is the digest or the refusal triple; this test requires those
/// to be the values the Rust side computed.
#[test]
fn the_python_reference_agrees() {
    let script = spec_dir().join("ref-impl-py/connections_v1.py");
    let output = Command::new("python3")
        .arg("-I")
        // No bytecode: running a check must not write into the spec tree.
        .arg("-B")
        .arg(&script)
        .arg("conformance")
        .arg(spec_dir())
        .output()
        .expect("python3 runs (the independent reference is part of the gate)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "python reference failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut expected = Vec::new();
    for (hex, rel) in pins() {
        expected.push(format!("ok pin {rel} blake3-256:{hex}"));
    }
    let cases = cases();
    for case in cases["cases"].as_array().unwrap() {
        let doc = connections::parse(case["input"].as_str().unwrap().as_bytes()).unwrap();
        expected.push(format!(
            "ok case {} blake3-256:{}",
            case["name"].as_str().unwrap(),
            blake3_hex(&doc.canonical_bytes())
        ));
    }
    for case in cases["rejects"].as_array().unwrap() {
        let err = connections::parse(case["input"].as_str().unwrap().as_bytes()).unwrap_err();
        expected.push(format!(
            "ok reject {} {}|{}|{}",
            case["name"].as_str().unwrap(),
            err.connection.as_deref().unwrap_or("-"),
            if err.field.is_empty() {
                "-"
            } else {
                &err.field
            },
            err.reason.code()
        ));
    }
    let actual: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        actual, expected,
        "the Python reference disagrees with the Rust parser"
    );
}
