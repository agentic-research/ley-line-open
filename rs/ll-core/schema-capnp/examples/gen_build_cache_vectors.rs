//! gen_build_cache_vectors — emit cloister-spec/build-cache/v1 conformance vectors.
//!
//! Deterministically generates the artifact bundle a `build-cache/v1`
//! provider must reproduce exactly. Run with:
//!
//! ```text
//! cargo run -p leyline-schema-capnp --example gen_build_cache_vectors -- \
//!     <output-dir>
//! ```
//!
//! Typical `<output-dir>` is
//! `../../cloister/cloister-spec/build-cache/v1/vectors` (relative to
//! `rs/ll-core/schema-capnp/`).
//!
//! Output files:
//!
//! - `chunk-001.bin` — bytes for `src/main.go`'s parse output (test fixture
//!   content; not a real parse)
//! - `chunk-002.bin` — bytes for `src/auth.go`'s parse output
//! - `lockfile-config.bin` — canonical-encoded `CacheLockfile` whose
//!   `sources[].chunkHash` matches the chunks above, root is BLAKE3 of the
//!   concatenated chunkHashes
//! - `manifest.json` — OCI manifest wrapping config + chunks, per
//!   `cloister-spec/build-cache/v1/wire/manifest-shape.md`
//! - `digests.json` — every file's BLAKE3 digest (as `sha256:` per the v1
//!   wire-prefix convention) AND its SHA-256 (for tools that need a real
//!   SHA-256 sidechannel)
//! - `VECTORS.sha256` — `sha256sum`-compatible file listing every other
//!   file's real SHA-256 hash, for git-tracked integrity gating
//!
//! Determinism: every input is a constant in this file. Two runs on
//! different machines produce byte-equal outputs.

use std::env;
use std::fs;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use leyline_schema_capnp::cache_capnp::cache_lockfile;

struct ChunkSpec {
    file_name: &'static str,
    source_path: &'static str,
    kind: &'static str,
    bytes: &'static [u8],
    input_bytes: &'static [u8],
}

const CHUNKS: &[ChunkSpec] = &[
    ChunkSpec {
        file_name: "chunk-001.bin",
        source_path: "src/main.go",
        kind: "go-source",
        bytes: b"// GENERATED CHUNK 001 - cloister/build-cache/v1 conformance vector\n// Source: src/main.go, kind=go-source\n// This is faux parse output; real chunks would be capnp-encoded\n// _ast tables from mache. The point of this fixture is the\n// hash chain, not the chunk format.\n",
        input_bytes: b"package main\n\nfunc main() {\n\tauth.Validate(\"hi\")\n}\n",
    },
    ChunkSpec {
        file_name: "chunk-002.bin",
        source_path: "src/auth.go",
        kind: "go-source",
        bytes: b"// GENERATED CHUNK 002 - cloister/build-cache/v1 conformance vector\n// Source: src/auth.go, kind=go-source\n// Faux parse output (see chunk-001.bin for rationale).\n",
        input_bytes: b"package auth\n\nfunc Validate(input string) error {\n\treturn nil\n}\n",
    },
];

const GENERATED_AT_MS: u64 = 1_748_345_600_000;

