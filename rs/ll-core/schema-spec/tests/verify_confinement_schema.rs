//! `confinement/v1` ships a machine-readable shape, and it gates.
//!
//! ## Why (bead `ley-line-open-41297c`)
//!
//! Until now confinement/v1 was README prose plus a pinned vector. A consumer
//! wanting types had to hand-mirror the manifest from English, and a
//! hand-mirror is a thing that silently disagrees — the same defect class as
//! `ley-line-open-d554a0` and `ley-line-open-60f0d3`.
//!
//! ADR-0035 §1 makes the manifest the single declaration that a
//! `nono::CapabilitySet`, the `confinementDigest`, and a backend's declared
//! digest are all projections of. That only holds if the manifest has one
//! definition, and §8 fixes the format as JSON Schema rather than capnp
//! because `confinementDigest` is computed over canonical JSON — the IDL
//! format follows the digest definition (schema-spec `LAYOUT.md`).
//!
//! A schema nothing validates against is prose with punctuation, so these
//! tests do two things: the repo's own pinned vector must satisfy it, and
//! each normative refusal in README §2–§5 must actually be refused.

use jsonschema::Validator;
use serde_json::{Value, json};

fn schema() -> Validator {
    let raw = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("confinement/v1/confinement.schema.json"),
    )
    .expect("confinement/v1 ships confinement.schema.json");
    let value: Value = serde_json::from_str(&raw).expect("schema is valid JSON");
    Validator::new(&value).expect("schema is a valid JSON Schema")
}

fn canonical_manifest() -> Value {
    let raw = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("confinement/v1/test-vectors/manifest-canonical.json"),
    )
    .expect("the pinned canonical manifest");
    serde_json::from_str(&raw).expect("vector is valid JSON")
}

/// The load-bearing one: the manifest whose BLAKE3 digest is pinned in
/// `CONFINEMENT_DIGESTS.blake3`, and which cloister already computes the same
/// digest for, must satisfy the schema. If it did not, the schema would be
/// describing a different contract than the one both implementations ship.
#[test]
fn the_pinned_canonical_manifest_satisfies_the_schema() {
    let validator = schema();
    let manifest = canonical_manifest();
    if let Err(error) = validator.validate(&manifest) {
        panic!("the pinned canonical manifest does not satisfy confinement.schema.json: {error}");
    }
}

/// Each refusal README §2–§5 states in prose, asserted as a refusal.
///
/// A schema that accepts everything the prose forbids has moved the rules
/// without enforcing them.
#[test]
fn the_schema_refuses_what_the_prose_refuses() {
    let validator = schema();
    let base = canonical_manifest();

    let cases: Vec<(&str, Value)> = vec![
        (
            "§2 read-only has no explicit spelling — one grant, one form",
            json!({"version": "cloister/confinement/v1",
                   "fs": {"allow": [{"path": "/srv/data/", "mode": "ro"}]}}),
        ),
        (
            "§2 a relative path is not a canonical prefix",
            json!({"version": "cloister/confinement/v1",
                   "fs": {"allow": ["srv/data/"]}}),
        ),
        (
            "§2 a traversal component is not canonical",
            json!({"version": "cloister/confinement/v1",
                   "fs": {"allow": ["/srv/../etc/"]}}),
        ),
        (
            "§3 a wildcard away from the leading label is rejected",
            json!({"version": "cloister/confinement/v1",
                   "network": {"allowHosts": ["api.*.example.com"]}}),
        ),
        (
            "§4 privileged ports are out of scope in v1",
            json!({"version": "cloister/confinement/v1", "port": {"bind": 80}}),
        ),
        (
            "§4 a port block without a port grants nothing and means nothing",
            json!({"version": "cloister/confinement/v1", "port": {"address": "0.0.0.0"}}),
        ),
        (
            "§5 a scheme nono::keystore will refuse is refused here too",
            json!({"version": "cloister/confinement/v1",
                   "credentialSource": "https://vault.example.com/secret"}),
        ),
        (
            "an unrecognised dimension is a refusal, not a forward-compatible extension",
            json!({"version": "cloister/confinement/v1", "gpu": {"allow": ["all"]}}),
        ),
        (
            "a manifest for another version is not interpreted under these rules",
            json!({"version": "cloister/confinement/v2"}),
        ),
    ];

    for (why, manifest) in cases {
        assert!(
            validator.validate(&manifest).is_err(),
            "schema accepted a manifest the spec refuses — {why}\n{manifest:#}"
        );
    }

    // The base must still pass, so the assertions above are refusals of the
    // specific defect rather than of everything.
    assert!(
        validator.validate(&base).is_ok(),
        "the canonical manifest must remain valid"
    );
}

/// Both `fs.allow` spellings are legal, and the bare string is read-only.
#[test]
fn both_fs_entry_forms_are_accepted() {
    let validator = schema();
    let manifest = json!({
        "version": "cloister/confinement/v1",
        "fs": {"allow": ["/etc/hosts", {"path": "/var/lib/bundle-X/", "mode": "rw"}]}
    });
    assert!(validator.validate(&manifest).is_ok());
}

/// An empty allow-list is a meaningful value, not a missing one: README §1
/// says a runner given `fs.allow: []` MUST refuse every filesystem operation.
/// A schema that rejected it would make "deny everything" unspellable.
#[test]
fn an_empty_allow_list_is_expressible() {
    let validator = schema();
    let manifest = json!({"version": "cloister/confinement/v1", "fs": {"allow": []}});
    assert!(
        validator.validate(&manifest).is_ok(),
        "deny-everything must be expressible"
    );
}
