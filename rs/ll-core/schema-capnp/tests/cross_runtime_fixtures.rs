//! T8.10 — cross-runtime fixture suite (ADR-0014 §F8.6.4).
//!
//! Locks the **canonical-encoded byte form** of each schema as a
//! committed fixture under `tests/fixtures/*.bin`. Assertion mode
//! (default) verifies the Rust producer's bytes are byte-equal to
//! the committed fixtures. Regen mode (`--features regen-fixtures`)
//! overwrites them with fresh output for deliberate updates.
//!
//! This is the strongest CI invariant in T8: any drift in capnp's
//! encoding mechanics (a runtime bump, a schema-change-without-allowlist-
//! update, a mistakenly non-canonical write path) fails CI loudly with
//! a byte-level diff.
//!
//! **Mirror in mache:** the Go side runs an identical assertion
//! against the same `.bin` files (vendored into `mache/schemas/` per
//! mache PR #353). When this test passes on both sides AND both sides
//! decode the fixtures into field-equal records, F8.6.4 is satisfied —
//! cross-runtime byte-equal canonical encoding is mechanized, not
//! aspirational.
//!
//! Fixtures committed in this commit (initial scope):
//!   - `binding-record-minimal.bin` — all defaults (canonical truncation
//!     proof: should be near-zero size)
//!   - `binding-record-realistic.bin` — every field of the post-T8.7
//!     schema populated, including `qualifier`
//!
//! Followups (separate beads): AstNode, SourceFile, Head fixtures.

use leyline_schema_capnp::binding_capnp::binding_record;
use leyline_schema_capnp::cache_capnp::cache_lockfile;
use leyline_schema_capnp::common_capnp;
use std::path::{Path, PathBuf};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Build a `BindingRecord` with all fields at their default value (no
/// `set_*` calls). Canonical encoding's truncation rule means the
/// resulting bytes should be **minimal** — most data and pointers
/// truncated as trailing zeros.
fn build_binding_record_minimal() -> Vec<u8> {
    let mut src = capnp::message::Builder::new_default();
    {
        let _rec: binding_record::Builder = src.init_root();
        // Intentionally no set_* calls — all defaults.
    }
    let mut canonical = capnp::message::Builder::new_default();
    canonical
        .set_root_canonical(src.get_root_as_reader::<binding_record::Reader>().unwrap())
        .unwrap();
    let mut buf = Vec::new();
    capnp::serialize::write_message(&mut buf, &canonical).unwrap();
    buf
}

/// Build a `BindingRecord` with every field of the post-T8.7 schema
/// populated. Exercises every wire path: Text fields, the nested
/// `Range`/`Position` structs, the `UInt64` `parseGen`, and `qualifier`
/// (the field T8.7 added at ordinal `@7`).
fn build_binding_record_realistic() -> Vec<u8> {
    let mut src = capnp::message::Builder::new_default();
    {
        let mut rec: binding_record::Builder = src.init_root();
        rec.set_target_node_id("pkg/auth.go/function_declaration/Validate");
        rec.set_ref_token("Validate");
        rec.set_construct_node_id("pkg/main.go/function_declaration");
        rec.set_ref_site_node_id(
            "pkg/main.go/function_declaration/block/expression_list/call_expression/selector_expression/field_identifier"
        );
        rec.set_ref_uri("file:///canon/pkg/main.go");
        rec.set_parse_gen(42);
        rec.set_qualifier("auth");
        let mut r = rec.reborrow().init_ref_range();
        {
            let mut s = r.reborrow().init_start();
            s.set_line(7);
            s.set_column(11);
            s.set_byte(123);
        }
        {
            let mut e = r.reborrow().init_end();
            e.set_line(7);
            e.set_column(19);
            e.set_byte(131);
        }
    }
    let mut canonical = capnp::message::Builder::new_default();
    canonical
        .set_root_canonical(src.get_root_as_reader::<binding_record::Reader>().unwrap())
        .unwrap();
    let mut buf = Vec::new();
    capnp::serialize::write_message(&mut buf, &canonical).unwrap();
    buf
}

/// Read fixture bytes from disk; assert on absence (refusing to
/// silently treat a missing fixture as a pass).
fn read_fixture(name: &str) -> Vec<u8> {
    let path = fixtures_dir().join(name);
    std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "T8.10: fixture {} missing. To generate, run:\n  \
             cargo test -p leyline-schema-capnp --features regen-fixtures \
             --test cross_runtime_fixtures",
            path.display()
        )
    })
}

