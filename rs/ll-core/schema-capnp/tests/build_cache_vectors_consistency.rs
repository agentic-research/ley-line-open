//! Consumer-side conformance test for `cloister-spec/build-cache/v1`
//! vectors (bead `ley-line-open-ae89aa` / `cloister-bb168f`).
//!
//! Reads the committed vectors at
//! `<repo>/../cloister/cloister-spec/build-cache/v1/vectors/` and
//! asserts:
//!
//! 1. `lockfile-config.bin` decodes as a `CacheLockfile` via the
//!    `cache.capnp` schema.
//! 2. Each `chunk-NNN.bin` exists and BLAKE3-hashes to the value
//!    in `lockfile.sources[i].chunkHash`.
//! 3. `lockfile.root` equals `BLAKE3(concat(chunkHash[0], chunkHash[1]))`
//!    — the producer-defined root rule for this vector set.
//! 4. The lockfile's `meta` fields match what the producer committed
//!    (producer = "mache", schemaVersion = "0.1.0", etc.).
//!
//! Skips with a clear message if the cloister-spec dir isn't checked
//! out alongside LLO (e.g. fresh CI clone of only LLO). The test does
//! NOT fail in that case — the cross-repo dependency is asserted in
//! `cloister-bb168f`'s own conformance gate.
//!
//! Why this lives in LLO not cloister:
//!
//! - LLO ships the producer (the gen_build_cache_vectors example) AND
//!   the schema (cache.capnp). Adding the consumer check here keeps
//!   both halves of the contract in one place where they're auditable
//!   together.
//! - cloister can ship its OWN conformance test (TS-side using zod
//!   bindings once schema-bridge generates them) without conflict.
//!   The two tests are complementary, not duplicative.

use std::path::PathBuf;

use leyline_schema_capnp::cache_capnp::cache_lockfile;

/// Find the cloister vectors dir relative to LLO's CARGO_MANIFEST_DIR.
/// LLO layout: `<repo>/rs/ll-core/schema-capnp/`. Going up four levels
/// gets us to the workspace parent (typically `~/remotes/art/`), then
/// over to `cloister/cloister-spec/...`.
///
/// Returns `None` if the directory doesn't exist — the test SKIPS
/// rather than failing, because not every checkout has cloister
/// alongside.
fn cloister_vectors_dir() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.ancestors().nth(4)?;
    let candidate = workspace.join("cloister/cloister-spec/build-cache/v1/vectors");
    if candidate.is_dir() {
        Some(candidate)
    } else {
        None
    }
}

#[test]
fn cloister_vectors_internally_consistent() {
    let vectors = match cloister_vectors_dir() {
        Some(p) => p,
        None => {
            eprintln!(
                "SKIP: cloister-spec/build-cache/v1/vectors/ not found. \
                 This test is a cross-repo conformance gate that needs \
                 cloister checked out alongside ley-line-open. Skipping \
                 (not a failure)."
            );
            return;
        }
    };

    // Read lockfile and decode.
    let lockfile_bytes = std::fs::read(vectors.join("lockfile-config.bin"))
        .expect("read lockfile-config.bin");
    let mut slice: &[u8] = &lockfile_bytes;
    let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
        .expect("decode lockfile-config.bin as capnp message");
    let lf: cache_lockfile::Reader = msg.get_root().expect("get_root CacheLockfile");

    // Meta sanity — pins that the committed vectors haven't drifted
    // from the producer's known constants.
    let meta = lf.get_meta().expect("lockfile.meta present");
    assert_eq!(
        meta.get_producer().unwrap().to_str().unwrap(),
        "mache",
        "vector producer drift"
    );
    assert_eq!(
        meta.get_schema_version().unwrap().to_str().unwrap(),
        "0.1.0",
        "vector schemaVersion drift"
    );
    assert_eq!(
        meta.get_producer_version().unwrap().to_str().unwrap(),
        "0.7.1",
        "vector producerVersion drift"
    );
    assert_eq!(
        meta.get_generated_at_ms(),
        1_748_345_600_000,
        "vector generatedAtMs drift"
    );

    let procs = meta.get_input_processors().unwrap();
    assert_eq!(procs.len(), 2, "input processors drift");

    // Sources — verify each chunk file exists and hashes correctly.
    let sources = lf.get_sources().expect("lockfile.sources present");
    assert_eq!(sources.len(), 2, "vector sources count drift");

    let expected_paths = ["src/main.go", "src/auth.go"];
    let expected_files = ["chunk-001.bin", "chunk-002.bin"];
    let mut chunk_hashes_in_order: Vec<[u8; 32]> = Vec::with_capacity(2);

    for (i, expected_path) in expected_paths.iter().enumerate() {
        let s = sources.get(i as u32);
        let path = s.get_path().unwrap().to_str().unwrap();
        assert_eq!(path, *expected_path, "source[{i}] path drift");

        let kind = s.get_kind().unwrap().to_str().unwrap();
        assert_eq!(kind, "go-source", "source[{i}] kind drift");

        let chunk_path = vectors.join(expected_files[i]);
        let chunk_bytes = std::fs::read(&chunk_path)
            .unwrap_or_else(|_| panic!("read {}", chunk_path.display()));
        let chunk_blake3: [u8; 32] = *blake3::hash(&chunk_bytes).as_bytes();

        let claimed = s.get_chunk_hash().unwrap().get_bytes().unwrap();
        assert_eq!(
            claimed,
            &chunk_blake3,
            "source[{i}] chunkHash drift: lockfile says {} but chunk file {} hashes to {}",
            hex_encode(claimed),
            chunk_path.display(),
            hex_encode(&chunk_blake3),
        );

        chunk_hashes_in_order.push(chunk_blake3);
    }

    // Topology — verify the single edge.
    let edges = lf.get_topology().unwrap();
    assert_eq!(edges.len(), 1, "topology edge count drift");
    let e = edges.get(0);
    assert_eq!(e.get_from().unwrap().to_str().unwrap(), "src/main.go");
    assert_eq!(e.get_to_source().unwrap().to_str().unwrap(), "src/auth.go");

    // Root — must equal BLAKE3 of concatenated chunk hashes.
    let mut hasher = blake3::Hasher::new();
    for h in &chunk_hashes_in_order {
        hasher.update(h);
    }
    let expected_root: [u8; 32] = *hasher.finalize().as_bytes();
    let actual_root = lf.get_root().unwrap().get_bytes().unwrap();
    assert_eq!(
        actual_root,
        &expected_root,
        "root drift: lockfile committed {} but BLAKE3(chunkHashes) is {}",
        hex_encode(actual_root),
        hex_encode(&expected_root),
    );
}

