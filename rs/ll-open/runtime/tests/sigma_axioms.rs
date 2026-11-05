//! Σ axiom falsification suite — the `leyline-runtime` slice (ADR-0047 §5,
//! bead `ley-line-open-10d71a`): `run_id`'s half of law (c), construction
//! separation. Its (DET) vector is `execution/v1/test-vectors/run-id.json`,
//! checked by `tests/authorization.rs`'s
//! `run_id_vector_pins_the_derivation_for_other_implementations`.
//!
//! **Known RED.** `run_id` is `"run-" ‖ σ_bytes(domain ‖ 0x00 ‖ fields)`:
//! plain σ over an in-band-prefixed preimage, so the bytes of that preimage,
//! stored as a blob, have the run's address as their `blob_hash`. The strict
//! test is `#[ignore]`d until ADR-0047 M3 (`ley-line-open-73add7`, run-id v2
//! under `derive_key`); the `_measured` test runs in every `task ci` and pins
//! today's aliasing so M3 cannot land without flipping both.

use leyline_core::ContentAddressed;
use leyline_runtime::authorization::derive_run_id;

/// The vector's cases as `(preimage, run_id)`, with `run_id` re-derived by
/// the shipped function so the alias is the shipped construction's.
fn cases() -> Vec<(Vec<u8>, String)> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/execution/v1/test-vectors/run-id.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let cases: Vec<(Vec<u8>, String)> = v["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let run_id = derive_run_id(
                c["canonicalSpecDigest"].as_str().unwrap(),
                c["grantId"].as_str().unwrap(),
                c["replayKey"].as_str().unwrap(),
            );
            (
                hex::decode(c["preimageHex"].as_str().unwrap()).unwrap(),
                run_id,
            )
        })
        .collect();
    assert!(!cases.is_empty());
    cases
}

/// How many vector cases a blob of the case's preimage bytes aliases: its
/// `blob_hash` rendered as a run identity equals the run's.
fn aliases() -> usize {
    cases()
        .into_iter()
        .filter(|(preimage, run_id)| format!("run-{}", preimage.as_slice().hash()) == *run_id)
        .count()
}

/// Law (c) (ADR-0047 §1.2): a blob holding `run_id`'s encoding does not
/// carry the run's address.
#[test]
#[ignore = "RED until ADR-0047 M3 (73add7): run_id is plain σ over a prefixed preimage"]
fn crafted_preimage_does_not_alias_across_constructions() {
    assert_eq!(
        aliases(),
        0,
        "a blob of a run's preimage has the run's address"
    );
}

/// The measured form, run in every `task ci`: every vector case aliases
/// today. M3 fails this pin and must delete it with the strict test's
/// `ignore`.
#[test]
fn crafted_preimage_does_not_alias_across_constructions_measured() {
    let n = aliases();
    eprintln!(
        "crafted_preimage_does_not_alias_across_constructions (run_id): {n} of {} cases alias: RED until 73add7",
        cases().len()
    );
    assert_eq!(
        n,
        cases().len(),
        "every run_id is σ_bytes of its preimage today"
    );
}
