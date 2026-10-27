//! `leyline execution-daemon` verifier selection fails closed (bead
//! `ley-line-open-410921`): a real verifier only from a complete, valid
//! `--trust-key` + `--evidence-store` configuration; the metadata-only
//! downgrade only from `--allow-unverified-evidence` alone; and reject-all
//! when nothing is configured. No partial or invalid configuration ever
//! yields a verifier.
//!
//! Lib tests rather than `tests/`: the diff-scoped mutants gate runs
//! cli-lib's `--lib` tests, so this is where a mutant of `build` is caught.

use std::path::PathBuf;

use super::{EvidenceVerifierConfig, EvidenceVerifierMode};

fn key_hex(seed: u8) -> String {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
        .verifying_key()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Dir(tempfile::TempDir);

impl Dir {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("evidence")).expect("evidence dir");
        Self(dir)
    }

    fn store(&self) -> PathBuf {
        self.0.path().join("evidence")
    }

    fn key(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.path().join(name);
        std::fs::write(&path, contents).expect("write key");
        path
    }
}

fn mode(config: EvidenceVerifierConfig) -> anyhow::Result<EvidenceVerifierMode> {
    config.build().map(|(_, mode)| mode)
}

#[test]
fn no_configuration_rejects_every_start() {
    assert_eq!(
        mode(EvidenceVerifierConfig::default()).expect("default"),
        EvidenceVerifierMode::RejectAll
    );
}

#[test]
fn the_downgrade_is_only_ever_explicit() {
    assert_eq!(
        mode(EvidenceVerifierConfig {
            allow_unverified_evidence: true,
            ..Default::default()
        })
        .expect("explicit downgrade"),
        EvidenceVerifierMode::MetadataOnly
    );
}

#[test]
fn a_complete_configuration_installs_the_cas_dsse_verifier() {
    let dir = Dir::new();
    let first = dir.key("a.key", &key_hex(1));
    let second = dir.key("b.key", &key_hex(2));
    let EvidenceVerifierMode::CasDsse {
        evidence_store,
        trusted_kids,
    } = mode(EvidenceVerifierConfig {
        allow_unverified_evidence: false,
        trust_keys: vec![first, second],
        evidence_store: Some(dir.store()),
    })
    .expect("complete configuration")
    else {
        panic!("a complete configuration must install the CAS DSSE verifier");
    };
    assert_eq!(evidence_store, dir.store());
    assert_eq!(trusted_kids.len(), 2);
    assert!(
        trusted_kids
            .iter()
            .all(|kid| kid.len() == 32 && kid.bytes().all(|b| b.is_ascii_hexdigit())),
        "kids are ADR-012 canonical kids: {trusted_kids:?}"
    );
}

#[test]
fn every_partial_or_invalid_configuration_refuses_startup() {
    let dir = Dir::new();
    let good = dir.key("good.key", &key_hex(3));
    let bad = dir.key("bad.key", "not a key");
    let cases = [
        (
            "downgrade combined with a real verifier",
            EvidenceVerifierConfig {
                allow_unverified_evidence: true,
                trust_keys: vec![good.clone()],
                evidence_store: Some(dir.store()),
            },
        ),
        (
            "downgrade combined with a store alone",
            EvidenceVerifierConfig {
                allow_unverified_evidence: true,
                trust_keys: Vec::new(),
                evidence_store: Some(dir.store()),
            },
        ),
        (
            "trust key without a store",
            EvidenceVerifierConfig {
                trust_keys: vec![good.clone()],
                ..Default::default()
            },
        ),
        (
            "store without a trust key",
            EvidenceVerifierConfig {
                evidence_store: Some(dir.store()),
                ..Default::default()
            },
        ),
        (
            "malformed trust key",
            EvidenceVerifierConfig {
                trust_keys: vec![good.clone(), bad],
                evidence_store: Some(dir.store()),
                ..Default::default()
            },
        ),
        (
            "missing trust key file",
            EvidenceVerifierConfig {
                trust_keys: vec![dir.0.path().join("absent.key")],
                evidence_store: Some(dir.store()),
                ..Default::default()
            },
        ),
        (
            "missing store directory",
            EvidenceVerifierConfig {
                trust_keys: vec![good.clone()],
                evidence_store: Some(dir.0.path().join("absent")),
                ..Default::default()
            },
        ),
        (
            "one key listed twice",
            EvidenceVerifierConfig {
                trust_keys: vec![good.clone(), good],
                evidence_store: Some(dir.store()),
                ..Default::default()
            },
        ),
    ];
    for (name, config) in cases {
        assert!(mode(config).is_err(), "{name} must refuse startup");
    }
}