/// Write fixture bytes to disk under regen-fixtures mode.
#[cfg(feature = "regen-fixtures")]
fn write_fixture(name: &str, bytes: &[u8]) {
    let path = fixtures_dir().join(name);
    std::fs::write(&path, bytes)
        .unwrap_or_else(|_| panic!("T8.10: failed to write fixture {}", path.display()));
    eprintln!(
        "T8.10: wrote fixture {} ({} bytes)",
        path.display(),
        bytes.len()
    );
}

/// Default mode: assert producer bytes byte-equal the committed
/// fixture. Drift here means **the canonical-encoding wire shape
/// changed** — that's a load-bearing event, not a routine refactor.
/// Failure message includes the byte diff so the cause is obvious.
#[cfg(not(feature = "regen-fixtures"))]
fn assert_or_regen(name: &str, produced: &[u8]) {
    let committed = read_fixture(name);
    assert_eq!(
        produced,
        committed.as_slice(),
        "T8.10: producer bytes for {name} drifted from committed fixture.\n\
         Produced: {} bytes\n\
         Committed: {} bytes\n\
         If this drift is intentional, regenerate via:\n  \
         cargo test -p leyline-schema-capnp --features regen-fixtures \
         --test cross_runtime_fixtures\n\
         Then commit the new fixtures + update ADR-0014 if the wire format \
         changed substantively. Cross-runtime: every consumer (mache Go, \
         future TS/Swift) MUST also pass after this regen.",
        produced.len(),
        committed.len(),
    );
}

#[cfg(feature = "regen-fixtures")]
fn assert_or_regen(name: &str, produced: &[u8]) {
    write_fixture(name, produced);
}

/// F8.6.4: BindingRecord with all defaults canonicalizes to a known
/// minimal byte sequence. If this drifts, capnp's canonical-encoding
/// engine has changed under us.
#[test]
fn binding_record_minimal_matches_fixture() {
    let bytes = build_binding_record_minimal();
    assert_or_regen("binding-record-minimal.bin", &bytes);
}

/// F8.6.4: BindingRecord with every field populated canonicalizes to
/// known bytes. Exercises Text + nested struct + UInt64 + the T8.7
/// `qualifier` field.
#[test]
fn binding_record_realistic_matches_fixture() {
    let bytes = build_binding_record_realistic();
    assert_or_regen("binding-record-realistic.bin", &bytes);
}

/// F8.6.4 sister test: read back each fixture and assert the typed
/// Reader sees the fields we expect. This is the *decode* direction —
/// proves the bytes aren't merely byte-equal but semantically equal
/// (parseable and field-equal). Catches a class of bug where bytes
/// are stable but the parser misreads them.
///
/// Decodes from freshly-built bytes (not from disk) to avoid the
/// regen-mode race where the fixture-writer test and this test run
/// in parallel. In assertion mode the byte equality is enforced
/// separately by `binding_record_realistic_matches_fixture`.
#[test]
fn binding_record_realistic_round_trips_via_decoder() {
    let bytes = build_binding_record_realistic();
    let mut slice: &[u8] = &bytes;
    let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
        .expect("decode realistic fixture");
    let rec: binding_record::Reader = msg.get_root().expect("get_root");

    assert_eq!(
        rec.get_target_node_id().unwrap().to_str().unwrap(),
        "pkg/auth.go/function_declaration/Validate"
    );
    assert_eq!(rec.get_ref_token().unwrap().to_str().unwrap(), "Validate");
    assert_eq!(
        rec.get_construct_node_id().unwrap().to_str().unwrap(),
        "pkg/main.go/function_declaration"
    );
    assert_eq!(
        rec.get_ref_site_node_id().unwrap().to_str().unwrap(),
        "pkg/main.go/function_declaration/block/expression_list/call_expression/selector_expression/field_identifier"
    );
    assert_eq!(
        rec.get_ref_uri().unwrap().to_str().unwrap(),
        "file:///canon/pkg/main.go"
    );
    assert_eq!(rec.get_parse_gen(), 42);
    assert_eq!(
        rec.get_qualifier().unwrap().to_str().unwrap(),
        "auth",
        "T8.7: qualifier round-trips through the cross-runtime fixture",
    );

    let r = rec.get_ref_range().unwrap();
    let s = r.get_start().unwrap();
    let e = r.get_end().unwrap();
    assert_eq!(s.get_line(), 7);
    assert_eq!(s.get_column(), 11);
    assert_eq!(s.get_byte(), 123);
    assert_eq!(e.get_line(), 7);
    assert_eq!(e.get_column(), 19);
    assert_eq!(e.get_byte(), 131);
}

