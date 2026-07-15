//! gen_leyline_net_vectors — emit leyline-net/v1 conformance vectors.
//!
//! Bead `ley-line-open-083344`. Deterministically generates the pinned
//! frame vectors for the leyline-net generic wire frames
//! (`schemas/net.capnp`: Manifest / ToolCall / ToolResult). Run with:
//!
//! ```text
//! cargo run -p leyline-schema-capnp --example gen_leyline_net_vectors -- \
//!     <output-dir>
//! ```
//!
//! Canonical `<output-dir>` is
//! `../schema-spec/leyline-net/v1/test-vectors` (relative to
//! `rs/ll-core/schema-capnp/`).
//!
//! Output layout:
//!
//! - `reference/<name>.bin` — reference-encoder bytes (plain
//!   `write_message` of the freshly built message; byte-equal to
//!   `capnp eval -b` and to cloister's committed
//!   `test/wire/fixtures/canonical.ts` arrays)
//! - `canonical/<name>.bin` — strict canonical form
//!   (`set_root_canonical`, trailing zero words truncated)
//! - `digests.json` — BLAKE3 + SHA-256 of every vector, both forms
//! - `VECTORS.sha256` — `sha256sum`-compatible pin of every vector file,
//!   `digests.json`, and `fixtures.capnp` (the value definitions)
//!
//! Determinism: every input is a constant in
//! `tests/net_vector_values/mod.rs`. Two runs on different machines
//! produce byte-equal outputs (capnp exact-pinned at =0.25.0 per
//! ADR-0014 §3).
//!
//! The drift gate lives in `tests/leyline_net_vectors.rs`: it rebuilds
//! every vector from the schema, asserts byte-equality with the
//! committed files, and asserts the BLAKE3 digests against constants
//! hardcoded in the test source. Editing any frame struct fails that
//! test loudly; regenerating with this example is the deliberate,
//! reviewable act that follows a spec version bump.

use std::env;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

#[path = "../tests/net_vector_values/mod.rs"]
mod net_vector_values;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: {} <output-dir>", args[0]);
        eprintln!("\nWrites leyline-net/v1 conformance vectors to <output-dir>.");
        eprintln!("Canonical: rs/ll-core/schema-spec/leyline-net/v1/test-vectors");
        std::process::exit(2);
    }
    let out_dir = PathBuf::from(&args[1]);
    fs::create_dir_all(out_dir.join("reference"))?;
    fs::create_dir_all(out_dir.join("canonical"))?;

    let vectors = net_vector_values::all_vectors();

    // 1. Write both byte-forms of every vector.
    for (name, reference, canonical) in &vectors {
        let ref_path = out_dir.join("reference").join(format!("{name}.bin"));
        fs::write(&ref_path, reference)?;
        println!(
            "wrote {} ({} bytes, BLAKE3 {})",
            ref_path.display(),
            reference.len(),
            hex::encode(blake3::hash(reference).as_bytes()),
        );
        let canon_path = out_dir.join("canonical").join(format!("{name}.bin"));
        fs::write(&canon_path, canonical)?;
        println!(
            "wrote {} ({} bytes, BLAKE3 {})",
            canon_path.display(),
            canonical.len(),
            hex::encode(blake3::hash(canonical).as_bytes()),
        );
    }

    // 2. digests.json — BLAKE3 + SHA-256 per vector, both forms.
    let digests = build_digests_json(&vectors);
    fs::write(out_dir.join("digests.json"), &digests)?;
    println!("wrote {}", out_dir.join("digests.json").display());

    // 3. VECTORS.sha256 — pin every vector file + digests.json +
    //    fixtures.capnp. Deterministic ordering (sorted paths).
    let vectors_sha256 = build_vectors_sha256(&out_dir)?;
    fs::write(out_dir.join("VECTORS.sha256"), &vectors_sha256)?;
    println!("wrote {}", out_dir.join("VECTORS.sha256").display());

    println!("\nDone. {} vectors x 2 forms written.", vectors.len());
    Ok(())
}

fn build_digests_json(vectors: &[net_vector_values::Vector]) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(
        "  \"$comment\": \"leyline-net/v1 conformance vector digests. `reference` is the \
         reference-encoder byte form (plain write_message / capnp eval -b — what cloister's \
         committed fixtures pin); `canonical` is strict canonical form (set_root_canonical, \
         trailing zero words truncated). Both are over the .bin file bytes verbatim. \
         Decoders MUST accept both forms; they are value-equal.\",\n",
    );
    // Lane-3 interface string per `_capability-mapping.md` §2 (owner
    // scheme `cloister/<name>/v<n>`) — the `version_bump_on_vector_change`
    // gate asserts this matches the containing spec dir.
    s.push_str("  \"version\": \"cloister/leyline-net/v1\",\n");
    s.push_str("  \"vectors\": [\n");
    for (i, (name, reference, canonical)) in vectors.iter().enumerate() {
        let trailing = if i + 1 < vectors.len() { "," } else { "" };
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{name}\",");
        let _ = writeln!(
            s,
            "      \"const\": \"{}\",",
            net_vector_values::const_name(name)
        );
        s.push_str("      \"reference\": {\n");
        let _ = writeln!(
            s,
            "        \"blake3_hex\": \"{}\",",
            hex::encode(blake3::hash(reference).as_bytes())
        );
        let _ = writeln!(
            s,
            "        \"sha256_hex\": \"{}\",",
            sha256_hex_string(reference)
        );
        let _ = writeln!(s, "        \"size\": {}", reference.len());
        s.push_str("      },\n");
        s.push_str("      \"canonical\": {\n");
        let _ = writeln!(
            s,
            "        \"blake3_hex\": \"{}\",",
            hex::encode(blake3::hash(canonical).as_bytes())
        );
        let _ = writeln!(
            s,
            "        \"sha256_hex\": \"{}\",",
            sha256_hex_string(canonical)
        );
        let _ = writeln!(s, "        \"size\": {}", canonical.len());
        s.push_str("      }\n");
        let _ = writeln!(s, "    }}{trailing}");
    }
    s.push_str("  ]\n");
    s.push_str("}\n");
    s
}

/// sha256sum-compatible pin file. Covers `reference/*.bin`,
/// `canonical/*.bin`, `digests.json`, and `fixtures.capnp` — everything
/// load-bearing. README.md is deliberately NOT pinned (errata-editable
/// prose per schema-spec LAYOUT.md).
fn build_vectors_sha256(out_dir: &Path) -> std::io::Result<String> {
    let mut rel_paths: Vec<String> = Vec::new();
    for sub in ["canonical", "reference"] {
        for entry in fs::read_dir(out_dir.join(sub))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Ok(name) = entry.file_name().into_string() {
                    rel_paths.push(format!("{sub}/{name}"));
                }
            }
        }
    }
    rel_paths.push("digests.json".to_string());
    rel_paths.push("fixtures.capnp".to_string());
    rel_paths.sort();

    let mut s = String::new();
    for rel in &rel_paths {
        let bytes = fs::read(out_dir.join(rel))?;
        let hex = sha256_hex_string(&bytes);
        let _ = writeln!(s, "{hex}  {rel}");
    }
    Ok(s)
}

/// Real SHA-256 via openssl (mirrors gen_build_cache_vectors — avoids
/// adding a sha2 dependency to this crate for a generator-only need).
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