#[test]
fn cloister_vectors_manifest_layer_digests_match_chunks() {
    let vectors = match cloister_vectors_dir() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: cloister vectors dir not found");
            return;
        }
    };

    let manifest_bytes = std::fs::read(vectors.join("manifest.json")).expect("read manifest.json");
    let manifest_str = std::str::from_utf8(&manifest_bytes).expect("manifest.json is utf-8");

    // Extract all `"digest": "sha256:<hex>"` occurrences. Order: config
    // first, then layers in declaration order.
    let mut digests: Vec<String> = Vec::new();
    let mut cursor = manifest_str;
    while let Some(pos) = cursor.find("\"digest\": \"sha256:") {
        let after_prefix = &cursor[pos + "\"digest\": \"sha256:".len()..];
        if let Some(close) = after_prefix.find('"') {
            digests.push(after_prefix[..close].to_string());
            cursor = &after_prefix[close..];
        } else {
            break;
        }
    }
    assert!(
        digests.len() >= 3,
        "manifest.json should have ≥3 digests (config + 2 layers), got {}",
        digests.len()
    );

    let config_blake3: [u8; 32] = *blake3::hash(
        &std::fs::read(vectors.join("lockfile-config.bin")).unwrap(),
    )
    .as_bytes();
    assert_eq!(
        digests[0],
        hex_encode(&config_blake3),
        "manifest.config.digest drift from lockfile-config.bin BLAKE3"
    );

    let chunk_files = ["chunk-001.bin", "chunk-002.bin"];
    for (i, fname) in chunk_files.iter().enumerate() {
        let bytes = std::fs::read(vectors.join(fname)).unwrap();
        let h: [u8; 32] = *blake3::hash(&bytes).as_bytes();
        assert_eq!(
            digests[i + 1],
            hex_encode(&h),
            "manifest.layers[{i}].digest drift from {fname} BLAKE3"
        );
    }
}

#[test]
fn cloister_vectors_sha256_self_verifies() {
    use std::process::Command;

    let vectors = match cloister_vectors_dir() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: cloister vectors dir not found");
            return;
        }
    };

    let out = Command::new("sha256sum")
        .arg("-c")
        .arg("VECTORS.sha256")
        .current_dir(&vectors)
        .output();

    let out = match out {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("SKIP: sha256sum not on PATH");
            return;
        }
        Err(e) => panic!("spawn sha256sum: {e}"),
    };

    assert!(
        out.status.success(),
        "sha256sum -c VECTORS.sha256 failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cloister_vectors_digests_json_matches_actual() {
    let vectors = match cloister_vectors_dir() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: cloister vectors dir not found");
            return;
        }
    };

    // digests.json claims specific BLAKE3 + SHA-256 values for each
    // file. Re-verify the BLAKE3 ones (the on-the-wire ones) match
    // the actual file contents. We don't re-verify SHA-256 because
    // VECTORS.sha256 is already that gate.
    let digests = std::fs::read_to_string(vectors.join("digests.json"))
        .expect("read digests.json");

    // Sanity check the lockfile-config blake3 field appears.
    let lockfile_blake3: [u8; 32] = *blake3::hash(
        &std::fs::read(vectors.join("lockfile-config.bin")).unwrap(),
    )
    .as_bytes();
    let lockfile_hex = hex_encode(&lockfile_blake3);
    assert!(
        digests.contains(&lockfile_hex),
        "digests.json should contain lockfile-config.bin BLAKE3 hex {lockfile_hex}; \
         either digests.json is stale or someone hand-edited lockfile-config.bin"
    );

    // Chunk BLAKE3 hexes should also be present.
    for fname in &["chunk-001.bin", "chunk-002.bin"] {
        let bytes = std::fs::read(vectors.join(fname)).unwrap();
        let h: [u8; 32] = *blake3::hash(&bytes).as_bytes();
        let hex = hex_encode(&h);
        assert!(
            digests.contains(&hex),
            "digests.json should contain {fname} BLAKE3 hex {hex}"
        );
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = std::fmt::Write::write_fmt(&mut s, format_args!("{b:02x}"));
    }
    s
}