/// T8.10 invariant: the *minimal* fixture is strictly smaller than
/// the *realistic* one. Pin guards against a regression where
/// canonical encoding stops truncating defaults.
#[test]
fn minimal_strictly_smaller_than_realistic() {
    let minimal = build_binding_record_minimal();
    let realistic = build_binding_record_realistic();
    assert!(
        minimal.len() < realistic.len(),
        "T8.10: minimal canonical bytes ({}) must be < realistic ({}) — \
         canonical-encoding truncation should make defaults near-empty.",
        minimal.len(),
        realistic.len(),
    );
    // Suppress dead-code warnings on Common types when only used for
    // documentation references.
    let _ = std::marker::PhantomData::<common_capnp::position::Reader>;
}

// ─────────────────────────────────────────────────────────────────────
// CacheLockfile fixtures (bead ley-line-open-ae89aa, ADR-0021)
// ─────────────────────────────────────────────────────────────────────
//
// Mirrors the BindingRecord pattern above for the cache.capnp schema.
// Two fixtures:
//
//   - cache-lockfile-minimal.bin    — only producer + schemaVersion in
//     meta; no sources, no topology, default root. Pins the "empty
//     lockfile is valid + canonically near-zero" invariant.
//
//   - cache-lockfile-realistic.bin  — producer + version + 1 processor
//     + 2 sources + 1 topology edge + populated root. Exercises every
//     wire path including the imported common.Hash type and nested
//     lists.
//
// These bytes are the cross-runtime contract: the Go binding in
// clients/go/leyline-schema/cache/ MUST decode them byte-equal and
// field-equal once the Go-side fixture test lands. Any consumer that
// implements the schema in TS / Swift / Python / whatever joins the
// same drift gate just by asserting against the same .bin files.

fn build_cache_lockfile_minimal() -> Vec<u8> {
    let mut src = capnp::message::Builder::new_default();
    {
        let mut lf: cache_lockfile::Builder = src.init_root();
        // Only producer + schemaVersion. Everything else default.
        // generatedAtMs intentionally NOT set — canonical encoding
        // should truncate the UInt64 default (0).
        let mut m = lf.reborrow().init_meta();
        m.set_producer("mache");
        m.set_schema_version("0.1.0");
    }
    let mut canonical = capnp::message::Builder::new_default();
    canonical
        .set_root_canonical(src.get_root_as_reader::<cache_lockfile::Reader>().unwrap())
        .unwrap();
    let mut buf = Vec::new();
    capnp::serialize::write_message(&mut buf, &canonical).unwrap();
    buf
}

