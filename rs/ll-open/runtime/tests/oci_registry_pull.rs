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
use oci_fixture::{Fixture, build_fixture, checked_in_fixture};
use oci_registry::{Behavior, TOKEN, TestRegistry};

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
    let registry = registry(Behavior {
        stall_blobs: true,
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
    assert!(matches!(error, OciImportError::Registry(_)), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(cas_entries(cas.path()).is_empty());
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

/// The bead's known answer, against Docker Hub. Opt-in: it needs the network.
#[test]
fn hello_world_from_docker_hub_is_a_known_answer() {
    if std::env::var_os("LEYLINE_REGISTRY_NETWORK_TESTS").is_none_or(|value| value != "1") {
        eprintln!(
            "SKIPPED (NOT a pass): pulling hello-world from Docker Hub needs the network; \
             set LEYLINE_REGISTRY_NETWORK_TESTS=1 to run it"
        );
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
    assert_eq!(
        imported.rootfs.value,
        "8597bea8e4baf11379489ff6eb118e21af4127718e465920d328326be727e4ec"
    );
}
