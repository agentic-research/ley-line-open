//! Registry source → the same authenticated CAS rootfs as a layout import
//! (bead `ley-line-open-97b636`).
//!
//! `pull_oci_image` fetches a digest-pinned image's host-platform manifest,
//! config and layers into a staged OCI image layout and imports that layout
//! with `import_oci_layout`, so a pull and a layout import of one digest give
//! one rootfs digest and one report. These tests run it against
//! `tests/oci_registry/mod.rs`, an in-process registry on a loopback port
//! that speaks the anonymous bearer-token flow and redirects blobs to a
//! "CDN", the way ghcr.io and Docker Hub do. The one network test is opt-in.

use std::fs;
use std::path::Path;
use std::time::Duration;

use leyline_runtime::artifacts::oci::{
    DEFAULT_MAX_ROOTFS_BYTES, ImportedRootfs, OciImport, OciImportError, Platform, SymlinkPolicy,
    import_oci_layout,
};
use leyline_runtime::artifacts::registry::{RegistryLimits, RegistryPull, pull_oci_image};
use serde_json::Value;
use tempfile::TempDir;

mod oci_fixture;
mod oci_registry;
use oci_fixture::{Fixture, build_fixture, checked_in_fixture, sha256};
use oci_registry::{Behavior, Slow, TOKEN, TestRegistry};

const REPOSITORY: &str = "fixture/multilayer";

fn fixture() -> Fixture {
    let scratch = TempDir::new().expect("scratch");
    build_fixture(&scratch.path().join("oci"), false)
}

fn pull<'a>(reference: &'a str, insecure: Option<&'a str>) -> RegistryPull<'a> {
    RegistryPull {
        reference,
        platform: Platform::linux("arm64"),
        symlinks: SymlinkPolicy::Resolve,
        max_rootfs_bytes: DEFAULT_MAX_ROOTFS_BYTES,
        limits: RegistryLimits::default(),
        insecure_plain_http: insecure,
    }
}

/// The checked-in fixture served as `REPOSITORY`, misbehaving per `behavior`.
fn registry(behavior: Behavior) -> TestRegistry {
    TestRegistry::serve(&checked_in_fixture(), REPOSITORY, behavior)
}