fn build_cache_lockfile_realistic() -> Vec<u8> {
    let mut src = capnp::message::Builder::new_default();
    {
        let mut lf: cache_lockfile::Builder = src.init_root();

        // Meta — every field populated, 1 processor entry.
        {
            let mut m = lf.reborrow().init_meta();
            m.set_producer("mache");
            m.set_producer_version("0.7.1");
            m.set_schema_version("0.1.0");
            // Stable timestamp — used as a known constant so the fixture
            // bytes are deterministic across regen runs.
            m.set_generated_at_ms(1_748_345_600_000);

            let mut procs = m.init_input_processors(1);
            let mut p = procs.reborrow().get(0);
            p.set_kind("tree-sitter-go");
            p.set_version("0.21.0");
        }

        // Sources — 2 entries pinning Hash imports, ordering, and the
        // free-form `kind` text.
        {
            let mut srcs = lf.reborrow().init_sources(2);
            {
                let mut s = srcs.reborrow().get(0);
                s.set_path("src/main.go");
                {
                    let mut ih = s.reborrow().init_input_hash();
                    // Hash bytes: a recognizable pattern (0x01, 0x02, ...
                    // 0x20). Deterministic and visually distinct from
                    // chunk_hash, which uses 0xA0..0xBF.
                    let mut buf = [0u8; 32];
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = (i as u8) + 1;
                    }
                    ih.set_bytes(&buf);
                }
                {
                    let mut ch = s.reborrow().init_chunk_hash();
                    let mut buf = [0u8; 32];
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = 0xA0 + (i as u8);
                    }
                    ch.set_bytes(&buf);
                }
                s.set_kind("go-source");
            }
            {
                let mut s = srcs.reborrow().get(1);
                s.set_path("src/auth.go");
                {
                    let mut ih = s.reborrow().init_input_hash();
                    let mut buf = [0u8; 32];
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = 0x40 + (i as u8);
                    }
                    ih.set_bytes(&buf);
                }
                {
                    let mut ch = s.reborrow().init_chunk_hash();
                    let mut buf = [0u8; 32];
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = 0xC0 + (i as u8);
                    }
                    ch.set_bytes(&buf);
                }
                s.set_kind("go-source");
            }
        }

        // Topology — 1 edge: main.go depends on auth.go.
        {
            let mut edges = lf.reborrow().init_topology(1);
            let mut e = edges.reborrow().get(0);
            e.set_from("src/main.go");
            e.set_to_source("src/auth.go");
        }

        // Root — distinct pattern from the source/chunk hashes.
        {
            let mut root = lf.reborrow().init_root();
            let mut buf = [0u8; 32];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = 0xF0 ^ (i as u8);
            }
            root.set_bytes(&buf);
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

/// F8.6.4: CacheLockfile with minimal fields canonicalizes to a known
/// byte sequence. Drift here means canonical encoding for the cache
/// schema has changed — load-bearing.
#[test]
fn cache_lockfile_minimal_matches_fixture() {
    let bytes = build_cache_lockfile_minimal();
    assert_or_regen("cache-lockfile-minimal.bin", &bytes);
}

/// F8.6.4: CacheLockfile with every field populated (meta + 1
/// processor + 2 sources + 1 edge + root) canonicalizes to known bytes.
/// Exercises Text + nested struct + nested lists + imported common.Hash.
#[test]
fn cache_lockfile_realistic_matches_fixture() {
    let bytes = build_cache_lockfile_realistic();
    assert_or_regen("cache-lockfile-realistic.bin", &bytes);
}

