//! `leyline execution-daemon --trust-key FILE --evidence-store DIR` installs
//! the real evidence verifier, end to end over the control socket (bead
//! `ley-line-open-410921`).
//!
//! The daemon runs the native backend with an empty catalog, so a start that
//! passes authorization fails next, at catalog resolution, with
//! `IdentityPolicyMismatch`. That error is the proof a grant was
//! authenticated: every authentication failure is `Unauthenticated` and
//! carries the typed rejection token, and the two never coexist.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use capnp::message::{Builder, ReaderOptions};
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION, GRANT_SIGNATURE_PAYLOAD_TYPE, canonical_digest,
    derive_run_id, grant_signing_bytes,
};
use leyline_runtime::cas_evidence::{CasEvidenceStore, TrustKey};
use serde_json::{Value, json};

const GRANT_ID: &str = "grant-cli";
const REPLAY_KEY: &str = "replay-cli";

fn signer(seed: u8) -> leyline_envelope::Ed25519RootSigner {
    leyline_envelope::Ed25519RootSigner::from_seed(&[seed; 32])
}

fn set_digest(mut digest: execution_capnp::digest_ref::Builder<'_>, value: &str) {
    digest.set_algorithm("blake3-256");
    digest.set_value(value);
}

fn spec_bytes() -> Vec<u8> {
    let mut message = Builder::new_default();
    let mut spec = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    spec.set_schema_version(EXECUTION_SCHEMA_VERSION);
    let mut executable = spec.reborrow().init_executable();
    executable.set_media_type("application/test-executable");
    set_digest(executable.reborrow().init_digest(), &"c".repeat(64));
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize spec");
    bytes
}

fn spec_hex(spec: &[u8]) -> String {
    canonical_digest(spec)
        .expect("spec digest")
        .strip_prefix("blake3-256:")
        .expect("prefix")
        .to_owned()
}

fn evidence_envelope(signer: &leyline_envelope::Ed25519RootSigner, spec: &[u8]) -> Vec<u8> {
    let run_id = derive_run_id(
        &canonical_digest(spec).expect("spec digest"),
        GRANT_ID,
        REPLAY_KEY,
    );
    let statement = leyline_envelope::Statement::new(
        [
            "issuerEvidence",
            "workloadIdentityEvidence",
            "actorProvenanceEvidence",
        ]
        .into_iter()
        .map(|role| leyline_envelope::Subject::with_digest(role, "blake3", &run_id))
        .collect(),
        "https://rosary.dev/Handoff/v1",
        json!({"dispatchId": "dispatch-cli"}),
    );
    leyline_envelope::Envelope::sign(&statement, signer).to_json_vec()
}

/// A native-backend grant over `spec` naming `evidence_digest` for every
/// role, signed by `signer` or unsigned.
fn grant_bytes(
    spec: &[u8],
    evidence_digest: &str,
    signer: Option<&leyline_envelope::Ed25519RootSigner>,
) -> Vec<u8> {
    let evidence_hex = evidence_digest
        .strip_prefix("blake3-256:")
        .expect("evidence digest domain");
    let mut message = Builder::new_default();
    let mut grant = message.init_root::<execution_capnp::run_grant::Builder<'_>>();
    grant.set_grant_id(GRANT_ID);
    grant.set_expires_at_unix_ms(u64::MAX);
    grant.set_replay_key(REPLAY_KEY);
    set_digest(grant.reborrow().init_run_spec_digest(), &spec_hex(spec));
    let in_toto = |mut evidence: execution_capnp::evidence_ref::Builder<'_>| {
        evidence.set_media_type("application/vnd.in-toto+json");
        set_digest(evidence.init_digest(), evidence_hex);
    };
    in_toto(grant.reborrow().init_issuer_evidence());
    in_toto(grant.reborrow().init_workload_identity_evidence());
    in_toto(grant.reborrow().init_actor_provenance_evidence());
    set_digest(grant.reborrow().init_confinement_digest(), &"b".repeat(64));
    grant.set_backend_class(execution_capnp::BackendClass::Native);
    let mut capability = grant.reborrow().init_capabilities(1).get(0);
    capability.set_grant(EXECUTION_CAPABILITY);
    capability.set_interface(EXECUTION_SCHEMA_VERSION);
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize grant");
    let Some(signer) = signer else {
        return bytes;
    };
    let value = leyline_envelope::sign_payload(
        GRANT_SIGNATURE_PAYLOAD_TYPE,
        &grant_signing_bytes(&bytes).expect("signing bytes"),
        signer,
    );
    let mut signature = message
        .get_root::<execution_capnp::run_grant::Builder<'_>>()
        .expect("grant root")
        .init_signature();
    signature.set_algorithm("ed25519");
    signature.set_key_id(&TrustKey::new(signer.verifying_key()).kid);
    signature.set_value(&value);
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize signed grant");
    bytes
}