fn cas_entries(cas: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(cas)
        .expect("read CAS root")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The arm64 manifest's config and layer digests, from the fixture.
fn arm64_blobs(fixture: &Fixture) -> Vec<String> {
    let manifest: Value = serde_json::from_slice(
        &fs::read(
            checked_in_fixture()
                .join("blobs/sha256")
                .join(fixture.arm64.hex()),
        )
        .unwrap(),
    )
    .unwrap();
    let mut blobs = vec![manifest["config"]["digest"].as_str().unwrap().to_string()];
    for layer in manifest["layers"].as_array().unwrap() {
        blobs.push(layer["digest"].as_str().unwrap().to_string());
    }
    blobs
}

fn blob_size(digest: &str) -> u64 {
    fs::metadata(
        checked_in_fixture()
            .join("blobs/sha256")
            .join(digest.strip_prefix("sha256:").unwrap()),
    )
    .unwrap()
    .len()
}

fn pull_into(request: &RegistryPull<'_>) -> (TempDir, Result<ImportedRootfs, OciImportError>) {
    let cas = TempDir::new().expect("cas");
    let result = pull_oci_image(request, cas.path());
    (cas, result)
}

fn assert_no_token(error: &OciImportError) {
    let shown = format!("{error} {error:?}");
    assert!(!shown.contains(TOKEN), "the bearer token leaked: {shown}");
}

#[test]
fn a_registry_pull_gives_the_layout_imports_rootfs_and_report() {
    let fixture = fixture();
    let registry = registry(Behavior::default());
    let reference = registry.reference(&fixture.index.digest);
    let (pulled_cas, pulled) = pull_into(&pull(&reference, Some(&registry.authority)));
    let pulled = pulled.expect("pull the fixture from the test registry");

    let layout_cas = TempDir::new().expect("cas");
    let imported = import_oci_layout(
        &OciImport {
            layout: &checked_in_fixture(),
            reference: &fixture.index.digest,
            platform: Platform::linux("arm64"),
            symlinks: SymlinkPolicy::Resolve,
            max_rootfs_bytes: DEFAULT_MAX_ROOTFS_BYTES,
        },
        layout_cas.path(),
    )
    .expect("import the fixture layout");

    // One identity, one report, byte for byte.
    assert_eq!(pulled, imported);
    assert_eq!(
        serde_json::to_string(&pulled).unwrap(),
        serde_json::to_string(&imported).unwrap()
    );
    assert_eq!(
        fs::read(
            pulled_cas
                .path()
                .join(&pulled.rootfs.value)
                .join("rootfs.manifest.json")
        )
        .unwrap(),
        fs::read(
            layout_cas
                .path()
                .join(&imported.rootfs.value)
                .join("rootfs.manifest.json")
        )
        .unwrap()
    );
    // The staging is gone: the CAS root holds the rootfs and nothing else.
    assert_eq!(
        cas_entries(pulled_cas.path()),
        vec![pulled.rootfs.value.clone()]
    );

    // Only the pinned index, the arm64 manifest, its config and its layers
    // were fetched, each once; the amd64 image was never touched.
    let mut fetched: Vec<String> = registry
        .paths()
        .into_iter()
        .filter(|path| !path.starts_with("/token"))
        .filter_map(|path| {
            let digest = path.rsplit('/').next().unwrap().to_string();
            (path.starts_with("/v2/")).then_some(digest)
        })
        .collect();
    // Each /v2/ request is made once without a token (challenged) at most.
    fetched.dedup();
    let mut expected = vec![fixture.index.digest.clone(), fixture.arm64.digest.clone()];
    expected.extend(arm64_blobs(&fixture));
    assert_eq!(fetched, expected);
    let paths = registry.paths();
    assert!(
        !paths.iter().any(|path| path.contains(fixture.amd64.hex())),
        "{paths:?}"
    );
    // The documents are fetched as manifests, with every type the importer
    // reads named in Accept; the config and layers as blobs.
    let seen = registry.seen();
    let manifest_requests: Vec<_> = seen
        .iter()
        .filter(|seen| seen.path.contains("/manifests/"))
        .collect();
    assert!(!manifest_requests.is_empty());
    for request in manifest_requests {
        let accept = request.accept.as_deref().unwrap_or_default();
        for media in [
            "application/vnd.oci.image.index.v1+json",
            "application/vnd.oci.image.manifest.v1+json",
            "application/vnd.docker.distribution.manifest.list.v2+json",
            "application/vnd.docker.distribution.manifest.v2+json",
        ] {
            assert!(accept.contains(media), "{accept}");
        }
    }
    for blob in arm64_blobs(&fixture) {
        assert!(
            paths.contains(&format!("/v2/{REPOSITORY}/blobs/{blob}")),
            "{paths:?}"
        );
    }
}

#[test]
fn the_anonymous_token_goes_to_the_registry_only() {
    let fixture = fixture();
    let registry = registry(Behavior::default());
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, pulled) = pull_into(&pull(&reference, Some(&registry.authority)));
    pulled.expect("pull");
    let seen = registry.seen();
    // One anonymous token, for this repository's pull scope.
    let tokens: Vec<_> = seen
        .iter()
        .filter(|seen| seen.path.starts_with("/token?"))
        .collect();
    assert_eq!(tokens.len(), 1, "{seen:?}");
    assert!(
        tokens[0].path.contains("service=test-registry"),
        "{}",
        tokens[0].path
    );
    assert!(
        tokens[0]
            .path
            .contains("scope=repository%3Afixture%2Fmultilayer%3Apull"),
        "{}",
        tokens[0].path
    );
    assert_eq!(tokens[0].authorization, None);
    // Every authenticated /v2/ request carries it; no CDN request does.
    let authorized: Vec<_> = seen
        .iter()
        .filter(|seen| seen.authorization.is_some())
        .collect();
    assert!(!authorized.is_empty());
    for seen in &authorized {
        assert!(seen.path.starts_with("/v2/"), "{seen:?}");
        assert_eq!(
            seen.authorization.as_deref(),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }
    let cdn: Vec<_> = seen
        .iter()
        .filter(|seen| seen.path.starts_with("/cdn/"))
        .collect();
    assert_eq!(cdn.len(), 4, "a config and three layers: {seen:?}");
    assert!(cdn.iter().all(|seen| seen.authorization.is_none()));
}

#[test]
fn a_registry_that_needs_no_token_is_pulled_without_one() {
    let fixture = fixture();
    let registry = registry(Behavior {
        challenge: None,
        blob_redirects: 0,
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, pulled) = pull_into(&pull(&reference, Some(&registry.authority)));
    pulled.expect("pull");
    let seen = registry.seen();
    assert_eq!(
        seen.len(),
        6,
        "index, manifest, config, three layers: {seen:?}"
    );
    assert!(seen.iter().all(|seen| seen.authorization.is_none()));
}

#[test]
fn a_tag_or_an_unpinned_reference_is_refused_before_any_request() {
    let fixture = fixture();
    let registry = registry(Behavior::default());
    let authority = registry.authority.clone();
    for reference in [
        format!("{authority}/{REPOSITORY}:latest"),
        format!("{authority}/{REPOSITORY}"),
        format!("{authority}/{REPOSITORY}:latest@{}", fixture.index.digest),
        "alpine:3.20".to_string(),
        "alpine".to_string(),
    ] {
        let (cas, result) = pull_into(&pull(&reference, Some(&authority)));
        let error = result.expect_err(&reference);
        assert!(
            matches!(error, OciImportError::MutableReference(_)),
            "{reference}: {error}"
        );
        assert!(cas_entries(cas.path()).is_empty());
    }
    // A bare digest names no repository to pull it from.
    let (_cas, result) = pull_into(&pull(&fixture.index.digest, Some(&authority)));
    assert!(
        matches!(result, Err(OciImportError::MalformedReference(_))),
        "{result:?}"
    );
    assert!(registry.seen().is_empty(), "{:?}", registry.seen());
    assert_eq!(registry.unparsed(), 0);
}

#[test]
fn a_tampered_manifest_is_refused_before_it_is_parsed() {
    let fixture = fixture();
    // A well-formed index that lists a manifest the server would log if the
    // pull followed it: the pull must refuse the bytes on their digest alone.
    let decoy = format!("sha256:{}", "d".repeat(64));
    let lie = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": decoy,
            "size": 10,
            "platform": {"os": "linux", "architecture": "arm64"}
        }]
    }))
    .unwrap();
    let registry = registry(Behavior {
        tamper: [(fixture.index.digest.clone(), lie)].into(),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    let error = result.expect_err("a tampered index");
    assert!(
        matches!(&error, OciImportError::DigestMismatch { digest, .. } if *digest == fixture.index.digest),
        "{error}"
    );
    assert_no_token(&error);
    assert!(
        !registry.paths().iter().any(|path| path.contains(&decoy)),
        "{:?}",
        registry.paths()
    );
    assert!(cas_entries(cas.path()).is_empty());

    // The same for the platform manifest the index selects.
    let registry = self::registry(Behavior {
        tamper: [(
            fixture.arm64.digest.clone(),
            b"{\"schemaVersion\":2}".to_vec(),
        )]
        .into(),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    let error = result.expect_err("a tampered manifest");
    assert!(
        matches!(&error, OciImportError::DigestMismatch { digest, .. } if *digest == fixture.arm64.digest),
        "{error}"
    );
    assert!(
        !registry.paths().iter().any(|path| path.contains("/blobs/")),
        "{:?}",
        registry.paths()
    );
    assert!(cas_entries(cas.path()).is_empty());
}

#[test]
fn a_tampered_blob_is_refused() {
    let fixture = fixture();
    let blobs = arm64_blobs(&fixture);
    for digest in [&blobs[0], &blobs[2]] {
        let size = blob_size(digest) as usize;
        let registry = registry(Behavior {
            tamper: [(digest.clone(), vec![b'x'; size])].into(),
            ..Behavior::default()
        });
        let reference = registry.reference(&fixture.index.digest);
        let (cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
        let error = result.expect_err("a tampered blob");
        match &error {
            OciImportError::DigestMismatch {
                digest: refused,
                reason,
            } => {
                assert_eq!(refused, digest);
                assert!(reason.contains("does not hash"), "{reason}");
            }
            other => panic!("{other}"),
        }
        assert_no_token(&error);
        assert!(cas_entries(cas.path()).is_empty());
    }
}

#[test]
fn a_blob_whose_size_is_not_its_descriptors_is_refused() {
    let fixture = fixture();
    let layer = arm64_blobs(&fixture)[1].clone();
    let original = fs::read(
        checked_in_fixture()
            .join("blobs/sha256")
            .join(layer.strip_prefix("sha256:").unwrap()),
    )
    .unwrap();
    let mut longer = original.clone();
    longer.extend_from_slice(b"trailing");
    let shorter = original[..original.len() - 1].to_vec();
    for served in [longer, shorter] {
        let registry = registry(Behavior {
            tamper: [(layer.clone(), served)].into(),
            ..Behavior::default()
        });
        let reference = registry.reference(&fixture.index.digest);
        let (cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
        match result.expect_err("a resized blob") {
            OciImportError::DigestMismatch { digest, reason } => {
                assert_eq!(digest, layer);
                assert!(reason.contains("size"), "{reason}");
            }
            other => panic!("{other}"),
        }
        assert!(cas_entries(cas.path()).is_empty());
    }
}

#[test]
fn plain_http_is_refused_without_the_named_opt_in() {
    let fixture = fixture();
    let registry = registry(Behavior::default());
    let reference = registry.reference(&fixture.index.digest);

    // No opt-in: the pull speaks TLS, so the plaintext registry never parses
    // a request, and the pull fails.
    let (cas, result) = pull_into(&pull(&reference, None));
    let error = result.expect_err("TLS to a plaintext registry");
    assert!(matches!(error, OciImportError::Registry(_)), "{error}");
    assert!(registry.seen().is_empty(), "{:?}", registry.seen());
    assert!(cas_entries(cas.path()).is_empty());

    // An opt-in for another port is no opt-in for this one.
    let other = "127.0.0.1:1";
    let (_cas, result) = pull_into(&pull(&reference, Some(other)));
    assert!(result.is_err());
    assert!(registry.seen().is_empty(), "{:?}", registry.seen());

    // The opt-in admits loopback addresses only: it is for a local test
    // registry, never a reachable one.
    for host in ["example.com:5000", "10.0.0.1:5000", "localhost:5000"] {
        let reference = format!("{host}/{REPOSITORY}@{}", fixture.index.digest);
        let (_cas, result) = pull_into(&pull(&reference, Some(host)));
        assert!(
            matches!(result, Err(OciImportError::InsecureTransport(_))),
            "{host}: {result:?}"
        );
    }
}

#[test]
fn a_redirect_to_plain_http_elsewhere_is_refused() {
    let fixture = fixture();
    let elsewhere = registry(Behavior::default());
    let registry = registry(Behavior {
        redirect_blobs_to: Some(format!("http://{}/cdn/1/", elsewhere.authority)),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    let error = result.expect_err("a plaintext redirect off the opted-in registry");
    assert!(
        matches!(error, OciImportError::InsecureTransport(_)),
        "{error}"
    );
    assert!(elsewhere.seen().is_empty(), "{:?}", elsewhere.seen());
    assert!(cas_entries(cas.path()).is_empty());

    // So is a redirect to a scheme that is neither.
    let registry = self::registry(Behavior {
        redirect_blobs_to: Some("ftp://127.0.0.1/".into()),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    assert!(
        matches!(result, Err(OciImportError::InsecureTransport(_))),
        "{result:?}"
    );
}

#[test]
fn redirects_are_followed_up_to_the_limit_and_no_further() {
    let fixture = fixture();
    let limits = |max_redirects| RegistryLimits {
        max_redirects,
        ..RegistryLimits::default()
    };
    let registry = registry(Behavior {
        blob_redirects: 3,
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);

    let request = RegistryPull {
        limits: limits(3),
        ..pull(&reference, Some(&registry.authority))
    };
    let (_cas, result) = pull_into(&request);
    result.expect("three redirects within a limit of three");

    let request = RegistryPull {
        limits: limits(2),
        ..pull(&reference, Some(&registry.authority))
    };
    let (cas, result) = pull_into(&request);
    let error = result.expect_err("three redirects over a limit of two");
    assert!(
        matches!(error, OciImportError::TooManyRedirects { limit: 2 }),
        "{error}"
    );
    assert!(cas_entries(cas.path()).is_empty());
}

#[test]
fn the_total_download_is_bounded_before_any_layer_is_fetched() {
    let fixture = fixture();
    // What a pull downloads: the index, the manifest, the config and the
    // layers, each exactly its descriptor's size.
    let total: u64 = [fixture.index.digest.clone(), fixture.arm64.digest.clone()]
        .into_iter()
        .chain(arm64_blobs(&fixture))
        .map(|digest| blob_size(&digest))
        .sum();
    let limits = |max_download_bytes| RegistryLimits {
        max_download_bytes,
        ..RegistryLimits::default()
    };

    let registry = registry(Behavior::default());
    let reference = registry.reference(&fixture.index.digest);
    let request = RegistryPull {
        limits: limits(total),
        ..pull(&reference, Some(&registry.authority))
    };
    let (_cas, result) = pull_into(&request);
    result.expect("a pull exactly at its download limit");

    let registry = self::registry(Behavior::default());
    let reference = registry.reference(&fixture.index.digest);
    let request = RegistryPull {
        limits: limits(total - 1),
        ..pull(&reference, Some(&registry.authority))
    };
    let (cas, result) = pull_into(&request);
    let error = result.expect_err("a pull one byte over its download limit");
    assert!(
        matches!(error, OciImportError::DownloadTooLarge { limit } if limit == total - 1),
        "{error}"
    );
    // The descriptors' sizes are summed before any layer is requested.
    assert!(
        !registry.paths().iter().any(|path| path.contains("/blobs/")),
        "{:?}",
        registry.paths()
    );
    assert!(cas_entries(cas.path()).is_empty());

    // A limit smaller than the pinned document itself stops at it.
    let registry = self::registry(Behavior::default());
    let reference = registry.reference(&fixture.index.digest);
    let request = RegistryPull {
        limits: limits(10),
        ..pull(&reference, Some(&registry.authority))
    };
    let (_cas, result) = pull_into(&request);
    assert!(
        matches!(result, Err(OciImportError::DownloadTooLarge { limit: 10 })),
        "{result:?}"
    );
    assert!(
        !registry
            .paths()
            .iter()
            .any(|path| path.contains(fixture.arm64.hex())),
        "{:?}",
        registry.paths()
    );
}

#[test]
fn a_stalled_body_times_out() {
    let fixture = fixture();
    for slow in [Slow::Headers, Slow::Body] {
        let registry = registry(Behavior {
            slow_blobs: slow,
            ..Behavior::default()
        });
        let reference = registry.reference(&fixture.index.digest);
        let request = RegistryPull {
            limits: RegistryLimits {
                response_timeout: Duration::from_millis(300),
                min_bytes_per_second: u64::MAX,
                ..RegistryLimits::default()
            },
            ..pull(&reference, Some(&registry.authority))
        };
        let started = std::time::Instant::now();
        let (cas, result) = pull_into(&request);
        let error = result.expect_err("a stalled blob");
        assert!(
            matches!(error, OciImportError::Registry(_)),
            "{slow:?}: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{slow:?}: {:?}",
            started.elapsed()
        );
        assert!(cas_entries(cas.path()).is_empty());
    }
}

/// A body that keeps arriving, never idle for `response_timeout`, still has
/// a deadline: `response_timeout` plus its size at the minimum rate.
#[test]
fn a_slow_drip_body_meets_its_deadline() {
    let fixture = fixture();
    let config = arm64_blobs(&fixture)[0].clone();
    let registry = registry(Behavior {
        // Never idle for the response timeout, so only the deadline stops it
        // (ureq alone would wait a second per byte once a deadline passed).
        slow_blobs: Slow::Drip(Duration::from_millis(900)),
        slow_only: Some(config.clone()),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let request = RegistryPull {
        limits: RegistryLimits {
            response_timeout: Duration::from_secs(2),
            min_bytes_per_second: u64::MAX,
            ..RegistryLimits::default()
        },
        ..pull(&reference, Some(&registry.authority))
    };
    let started = std::time::Instant::now();
    let (cas, result) = pull_into(&request);
    let error = result.expect_err("a body slower than its deadline");
    assert!(
        matches!(&error, OciImportError::Registry(detail) if detail.contains("deadline")),
        "{error}"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(6),
        "{elapsed:?}"
    );
    assert!(cas_entries(cas.path()).is_empty());
}

/// A body may take longer than `response_timeout` to arrive, as a large
/// layer does, when it arrives within its size's deadline.
#[test]
fn a_body_slower_than_the_response_timeout_but_within_its_rate_arrives() {
    let fixture = fixture();
    let config = arm64_blobs(&fixture)[0].clone();
    let registry = registry(Behavior {
        slow_blobs: Slow::Drip(Duration::from_millis(2)),
        slow_only: Some(config.clone()),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let request = RegistryPull {
        limits: RegistryLimits {
            response_timeout: Duration::from_millis(300),
            min_bytes_per_second: 100,
            ..RegistryLimits::default()
        },
        ..pull(&reference, Some(&registry.authority))
    };
    let started = std::time::Instant::now();
    let (_cas, result) = pull_into(&request);
    result.expect("a slow body within its deadline");
    assert!(
        started.elapsed() > Duration::from_millis(300),
        "the drip must outlast the response timeout: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_credentials_challenge_is_refused() {
    let fixture = fixture();
    for challenge in [
        "Basic realm=\"registry\"",
        "Bearer service=\"no-realm\"",
        "Bearer realm=\"ftp://127.0.0.1/token\"",
    ] {
        let registry = registry(Behavior {
            challenge: Some(challenge.into()),
            ..Behavior::default()
        });
        let reference = registry.reference(&fixture.index.digest);
        let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
        let error = result.expect_err(challenge);
        assert!(
            matches!(
                error,
                OciImportError::Registry(_) | OciImportError::InsecureTransport(_)
            ),
            "{challenge}: {error}"
        );
        assert!(
            !registry
                .paths()
                .iter()
                .any(|path| path.starts_with("/token")),
            "{:?}",
            registry.paths()
        );
    }
}

/// The non-public HTTPS destinations a registry may name as its token realm
/// or as a redirect (bead `ley-line-open-b1c18d`, R4), each paired with the
/// address the refusal must name. `port` is a loopback listener that must
/// never see a connection.
fn non_public_destinations(port: u16) -> Vec<(String, &'static str)> {
    vec![
        (format!("https://127.0.0.1:{port}/"), "127.0.0.1"),
        (format!("https://[::1]:{port}/"), "::1"),
        // A name, not a literal: refused on what it resolves to (either
        // loopback address, whichever the resolver lists first).
        (format!("https://localhost:{port}/"), LOOPBACK),
        ("https://10.0.0.1/".to_string(), "10.0.0.1"),
        ("https://172.16.5.4/".to_string(), "172.16.5.4"),
        ("https://192.168.1.1/".to_string(), "192.168.1.1"),
        ("https://169.254.169.254/".to_string(), "169.254.169.254"),
        ("https://100.64.0.1/".to_string(), "100.64.0.1"),
        ("https://0.0.0.0/".to_string(), "0.0.0.0"),
        ("https://[fd00::1]/".to_string(), "fd00::1"),
        ("https://[fe80::1]/".to_string(), "fe80::1"),
        ("https://[::ffff:10.0.0.1]/".to_string(), "10.0.0.1"),
    ]
}

/// A listener no destination in `non_public_destinations` may reach.
fn untouched_listener() -> std::net::TcpListener {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    listener
}

fn assert_never_connected(listener: &std::net::TcpListener, what: &str) {
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("{what}: the non-public destination was connected to: {other:?}"),
    }
}

/// The expected address of a name that resolves to loopback.
const LOOPBACK: &str = "loopback";

fn assert_non_public(error: &OciImportError, expected: &str, what: &str) {
    match error {
        OciImportError::NonPublicDestination { address, url } => {
            let matched = if expected == LOOPBACK {
                address == "127.0.0.1" || address == "::1"
            } else {
                address == expected || address.ends_with(expected)
            };
            assert!(matched, "{what}: named {address}, expected {expected}");
            assert!(
                error.to_string().contains(address.as_str()) && !url.contains('?'),
                "{error}"
            );
        }
        other => panic!("{what}: expected a non-public destination refusal, got {other}"),
    }
}

/// A redirect to a loopback, private, link-local or CGNAT address is refused
/// before any connection, whatever the scheme the registry chose (HTTPS here,
/// so the plain-HTTP rule does not apply), and quickly: no connect timeout is
/// waited out.
#[test]
fn a_redirect_to_a_non_public_address_is_refused_without_a_connection() {
    let fixture = fixture();
    let listener = untouched_listener();
    let port = listener.local_addr().unwrap().port();
    for (destination, address) in non_public_destinations(port) {
        let registry = registry(Behavior {
            redirect_blobs_to: Some(destination.clone()),
            ..Behavior::default()
        });
        let reference = registry.reference(&fixture.index.digest);
        let started = std::time::Instant::now();
        let (cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
        let error = result.expect_err(&destination);
        assert_non_public(&error, address, &destination);
        assert_no_token(&error);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{destination}: {:?}",
            started.elapsed()
        );
        assert!(cas_entries(cas.path()).is_empty());
        assert_never_connected(&listener, &destination);
    }
}

/// A token realm naming a non-public address is refused before the token
/// request, so the registry cannot use the anonymous-token flow to make the
/// host GET an internal HTTPS service.
#[test]
fn a_token_realm_at_a_non_public_address_is_refused_without_a_connection() {
    let fixture = fixture();
    let listener = untouched_listener();
    let port = listener.local_addr().unwrap().port();
    for (destination, address) in non_public_destinations(port) {
        let registry = registry(Behavior {
            challenge: Some(format!(
                "Bearer realm=\"{destination}token\",service=\"internal\""
            )),
            ..Behavior::default()
        });
        let reference = registry.reference(&fixture.index.digest);
        let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
        let error = result.expect_err(&destination);
        assert_non_public(&error, address, &destination);
        assert_never_connected(&listener, &destination);
    }
}

/// The insecure opt-in admits the one loopback authority it names, as a
/// realm and as a redirect target, and no other loopback port.
#[test]
fn the_opt_in_admits_its_own_authority_and_no_other_loopback_port() {
    let fixture = fixture();
    // The default behavior's realm and CDN are the opted-in authority.
    let registry = registry(Behavior::default());
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    result.expect("realm and redirects at the opted-in authority");

    // Another loopback port is not the opted-in one, over HTTPS either.
    let listener = untouched_listener();
    let port = listener.local_addr().unwrap().port();
    let registry = self::registry(Behavior {
        redirect_blobs_to: Some(format!("https://127.0.0.1:{port}/cdn/")),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    assert_non_public(
        &result.expect_err("another loopback port"),
        "127.0.0.1",
        "another loopback port",
    );
    assert_never_connected(&listener, "another loopback port");
}

/// A server that sends its response headers one byte at a time, each byte
/// within any per-read timeout, is bounded by the header deadline
/// (`connect_timeout + response_timeout`), not by the 64 KiB header cap at
/// up to a second per byte (bead `ley-line-open-b1c18d`). Whether ureq alone
/// is caught depends on a byte landing as its deadline passes, so the
/// trickle is tried several times; the client's own deadline bounds every
/// attempt. `registry.rs`'s unit test pins the same bound deterministically.
#[test]
fn a_header_trickle_is_bounded_by_the_header_deadline() {
    let fixture = fixture();
    for attempt in 0..5 {
        let registry = registry(Behavior {
            slow_blobs: Slow::HeaderDrip(Duration::from_millis(500)),
            ..Behavior::default()
        });
        let reference = registry.reference(&fixture.index.digest);
        let request = RegistryPull {
            limits: RegistryLimits {
                connect_timeout: Duration::from_millis(500),
                response_timeout: Duration::from_millis(500),
                min_bytes_per_second: u64::MAX,
                ..RegistryLimits::default()
            },
            ..pull(&reference, Some(&registry.authority))
        };
        let started = std::time::Instant::now();
        let (cas, result) = pull_into(&request);
        let error = result.expect_err("a header trickle");
        let elapsed = started.elapsed();
        assert!(
            matches!(&error, OciImportError::Registry(_)),
            "attempt {attempt}: {error}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "attempt {attempt}: {elapsed:?}: {error}"
        );
        assert!(cas_entries(cas.path()).is_empty());
    }
}

#[test]
fn a_registry_error_status_is_reported() {
    let fixture = fixture();
    let registry = registry(Behavior::default());
    // A digest the registry does not hold.
    let missing = format!("sha256:{}", "e".repeat(64));
    let reference = registry.reference(&missing);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    let error = result.expect_err("a missing manifest");
    assert!(
        matches!(&error, OciImportError::Registry(detail) if detail.contains("404")),
        "{error}"
    );
    let _ = fixture;
}

fn fixture_json(digest: &str) -> Value {
    serde_json::from_slice(
        &fs::read(
            checked_in_fixture()
                .join("blobs/sha256")
                .join(digest.strip_prefix("sha256:").unwrap()),
        )
        .unwrap(),
    )
    .unwrap()
}

/// Serve `documents` (beside the fixture) at their own digests; returns them.
fn serving(documents: &[Vec<u8>], behavior: Behavior) -> (TestRegistry, Vec<String>) {
    let mut behavior = behavior;
    let digests: Vec<String> = documents.iter().map(|bytes| sha256(bytes)).collect();
    for (digest, bytes) in digests.iter().zip(documents) {
        behavior.tamper.insert(digest.clone(), bytes.clone());
    }
    (registry(behavior), digests)
}

fn blob_requests(registry: &TestRegistry) -> Vec<String> {
    registry
        .paths()
        .into_iter()
        .filter(|path| path.contains("/blobs/"))
        .collect()
}

#[test]
fn an_index_entry_digest_is_well_formed_before_it_is_requested() {
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": "sha256:../../other/manifests/x?q",
            "size": 10,
            "platform": {"os": "linux", "architecture": "arm64"}
        }]
    }))
    .unwrap();
    let (registry, digests) = serving(&[index], Behavior::default());
    let reference = registry.reference(&digests[0]);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    assert!(
        matches!(&result, Err(OciImportError::Unsupported(detail)) if detail.contains("only sha256")),
        "{result:?}"
    );
    let manifests: Vec<_> = registry
        .paths()
        .into_iter()
        .filter(|path| path.contains("/manifests/"))
        .collect();
    assert!(
        manifests.iter().all(|path| path.ends_with(&digests[0])),
        "{manifests:?}"
    );
}

#[test]
fn what_the_import_would_refuse_is_refused_before_any_blob_is_requested() {
    let fixture = fixture();
    let manifest = fixture_json(&fixture.arm64.digest);
    let mut not_an_image = manifest.clone();
    not_an_image["config"]["mediaType"] = "application/vnd.cncf.helm.config.v1+json".into();
    let mut zstd = manifest.clone();
    zstd["layers"][1]["mediaType"] = "application/vnd.oci.image.layer.v1.tar+zstd".into();
    let mut huge_config = manifest.clone();
    huge_config["config"]["size"] = ((4u64 << 20) + 1).into();
    for (lie, refusal) in [
        (not_an_image, "not a container image"),
        (zstd, "layer media type"),
        (huge_config, "larger than"),
    ] {
        let (registry, digests) =
            serving(&[serde_json::to_vec(&lie).unwrap()], Behavior::default());
        let reference = registry.reference(&digests[0]);
        let (cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
        assert!(
            matches!(&result, Err(OciImportError::Unsupported(detail)) if detail.contains(refusal)),
            "{refusal}: {result:?}"
        );
        assert!(
            blob_requests(&registry).is_empty(),
            "{refusal}: {:?}",
            registry.paths()
        );
        assert!(cas_entries(cas.path()).is_empty());
    }

    // A config descriptor of exactly 4 MiB is admitted, and its blob is
    // requested (here it is not that size, so it is then refused for that).
    let mut largest_config = manifest.clone();
    largest_config["config"]["size"] = (4u64 << 20).into();
    let (registry, digests) = serving(
        &[serde_json::to_vec(&largest_config).unwrap()],
        Behavior::default(),
    );
    let reference = registry.reference(&digests[0]);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    assert!(
        matches!(&result, Err(OciImportError::DigestMismatch { reason, .. }) if reason.contains("size")),
        "{result:?}"
    );
    assert!(
        !blob_requests(&registry).is_empty(),
        "{:?}",
        registry.paths()
    );
}

#[test]
fn a_token_that_is_not_a_header_value_is_refused() {
    let fixture = fixture();
    for token in ["", "two words", "line\nbreak"] {
        let reply = serde_json::to_vec(&serde_json::json!({ "token": token })).unwrap();
        let registry = registry(Behavior {
            token_reply: Some(("200 OK".into(), reply)),
            ..Behavior::default()
        });
        let reference = registry.reference(&fixture.index.digest);
        let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
        assert!(
            matches!(&result, Err(OciImportError::Registry(detail)) if detail.contains("no usable token")),
            "{token:?}: {result:?}"
        );
    }
    // `access_token` is read when `token` is absent.
    let reply = serde_json::to_vec(&serde_json::json!({ "access_token": TOKEN })).unwrap();
    let registry = registry(Behavior {
        token_reply: Some(("200 OK".into(), reply)),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    result.expect("a token under access_token");
}

#[test]
fn a_layer_listed_twice_is_fetched_once() {
    let fixture = fixture();
    let mut manifest = fixture_json(&fixture.arm64.digest);
    let mut config = fixture_json(manifest["config"]["digest"].as_str().unwrap());
    let layer = manifest["layers"][0].clone();
    let diff_id = config["rootfs"]["diff_ids"][0].clone();
    manifest["layers"] = serde_json::json!([layer.clone(), layer.clone()]);
    config["rootfs"]["diff_ids"] = serde_json::json!([diff_id.clone(), diff_id]);
    let config = serde_json::to_vec(&config).unwrap();
    manifest["config"]["digest"] = sha256(&config).into();
    manifest["config"]["size"] = config.len().into();
    let manifest = serde_json::to_vec(&manifest).unwrap();
    let (registry, digests) = serving(&[manifest, config], Behavior::default());
    let reference = registry.reference(&digests[0]);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    result.expect("an image that lists one layer twice");
    let layer = layer["digest"].as_str().unwrap();
    let fetched: Vec<_> = blob_requests(&registry)
        .into_iter()
        .filter(|path| path.starts_with("/v2/") && path.ends_with(layer))
        .collect();
    assert_eq!(fetched.len(), 1, "{:?}", registry.paths());
}

#[test]
fn a_registry_that_refuses_the_anonymous_token_is_refused() {
    let fixture = fixture();
    let registry = registry(Behavior {
        refuse_token: true,
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    let error = result.expect_err("a refused token");
    assert!(
        matches!(&error, OciImportError::Registry(detail) if detail.contains("credentials are not supported")),
        "{error}"
    );
    assert_no_token(&error);
    let tokens = registry
        .paths()
        .iter()
        .filter(|path| path.starts_with("/token"))
        .count();
    assert_eq!(tokens, 1, "{:?}", registry.paths());
}

#[test]
fn a_token_endpoint_that_challenges_is_not_answered() {
    let fixture = fixture();
    let registry = registry(Behavior {
        token_reply: Some(("401 Unauthorized".into(), Vec::new())),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    assert!(
        matches!(&result, Err(OciImportError::Registry(detail)) if detail.contains("401")),
        "{result:?}"
    );
    let tokens = registry
        .paths()
        .iter()
        .filter(|path| path.starts_with("/token"))
        .count();
    assert_eq!(tokens, 1, "{:?}", registry.paths());
}

#[test]
fn a_token_reply_is_bounded_at_sixty_four_kibibytes() {
    let fixture = fixture();
    let padded = |length: usize| {
        let head = format!("{{\"token\":\"{TOKEN}\"");
        let mut reply = head.into_bytes();
        reply.resize(length - 1, b' ');
        reply.push(b'}');
        reply
    };
    let registry = registry(Behavior {
        token_reply: Some(("200 OK".into(), padded(64 << 10))),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    result.expect("a token reply of exactly 64 KiB");

    let registry = self::registry(Behavior {
        token_reply: Some(("200 OK".into(), padded((64 << 10) + 1))),
        ..Behavior::default()
    });
    let reference = registry.reference(&fixture.index.digest);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    assert!(
        matches!(&result, Err(OciImportError::Registry(detail)) if detail.contains("larger than")),
        "{result:?}"
    );
}

#[test]
fn a_document_may_use_the_whole_remaining_budget() {
    let fixture = fixture();
    let registry = registry(Behavior::default());
    let reference = registry.reference(&fixture.index.digest);
    let request = RegistryPull {
        limits: RegistryLimits {
            max_download_bytes: blob_size(&fixture.index.digest),
            ..RegistryLimits::default()
        },
        ..pull(&reference, Some(&registry.authority))
    };
    let (_cas, result) = pull_into(&request);
    assert!(
        matches!(result, Err(OciImportError::DownloadTooLarge { .. })),
        "{result:?}"
    );
    // The index fit exactly; the manifest after it did not.
    assert!(
        registry
            .paths()
            .iter()
            .any(|path| path.contains(fixture.arm64.hex())),
        "{:?}",
        registry.paths()
    );
}

#[test]
fn documents_are_bounded_at_four_mebibytes() {
    let fixture = fixture();
    let padded = |length: usize| {
        let mut index = fs::read(
            checked_in_fixture()
                .join("blobs/sha256")
                .join(fixture.index.hex()),
        )
        .unwrap();
        index.resize(length, b' ');
        index
    };
    let (registry, digests) = serving(&[padded(4 << 20)], Behavior::default());
    let reference = registry.reference(&digests[0]);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    result.expect("an index of exactly 4 MiB");

    let (registry, digests) = serving(&[padded((4 << 20) + 1)], Behavior::default());
    let reference = registry.reference(&digests[0]);
    let (_cas, result) = pull_into(&pull(&reference, Some(&registry.authority)));
    assert!(
        matches!(&result, Err(OciImportError::Unsupported(detail)) if detail.contains("larger than")),
        "{result:?}"
    );
}

fn network_tests_enabled(what: &str) -> bool {
    let enabled =
        std::env::var_os("LEYLINE_REGISTRY_NETWORK_TESTS").is_some_and(|value| value == "1");
    if !enabled {
        eprintln!(
            "SKIPPED (NOT a pass): pulling {what} from Docker Hub needs the network; \
             set LEYLINE_REGISTRY_NETWORK_TESTS=1 to run it"
        );
    }
    enabled
}

/// A Debian-based image (Cloister's request): dolthub/dolt-sql-server's
/// index, pinned by the digest Docker Hub served for `latest` on
/// 2026-11-10, imported twice with its links kept (`--symlinks preserve`,
/// rootfs manifest v2, bead `ley-line-open-4f08ea`): the 14 links
/// `resolve` dropped as dangling are kept. The identity and the counts are
/// known answers. Opt-in: it needs the network and about 3 GB of temporary
/// space.
///
/// The image holds paths that differ only in case
/// (`usr/share/perl/5.36.0/Pod` and `…/pod`, `…/perl/5.36.0/Sys` and
/// `…/sys`), so on a case-insensitive host filesystem (macOS's default
/// APFS) the import refuses to write it, as it refuses any such image. On
/// macOS run it with `TMPDIR` on a case-sensitive volume.
#[test]
fn dolt_sql_server_from_docker_hub_is_a_known_answer() {
    if !network_tests_enabled("dolthub/dolt-sql-server") {
        return;
    }
    let reference = "dolthub/dolt-sql-server@sha256:4208ed89253c9d6a36af78f0129a293dc892681b5aca50ae66b943704b8fe052";
    let request = RegistryPull {
        platform: Platform::linux("arm64"),
        symlinks: SymlinkPolicy::Preserve,
        ..pull(reference, None)
    };
    let (_first_cas, first) = pull_into(&request);
    let first = first.expect("pull dolt-sql-server from Docker Hub");
    let (_second_cas, second) = pull_into(&request);
    let second = second.expect("pull dolt-sql-server again");
    assert_eq!(first, second, "two pulls of one digest");
    let counts = [
        first.files,
        first.directories,
        first.symlinks,
        first.special_mode_bits_cleared,
        first.symlinks_resolved,
        first.special_entries_dropped.len(),
        first.links_dropped_kernel_mount.len(),
        first.links_dropped_dangling.len(),
        first.unreadable_files_dropped.len(),
    ];
    eprintln!(
        "dolt-sql-server arm64: rootfs blake3-256:{} manifest {} [files, directories, symlinks, \
         specialModeBitsCleared, symlinksResolved, specialEntriesDropped, \
         linksDroppedKernelMount, linksDroppedDangling, unreadableFilesDropped] = {counts:?}",
        first.rootfs.value, first.image.manifest
    );
    assert_eq!(
        first.image.manifest,
        "sha256:6571792965684c0adae45d9062cf4c91d4d81e248cc4b02a4bfeb94d75561280"
    );
    assert_eq!(first.rootfs.value, DOLT_ARM64_ROOTFS);
    assert_eq!(counts, DOLT_ARM64_COUNTS);
}

const DOLT_ARM64_ROOTFS: &str = "11cf2e2525b9c36526269b9ebd7bfe01f176b24db0a076843223746369e23436";
/// files, directories, symlinks, specialModeBitsCleared, symlinksResolved,
/// specialEntriesDropped, linksDroppedKernelMount, linksDroppedDangling,
/// unreadableFilesDropped.
const DOLT_ARM64_COUNTS: [usize; 9] = [5362, 900, 1015, 13, 0, 0, 0, 0, 0];

/// The bead's known answer, against Docker Hub. Opt-in: it needs the network.
#[test]
fn hello_world_from_docker_hub_is_a_known_answer() {
    if !network_tests_enabled("hello-world") {
        return;
    }
    let reference =
        "hello-world@sha256:c567b231f65614688711a75f31518a064258a51d442117a3e771a3a8fdba78ef";
    // The known answer is linux/arm64's (hello-world's binary differs per
    // architecture); an import never runs the image, so any host checks it.
    let request = RegistryPull {
        platform: Platform::linux("arm64"),
        symlinks: SymlinkPolicy::Refuse,
        ..pull(reference, None)
    };
    let (_cas, result) = pull_into(&request);
    let imported = result.expect("pull hello-world from Docker Hub");
    // Rootfs manifest v2 (bead ley-line-open-4f08ea); the layout import of
    // the same digest in tests/oci_rootfs.rs records the same answer.
    assert_eq!(
        imported.rootfs.value,
        "6fbd5b6c842ee54f7461384badeefb0b922be7fcb5e9f0a3b593431888c1ba63"
    );
}