/// F8.6.4 decode-direction test: realistic fixture, when re-read,
/// surfaces every field the producer set. Pins that bytes are not just
/// byte-equal but semantically equal across encode + decode.
#[test]
fn cache_lockfile_realistic_round_trips_via_decoder() {
    let bytes = build_cache_lockfile_realistic();
    let mut slice: &[u8] = &bytes;
    let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
        .expect("decode realistic cache lockfile");
    let lf: cache_lockfile::Reader = msg.get_root().expect("get_root");

    // Meta
    let m = lf.get_meta().unwrap();
    assert_eq!(m.get_producer().unwrap().to_str().unwrap(), "mache");
    assert_eq!(m.get_producer_version().unwrap().to_str().unwrap(), "0.7.1");
    assert_eq!(m.get_schema_version().unwrap().to_str().unwrap(), "0.1.0");
    assert_eq!(m.get_generated_at_ms(), 1_748_345_600_000);

    let procs = m.get_input_processors().unwrap();
    assert_eq!(procs.len(), 1);
    assert_eq!(procs.get(0).get_kind().unwrap().to_str().unwrap(), "tree-sitter-go");
    assert_eq!(procs.get(0).get_version().unwrap().to_str().unwrap(), "0.21.0");

    // Sources
    let srcs = lf.get_sources().unwrap();
    assert_eq!(srcs.len(), 2);

    let s0 = srcs.get(0);
    assert_eq!(s0.get_path().unwrap().to_str().unwrap(), "src/main.go");
    assert_eq!(s0.get_kind().unwrap().to_str().unwrap(), "go-source");
    let ih0 = s0.get_input_hash().unwrap().get_bytes().unwrap();
    assert_eq!(ih0.len(), 32);
    assert_eq!(ih0[0], 1);
    assert_eq!(ih0[31], 32);
    let ch0 = s0.get_chunk_hash().unwrap().get_bytes().unwrap();
    assert_eq!(ch0.len(), 32);
    assert_eq!(ch0[0], 0xA0);
    assert_eq!(ch0[31], 0xA0 + 31);

    let s1 = srcs.get(1);
    assert_eq!(s1.get_path().unwrap().to_str().unwrap(), "src/auth.go");
    let ih1 = s1.get_input_hash().unwrap().get_bytes().unwrap();
    assert_eq!(ih1[0], 0x40);
    assert_eq!(ih1[31], 0x40 + 31);
    let ch1 = s1.get_chunk_hash().unwrap().get_bytes().unwrap();
    assert_eq!(ch1[0], 0xC0);
    assert_eq!(ch1[31], 0xC0 + 31);

    // Topology
    let edges = lf.get_topology().unwrap();
    assert_eq!(edges.len(), 1);
    let e0 = edges.get(0);
    assert_eq!(e0.get_from().unwrap().to_str().unwrap(), "src/main.go");
    assert_eq!(e0.get_to_source().unwrap().to_str().unwrap(), "src/auth.go");

    // Root
    let root = lf.get_root().unwrap().get_bytes().unwrap();
    assert_eq!(root.len(), 32);
    assert_eq!(root[0], 0xF0);
    assert_eq!(root[31], 0xF0 ^ 31);
}

/// T8.10 invariant for cache: minimal canonical bytes < realistic.
/// Same canonical-truncation property as the BindingRecord siblings.
#[test]
fn cache_lockfile_minimal_strictly_smaller_than_realistic() {
    let minimal = build_cache_lockfile_minimal();
    let realistic = build_cache_lockfile_realistic();
    assert!(
        minimal.len() < realistic.len(),
        "T8.10: minimal cache lockfile ({} bytes) must be < realistic ({} bytes)",
        minimal.len(),
        realistic.len(),
    );
}

/// Re-decode the MINIMAL fixture and assert the absence of populated
/// fields is reflected on the reader side — sources/topology lists are
/// empty, root is the default (empty Hash bytes). Catches the class of
/// bug where defaults are lost on encode/decode.
#[test]
fn cache_lockfile_minimal_round_trips_via_decoder() {
    let bytes = build_cache_lockfile_minimal();
    let mut slice: &[u8] = &bytes;
    let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
        .expect("decode minimal cache lockfile");
    let lf: cache_lockfile::Reader = msg.get_root().expect("get_root");

    let m = lf.get_meta().unwrap();
    assert_eq!(m.get_producer().unwrap().to_str().unwrap(), "mache");
    assert_eq!(m.get_schema_version().unwrap().to_str().unwrap(), "0.1.0");
    assert_eq!(m.get_generated_at_ms(), 0, "default UInt64 must round-trip as 0");
    assert_eq!(m.get_input_processors().unwrap().len(), 0);

    assert_eq!(lf.get_sources().unwrap().len(), 0);
    assert_eq!(lf.get_topology().unwrap().len(), 0);
    assert_eq!(
        lf.get_root().unwrap().get_bytes().unwrap().len(),
        0,
        "default Hash has zero-length Data"
    );
}