fn spec_json(bytes: &[u8]) -> Value {
    let message =
        capnp::serialize::read_message(&mut &bytes[..], ReaderOptions::new()).expect("read spec");
    let root = message
        .get_root::<execution_capnp::run_spec::Reader<'_>>()
        .expect("spec root");
    serde_json::from_str(&capnp_json::to_json(root).expect("spec json")).expect("parse")
}

fn grant_json(bytes: &[u8]) -> Value {
    let message =
        capnp::serialize::read_message(&mut &bytes[..], ReaderOptions::new()).expect("read grant");
    let root = message
        .get_root::<execution_capnp::run_grant::Reader<'_>>()
        .expect("grant root");
    serde_json::from_str(&capnp_json::to_json(root).expect("grant json")).expect("parse")
}

fn call(sock: &Path, request: Value) -> Value {
    let mut stream = UnixStream::connect(sock).expect("connect daemon socket");
    stream
        .write_all(format!("{request}\n").as_bytes())
        .expect("send request");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("end request");
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("read reply");
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("reply {line:?}: {error}"))
}

/// The files an execution daemon needs, in one temporary directory.
struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["cas", "runs", "home", "evidence"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("mkdir");
        }
        // The native backend only needs the worker to exist: every start in
        // this file stops before a worker is spawned.
        std::fs::write(dir.path().join("worker"), "#!/bin/sh\nexit 1\n").expect("worker");
        std::fs::write(dir.path().join("catalog.json"), r#"{"entries":[]}"#).expect("catalog");
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn trust_key(&self, name: &str, signer: &leyline_envelope::Ed25519RootSigner) -> PathBuf {
        let path = self.path(name);
        let hex: String = signer
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        std::fs::write(&path, hex).expect("trust key");
        path
    }

    fn command(&self, verifier_args: &[&std::ffi::OsStr]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_leyline"));
        command
            .arg("execution-daemon")
            .arg("--backend")
            .arg("native")
            .arg("--arena")
            .arg(self.path("daemon.arena"))
            .arg("--control")
            .arg(self.path("daemon.ctrl"))
            .arg("--cas-root")
            .arg(self.path("cas"))
            .arg("--run-root")
            .arg(self.path("runs"))
            .arg("--worker")
            .arg(self.path("worker"))
            .arg("--catalog")
            .arg(self.path("catalog.json"))
            .args(verifier_args)
            .env("HOME", self.path("home"))
            .stdin(Stdio::null());
        command
    }

    /// Run a daemon that must refuse to start, and return its output.
    fn refuses(&self, verifier_args: &[&std::ffi::OsStr]) -> Output {
        let mut child = self
            .command(verifier_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn leyline execution-daemon");
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().expect("poll daemon").is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("the daemon started with {verifier_args:?}; it must refuse");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().expect("daemon output");
        assert!(
            !output.status.success(),
            "a refused configuration exits non-zero: {verifier_args:?}"
        );
        output
    }

    fn daemon(&self, verifier_args: &[&std::ffi::OsStr]) -> Daemon {
        let log = std::fs::File::create(self.path("daemon.log")).expect("log");
        let child = self
            .command(verifier_args)
            .stdout(log.try_clone().expect("log"))
            .stderr(log)
            .spawn()
            .expect("spawn leyline execution-daemon");
        let mut daemon = Daemon {
            child,
            sock: self.path("daemon.sock"),
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if daemon.sock.exists() && UnixStream::connect(&daemon.sock).is_ok() {
                break;
            }
            if let Some(status) = daemon.child.try_wait().expect("poll daemon") {
                panic!(
                    "the daemon exited ({status}); log:\n{}",
                    std::fs::read_to_string(self.path("daemon.log")).unwrap_or_default()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the daemon socket never answered"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let provision = call(
            &daemon.sock,
            json!({
                "op": "llo_execution_provision",
                "backendClass": "native",
                "idempotencyKey": "provision-cli",
            }),
        );
        assert_ne!(provision["ok"], false, "provision: {provision}");
        daemon
    }
}

struct Daemon {
    child: Child,
    sock: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    /// The error a start reports. Every start in this file fails: either at
    /// authentication or, once authenticated, at the empty catalog.
    fn start_error(&self, spec: &[u8], grant: &[u8]) -> String {
        let reply = call(
            &self.sock,
            json!({
                "op": "llo_execution_start",
                "spec": spec_json(spec),
                "grant": grant_json(grant),
            }),
        );
        assert_eq!(reply["ok"], false, "start must fail here: {reply}");
        reply["error"].as_str().expect("error text").to_owned()
    }
}

#[test]
fn trust_key_and_evidence_store_install_the_real_verifier() {
    let host = Host::new();
    let issuer = signer(61);
    let stranger = signer(62);
    let trust_key = host.trust_key("issuer.key", &issuer);
    let evidence_store = host.path("evidence");
    let spec = spec_bytes();
    let digest = CasEvidenceStore::open(&evidence_store)
        .expect("evidence store")
        .insert(&evidence_envelope(&issuer, &spec))
        .expect("insert evidence");

    let daemon = host.daemon(&[
        "--trust-key".as_ref(),
        trust_key.as_os_str(),
        "--evidence-store".as_ref(),
        evidence_store.as_os_str(),
    ]);

    let accepted = daemon.start_error(&spec, &grant_bytes(&spec, &digest, Some(&issuer)));
    assert!(
        accepted.starts_with("IdentityPolicyMismatch:") && accepted.contains("trusted catalog"),
        "a validly signed grant passes authentication and reaches the catalog: {accepted}"
    );

    let missing = format!("blake3-256:{}", "e".repeat(64));
    for (name, grant, token) in [
        (
            "unsigned",
            grant_bytes(&spec, &digest, None),
            "grant-unsigned",
        ),
        (
            "wrong key",
            grant_bytes(&spec, &digest, Some(&stranger)),
            "untrusted-signer",
        ),
        (
            "missing evidence",
            grant_bytes(&spec, &missing, Some(&issuer)),
            "evidence-missing",
        ),
    ] {
        let error = daemon.start_error(&spec, &grant);
        assert!(
            error.starts_with(&format!("Unauthenticated: {token}: ")),
            "{name}: {error}"
        );
    }
}

#[test]
fn without_a_verifier_configuration_every_start_is_refused() {
    let host = Host::new();
    let issuer = signer(63);
    let spec = spec_bytes();
    let digest = CasEvidenceStore::open(&host.path("evidence"))
        .expect("evidence store")
        .insert(&evidence_envelope(&issuer, &spec))
        .expect("insert evidence");
    let daemon = host.daemon(&[]);
    let error = daemon.start_error(&spec, &grant_bytes(&spec, &digest, Some(&issuer)));
    assert!(
        error.starts_with("Unauthenticated:") && error.contains("no trusted verifier"),
        "{error}"
    );
}

#[test]
fn a_partial_or_invalid_verifier_configuration_refuses_startup() {
    let host = Host::new();
    let good = host.trust_key("good.key", &signer(64));
    let bad = host.path("bad.key");
    std::fs::write(&bad, "not a key").expect("bad key");
    let store = host.path("evidence");

    let stderr = |output: Output| String::from_utf8_lossy(&output.stderr).into_owned();

    let output = stderr(host.refuses(&["--trust-key".as_ref(), good.as_os_str()]));
    assert!(output.contains("--evidence-store"), "{output}");

    let output = stderr(host.refuses(&["--evidence-store".as_ref(), store.as_os_str()]));
    assert!(output.contains("--trust-key"), "{output}");

    let output = stderr(host.refuses(&[
        "--trust-key".as_ref(),
        bad.as_os_str(),
        "--evidence-store".as_ref(),
        store.as_os_str(),
    ]));
    assert!(output.contains("bad.key"), "{output}");

    let absent = host.path("absent-store");
    let output = stderr(host.refuses(&[
        "--trust-key".as_ref(),
        good.as_os_str(),
        "--evidence-store".as_ref(),
        absent.as_os_str(),
    ]));
    assert!(output.contains("absent-store"), "{output}");
    assert!(
        !absent.exists(),
        "a refused store directory is never created"
    );

    let output = stderr(host.refuses(&[
        "--allow-unverified-evidence".as_ref(),
        "--trust-key".as_ref(),
        good.as_os_str(),
        "--evidence-store".as_ref(),
        store.as_os_str(),
    ]));
    assert!(output.contains("--allow-unverified-evidence"), "{output}");
}