struct ChunkRecord {
    file_name: String,
    source_path: &'static str,
    kind: &'static str,
    chunk_hash: [u8; 32],
    input_hash: [u8; 32],
    chunk_bytes: &'static [u8],
    size: u64,
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: {} <output-dir>", args[0]);
        eprintln!("\nWrites build-cache/v1 conformance vectors to <output-dir>.");
        eprintln!("Typical: <output-dir> = path/to/cloister/cloister-spec/build-cache/v1/vectors");
        std::process::exit(2);
    }
    let out_dir = PathBuf::from(&args[1]);
    fs::create_dir_all(&out_dir)?;

    // 1. Compute each chunk's BLAKE3 hash + write the chunk file.
    let mut records: Vec<ChunkRecord> = Vec::new();
    for spec in CHUNKS {
        let chunk_hash: [u8; 32] = *blake3::hash(spec.bytes).as_bytes();
        let input_hash: [u8; 32] = *blake3::hash(spec.input_bytes).as_bytes();
        let path = out_dir.join(spec.file_name);
        fs::write(&path, spec.bytes)?;
        records.push(ChunkRecord {
            file_name: spec.file_name.to_string(),
            source_path: spec.source_path,
            kind: spec.kind,
            chunk_hash,
            input_hash,
            chunk_bytes: spec.bytes,
            size: spec.bytes.len() as u64,
        });
        println!(
            "wrote {} ({} bytes, BLAKE3 {})",
            path.display(),
            spec.bytes.len(),
            hex::encode(chunk_hash),
        );
    }

    // 2. Build the lockfile referencing the real chunk hashes.
    let lockfile_bytes = build_lockfile(&records);
    let lockfile_path = out_dir.join("lockfile-config.bin");
    fs::write(&lockfile_path, &lockfile_bytes)?;
    let lockfile_hash: [u8; 32] = *blake3::hash(&lockfile_bytes).as_bytes();
    println!(
        "wrote {} ({} bytes, BLAKE3 {})",
        lockfile_path.display(),
        lockfile_bytes.len(),
        hex::encode(lockfile_hash),
    );

    // 3. Build the OCI manifest JSON wrapping config + chunks.
    let manifest_json = build_manifest_json(&lockfile_hash, lockfile_bytes.len() as u64, &records);
    let manifest_path = out_dir.join("manifest.json");
    fs::write(&manifest_path, &manifest_json)?;
    let manifest_hash: [u8; 32] = *blake3::hash(manifest_json.as_bytes()).as_bytes();
    println!(
        "wrote {} ({} bytes, BLAKE3 {})",
        manifest_path.display(),
        manifest_json.len(),
        hex::encode(manifest_hash),
    );

    // 4. Write digests.json — both BLAKE3 (= the on-the-wire sha256:
    //    digest per v1 convention) AND real SHA-256 sidechannel.
    let digests = build_digests_json(
        &records,
        &lockfile_hash,
        &lockfile_bytes,
        &manifest_hash,
        &manifest_json,
    );
    let digests_path = out_dir.join("digests.json");
    fs::write(&digests_path, &digests)?;
    println!("wrote {}", digests_path.display());

    // 5. VECTORS.sha256 — sha256sum-compatible listing for CI integrity
    //    gating. Listed alphabetically so the file is deterministic.
    let vectors_sha256 = build_vectors_sha256(&out_dir)?;
    let vectors_sha256_path = out_dir.join("VECTORS.sha256");
    fs::write(&vectors_sha256_path, &vectors_sha256)?;
    println!("wrote {}", vectors_sha256_path.display());

    println!(
        "\nDone. {} files written to {}",
        CHUNKS.len() + 4,
        out_dir.display()
    );
    Ok(())
}

fn build_lockfile(records: &[ChunkRecord]) -> Vec<u8> {
    let mut src = capnp::message::Builder::new_default();
    {
        let mut lf: cache_lockfile::Builder = src.init_root();

        {
            let mut m = lf.reborrow().init_meta();
            m.set_producer("mache");
            m.set_producer_version("0.7.1");
            m.set_schema_version("0.1.0");
            m.set_generated_at_ms(GENERATED_AT_MS);

            let mut procs = m.init_input_processors(2);
            {
                let mut p = procs.reborrow().get(0);
                p.set_kind("tree-sitter-go");
                p.set_version("0.21.0");
            }
            {
                let mut p = procs.reborrow().get(1);
                p.set_kind("blake3");
                p.set_version("1.5.0");
            }
        }

        {
            let mut sources = lf.reborrow().init_sources(records.len() as u32);
            for (i, r) in records.iter().enumerate() {
                let mut s = sources.reborrow().get(i as u32);
                s.set_path(r.source_path);
                s.set_kind(r.kind);
                s.reborrow().init_input_hash().set_bytes(&r.input_hash);
                s.reborrow().init_chunk_hash().set_bytes(&r.chunk_hash);
            }
        }

        {
            let mut edges = lf.reborrow().init_topology(1);
            let mut e = edges.reborrow().get(0);
            e.set_from("src/main.go");
            e.set_to_source("src/auth.go");
        }

        {
            let mut hasher = blake3::Hasher::new();
            for r in records {
                hasher.update(&r.chunk_hash);
            }
            let root_hash: [u8; 32] = *hasher.finalize().as_bytes();
            lf.reborrow().init_root().set_bytes(&root_hash);
        }
    }

    let mut canonical = capnp::message::Builder::new_default();
    canonical
        .set_root_canonical(src.get_root_as_reader::<cache_lockfile::Reader>().unwrap())
        .unwrap();
    let mut buf = Vec::new();
    capnp::serialize::write_message(&mut buf, &canonical).unwrap();
    buf
}

fn build_manifest_json(
    config_hash: &[u8; 32],
    config_size: u64,
    records: &[ChunkRecord],
) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str("  \"schemaVersion\": 2,\n");
    s.push_str("  \"mediaType\": \"application/vnd.oci.image.manifest.v1+json\",\n");
    s.push_str("  \"config\": {\n");
    s.push_str("    \"mediaType\": \"application/vnd.cloister.build-cache.v1.config+json\",\n");
    let _ = write!(s, "    \"digest\": \"sha256:{}\",\n", hex::encode(config_hash));
    let _ = write!(s, "    \"size\": {}\n", config_size);
    s.push_str("  },\n");
    s.push_str("  \"layers\": [\n");
    for (i, r) in records.iter().enumerate() {
        let trailing = if i + 1 < records.len() { "," } else { "" };
        s.push_str("    {\n");
        s.push_str(
            "      \"mediaType\": \"application/vnd.cloister.build-cache.v1.chunk\",\n",
        );
        let _ = write!(s, "      \"digest\": \"sha256:{}\",\n", hex::encode(r.chunk_hash));
        let _ = write!(s, "      \"size\": {},\n", r.size);
        s.push_str("      \"annotations\": {\n");
        let _ = write!(
            s,
            "        \"org.cloister.build-cache.kind\": \"{}\",\n",
            r.kind
        );
        let _ = write!(
            s,
            "        \"org.cloister.build-cache.path\": \"{}\"\n",
            r.source_path
        );
        s.push_str("      }\n");
        let _ = write!(s, "    }}{trailing}\n");
    }
    s.push_str("  ],\n");
    s.push_str("  \"annotations\": {\n");
    s.push_str("    \"org.cloister.build-cache.producer\": \"mache\",\n");
    s.push_str("    \"org.cloister.build-cache.producer_version\": \"0.7.1\",\n");
    s.push_str("    \"org.cloister.build-cache.schema_version\": \"0.1.0\"\n");
    s.push_str("  }\n");
    s.push_str("}\n");
    s
}

fn build_digests_json(
    records: &[ChunkRecord],
    lockfile_hash: &[u8; 32],
    lockfile_bytes: &[u8],
    manifest_hash: &[u8; 32],
    manifest_json: &str,
) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str("  \"$comment\": \"cloister/build-cache/v1 conformance vector digests. blake3_hex matches the on-the-wire sha256:<hex> per the v1 digest-encoding decision; sha256_hex is the real SHA-256 for ecosystem-tools sidechannel (commit gates, git LFS, etc.). Both are over the file's bytes verbatim.\",\n");
    s.push_str("  \"version\": \"cloister/build-cache/v1\",\n");
    s.push_str("  \"manifest\": {\n");
    let _ = write!(s, "    \"blake3_hex\": \"{}\",\n", hex::encode(manifest_hash));
    let _ = write!(
        s,
        "    \"sha256_hex\": \"{}\",\n",
        sha256_hex_string(manifest_json.as_bytes())
    );
    let _ = write!(s, "    \"size\": {}\n", manifest_json.len());
    s.push_str("  },\n");
    s.push_str("  \"lockfile_config\": {\n");
    let _ = write!(s, "    \"blake3_hex\": \"{}\",\n", hex::encode(lockfile_hash));
    let _ = write!(
        s,
        "    \"sha256_hex\": \"{}\",\n",
        sha256_hex_string(lockfile_bytes)
    );
    let _ = write!(s, "    \"size\": {}\n", lockfile_bytes.len());
    s.push_str("  },\n");
    s.push_str("  \"chunks\": [\n");
    for (i, r) in records.iter().enumerate() {
        let trailing = if i + 1 < records.len() { "," } else { "" };
        s.push_str("    {\n");
        let _ = write!(s, "      \"file\": \"{}\",\n", r.file_name);
        let _ = write!(s, "      \"source_path\": \"{}\",\n", r.source_path);
        let _ = write!(s, "      \"kind\": \"{}\",\n", r.kind);
        let _ = write!(
            s,
            "      \"chunk_blake3_hex\": \"{}\",\n",
            hex::encode(r.chunk_hash)
        );
        let _ = write!(
            s,
            "      \"chunk_sha256_hex\": \"{}\",\n",
            sha256_hex_string(r.chunk_bytes)
        );
        let _ = write!(
            s,
            "      \"input_blake3_hex\": \"{}\",\n",
            hex::encode(r.input_hash)
        );
        let _ = write!(s, "      \"chunk_size\": {}\n", r.size);
        let _ = write!(s, "    }}{trailing}\n");
    }
    s.push_str("  ]\n");
    s.push_str("}\n");
    s
}

fn sha256_hex_string(bytes: &[u8]) -> String {
    use std::process::{Command, Stdio};
    let mut child = Command::new("openssl")
        .args(["dgst", "-sha256", "-hex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn openssl");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(bytes)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait openssl");
    let line = String::from_utf8_lossy(&out.stdout);
    line.trim()
        .rsplit_once(' ')
        .map(|(_, h)| h.to_string())
        .unwrap_or_else(|| line.trim().to_string())
}

fn build_vectors_sha256(out_dir: &Path) -> std::io::Result<String> {
    let mut names: Vec<String> = fs::read_dir(out_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != "VECTORS.sha256")
        .collect();
    names.sort();

    let mut s = String::new();
    for name in &names {
        let path = out_dir.join(name);
        let bytes = fs::read(&path)?;
        let hex = sha256_hex_string(&bytes);
        let _ = writeln!(s, "{hex}  {name}");
    }
    Ok(s)
}

// `write!` for String requires `std::fmt::Write` in scope.
use std::fmt::Write;
