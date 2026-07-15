//! leyline-net/v1 conformance vector drift gate (bead `ley-line-open-083344`).
//!
//! `schemas/net.capnp` is the canonical home of the leyline-net generic
//! wire frames (Manifest / ToolCall / ToolResult), lifted from cloister's
//! `wire/cloister.capnp` (@0xa1c0157e2a1e0001). This test mechanizes the
//! wire-compatibility claim in four layers:
//!
//! 1. **Digest pins** — every vector's BLAKE3 is hardcoded HERE, in the
//!    test source. Any edit to any frame struct (field, ordinal, type,
//!    layout) changes the produced bytes and fails loudly. The pins were
//!    captured from encodings produced by the ORIGINAL cloister schema
//!    (capnp =0.25.0), so they also prove the split moved zero wire bytes.
//! 2. **Committed-file byte equality** — freshly built frames byte-equal
//!    the committed vectors in
//!    `../schema-spec/leyline-net/v1/test-vectors/{reference,canonical}/`.
//! 3. **Decode direction** — both byte forms decode via the `net.capnp`
//!    readers into the expected field values (canonical truncation must
//!    be transparent to readers).
//! 4. **Cross-schema gates** — `capnp eval -b` over the LLO fixtures file
//!    reproduces the reference bytes (LLO schema × reference encoder);
//!    and, when a cloister checkout sits alongside LLO, `capnp eval -b`
//!    over cloister's own `wire/cross-check-fixtures.capnp` reproduces
//!    the SAME bytes (live cross-repo drift gate; skips when cloister
//!    isn't checked out, mirroring `build_cache_vectors_consistency`).
//!
//! Two byte-forms per value, per the empirical finding recorded on the
//! bead: `capnp eval -b` / plain `write_message` output ("reference",
//! what cloister's committed TS fixtures pin) is NOT strict canonical
//! form for values with trailing zero words; `set_root_canonical`
//! ("canonical") truncates them. Both circulate; both are pinned;
//! decoders accept both.
//!
//! Regenerating after a DELIBERATE spec change:
//!
//! ```text
//! cargo run -p leyline-schema-capnp --example gen_leyline_net_vectors -- \
//!     ../schema-spec/leyline-net/v1/test-vectors
//! ```
//!
//! then update the pins below + the leyline-net/v1 README version stanza,
//! and coordinate downstream (cloister wire schema, rosary re-vendor —
//! rosary-086973).

use std::path::{Path, PathBuf};
use std::process::Command;

use leyline_schema_capnp::net_capnp::{content, manifest, tool_call, tool_result};

#[path = "net_vector_values/mod.rs"]
mod net_vector_values;

/// (vector name, BLAKE3(reference bytes), BLAKE3(canonical bytes)).
///
/// Captured 2026-07-15 from encodings of the original cloister schema
/// (`wire/cloister.capnp` @0xa1c0157e2a1e0001) under capnp =0.25.0;
/// byte-equal to cloister's committed `test/wire/fixtures/canonical.ts`
/// on the reference side. A change here is a WIRE-FORMAT change, never
/// a routine refactor.
const VECTOR_BLAKE3_PINS: &[(&str, &str, &str)] = &[
    (
        "manifest-canonical",
        "3353d8fcf732a760a16b3239900a68d75a0ee7fb96b85f305560c9212602aa84",
        "3353d8fcf732a760a16b3239900a68d75a0ee7fb96b85f305560c9212602aa84",
    ),
    (
        "manifest-zero-sequence",
        "fee14b099da3ba47d97e2bf5f5dc859c069bb808360dab509f79f51751aeeca9",
        "21c900f40f002c73b92172e675c10328073dd9240bfaae348845f1047ed0b58f",
    ),
    (
        "tool-call-basic",
        "c765422e012a160f24352d462eb01564329ef0877e2966365bcc2e1d0d6780bb",
        "c765422e012a160f24352d462eb01564329ef0877e2966365bcc2e1d0d6780bb",
    ),
    (
        "tool-call-empty",
        "0b1d3323e78d70f0d461fff3449110504615e365498495730e17b8246d4f4b94",
        "d8ed4f07a52ea8e3b0e29ce0ddac06a1e2ba1096b3c1417cdb873ea3f962186a",
    ),
    (
        "tool-call-with-args",
        "c7dd1743b27d098a7b1ed2a93fe92f323b5e68605d9b0f9e67b88d517e230d96",
        "c7dd1743b27d098a7b1ed2a93fe92f323b5e68605d9b0f9e67b88d517e230d96",
    ),
    (
        "tool-result-empty",
        "bacfd7083d752278f0064a32df23e749a689776a21ef568f0151047652126c52",
        "4a271d252c4db7b7330e7063af44068f22e28cbd0c9911bf886b9292d6ebcf8e",
    ),
    (
        "tool-result-error-empty",
        "01a04ef3cafda63c995e26d6d038ebc6d313b98ca97f6c5bdacca70d5f4a2b48",
        "914b1477056f3a9ebbb32986b74ccba679e94ef4831db8fad755a794e4260342",
    ),
    (
        "tool-result-text",
        "cb4a82f59b2c50a3a0cf6a2212149a412c8389bf238811982181502e83e74ace",
        "2b1ee6b1aa6f67049692bc274987666d577f535a0c76293787247328913109b8",
    ),
    (
        "tool-result-resource",
        "c50b0ec8435ab260c4b3c67a51fb293e216e499a4817f108b85228b072da9058",
        "27a81c39f32149c5da3c6da885500f406d7b0c8d57ca8754f18187eaeb29bc87",
    ),
    (
        "tool-result-binary",
        "e4f07fb8b4acc96632fc9aaffe7af2c2d3058f127c286d297386aa9b53b4ae96",
        "f38780082f17f398a8f2caf67ab8dccad95031b631b03933c35fd686616579da",
    ),
    (
        "tool-result-mixed",
        "b59a8e618eaab40bc6362931553e9592e1c3f2d7de55fbb306d0f71a14ba87f3",
        "2e70fdd966e86fd06aa9eea3ac5c14ef522ce995cc3841f62e81b1cda1297473",
    ),
    (
        "tool-result-error-with-text",
        "e55bf92bc7ffe71e7a9b6b472ac54db3729873e985f4adaef923527abf55e9d8",
        "d52292c3febc637f6ec11d8964a141c62aa8f50e090ddc0a5700b18c17fafa4d",
    ),
];

fn vectors_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../schema-spec/leyline-net/v1/test-vectors")
}

/// Layer 1: freshly built frame bytes hash to the pinned BLAKE3 values.
/// Independent of the committed files — a frame edit fails here even if
/// someone regenerates the vectors in the same change.
#[test]
fn frame_bytes_match_blake3_pins() {
    let built = net_vector_values::all_vectors();
    assert_eq!(built.len(), VECTOR_BLAKE3_PINS.len());
    for ((name, reference, canonical), (pin_name, ref_pin, canon_pin)) in
        built.iter().zip(VECTOR_BLAKE3_PINS)
    {
        assert_eq!(name, pin_name, "vector ordering drift");
        assert_eq!(
            hex::encode(blake3::hash(reference).as_bytes()),
            *ref_pin,
            "leyline-net WIRE DRIFT: {name} (reference form) no longer hashes to its pin. \
             The frame structs in schemas/net.capnp are load-bearing for cloister + rosary; \
             a byte change here is a spec version bump, not a refactor. If deliberate, \
             regenerate via gen_leyline_net_vectors, update these pins, bump the \
             leyline-net/v1 README, and coordinate downstream (rosary-086973).",
        );
        assert_eq!(
            hex::encode(blake3::hash(canonical).as_bytes()),
            *canon_pin,
            "leyline-net WIRE DRIFT: {name} (canonical form) no longer hashes to its pin. \
             See the reference-form message above for the required procedure.",
        );
    }
}

/// Layer 2: freshly built bytes byte-equal the committed vector files.
/// Catches hand-edited or stale committed vectors (the inverse failure
/// mode of layer 1).
#[test]
fn committed_vectors_match_built_frames() {
    let dir = vectors_dir();
    assert!(
        dir.is_dir(),
        "leyline-net/v1 test-vectors dir missing at {} — the spec dir is committed \
         in-repo and must exist",
        dir.display()
    );
    for (name, reference, canonical) in net_vector_values::all_vectors() {
        for (form, built) in [("reference", &reference), ("canonical", &canonical)] {
            let path = dir.join(form).join(format!("{name}.bin"));
            let committed = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("read committed vector {}: {e}", path.display()));
            assert_eq!(
                built.as_slice(),
                committed.as_slice(),
                "committed vector {} drifted from schema output ({} bytes built vs {} committed). \
                 Regenerate via gen_leyline_net_vectors if a spec change was intended.",
                path.display(),
                built.len(),
                committed.len(),
            );
        }
    }
}

/// Layer 3a: decode both byte forms of the fully populated Manifest and
/// assert field equality. Canonical truncation must be invisible to
/// readers.
#[test]
fn manifest_vectors_decode_field_equal() {
    for form in ["reference", "canonical"] {
        let bytes = read_committed(form, "manifest-canonical");
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap_or_else(|e| panic!("decode manifest-canonical ({form}): {e}"));
        let m: manifest::Reader = msg.get_root().expect("get_root Manifest");
        assert_eq!(m.get_sequence(), 42);
        assert_eq!(m.get_public_key().unwrap(), &[0x11u8; 32][..]);
        assert_eq!(m.get_signature().unwrap(), &[0x22u8; 64][..]);
        assert_eq!(m.get_content_hash().unwrap(), &[0x33u8; 32][..]);
    }
}

/// Layer 3b: ToolCall decode direction, including the defaulted-Data
/// case (tool-call-empty's omitted argumentsJson).
#[test]
fn tool_call_vectors_decode_field_equal() {
    for form in ["reference", "canonical"] {
        let bytes = read_committed(form, "tool-call-basic");
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let t: tool_call::Reader = msg.get_root().expect("get_root ToolCall");
        assert_eq!(t.get_upstream_id().unwrap().to_str().unwrap(), "rosary");
        assert_eq!(t.get_tool_name().unwrap().to_str().unwrap(), "rsry_status");
        assert_eq!(t.get_arguments_json().unwrap(), b"{}");

        let bytes = read_committed(form, "tool-call-empty");
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let t: tool_call::Reader = msg.get_root().expect("get_root ToolCall");
        assert_eq!(t.get_upstream_id().unwrap().to_str().unwrap(), "");
        assert_eq!(t.get_tool_name().unwrap().to_str().unwrap(), "");
        assert_eq!(
            t.get_arguments_json().unwrap(),
            b"",
            "omitted argumentsJson must read as empty Data in both byte forms",
        );
    }
}

/// Layer 3c: ToolResult decode direction across every union variant
/// (text / binary / resource) via the mixed vector, plus the isError
/// flag via the error vectors.
#[test]
fn tool_result_vectors_decode_field_equal() {
    for form in ["reference", "canonical"] {
        let bytes = read_committed(form, "tool-result-mixed");
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let t: tool_result::Reader = msg.get_root().expect("get_root ToolResult");
        assert!(!t.get_is_error());
        let c = t.get_content().unwrap();
        assert_eq!(c.len(), 4);
        match c.get(0).get_body().which().unwrap() {
            content::body::Text(txt) => {
                assert_eq!(txt.unwrap().to_str().unwrap(), "first")
            }
            _ => panic!("content[0] wrong variant"),
        }
        match c.get(1).get_body().which().unwrap() {
            content::body::Binary(bin) => {
                let bin = bin.unwrap();
                assert_eq!(bin.get_data().unwrap(), &[1u8, 2, 3][..]);
                assert_eq!(
                    bin.get_mime_type().unwrap().to_str().unwrap(),
                    "application/octet-stream"
                );
            }
            _ => panic!("content[1] wrong variant"),
        }
        match c.get(2).get_body().which().unwrap() {
            content::body::Resource(r) => assert_eq!(r.unwrap(), b"opaque2"),
            _ => panic!("content[2] wrong variant"),
        }
        match c.get(3).get_body().which().unwrap() {
            content::body::Text(txt) => {
                assert_eq!(txt.unwrap().to_str().unwrap(), "last")
            }
            _ => panic!("content[3] wrong variant"),
        }

        let bytes = read_committed(form, "tool-result-error-empty");
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let t: tool_result::Reader = msg.get_root().expect("get_root ToolResult");
        assert!(t.get_is_error());
        assert_eq!(t.get_content().unwrap().len(), 0);
    }
}

/// Layer 4a: `capnp eval -b` over the LLO fixtures file reproduces the
/// committed reference bytes. Proves the schema FILE (not just the Rust
/// codegen path) encodes these values to the pinned bytes — the same
/// mechanism cloister used to pin its TS fixtures. `capnp` is a hard
/// build requirement of this crate (build.rs), so no skip path.
#[test]
fn capnp_eval_on_llo_fixtures_reproduces_reference_vectors() {
    let dir = vectors_dir();
    let schemas_include = Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas");
    for (name, _, _) in VECTOR_BLAKE3_PINS {
        let const_name = net_vector_values::const_name(name);
        let out = Command::new("capnp")
            .arg("eval")
            .arg("-I")
            .arg(&schemas_include)
            .arg("--no-standard-import")
            .arg(dir.join("fixtures.capnp"))
            .arg(&const_name)
            .arg("-b")
            .output()
            .expect("spawn capnp eval (capnp is a build requirement of this crate)");
        assert!(
            out.status.success(),
            "capnp eval failed for {const_name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let committed = read_committed("reference", name);
        assert_eq!(
            out.stdout, committed,
            "capnp eval bytes for {const_name} drifted from committed reference vector \
             {name}.bin — schema file and pinned vectors disagree",
        );
    }
}

/// Layer 4b: cross-repo drift gate. When a cloister checkout sits
/// alongside LLO (`<workspace>/cloister`), `capnp eval -b` over
/// cloister's own `wire/cross-check-fixtures.capnp` must reproduce the
/// SAME reference bytes — proving net.capnp and cloister's
/// wire/cloister.capnp are wire-identical, live, not just at capture
/// time. Skips (does not fail) when cloister isn't checked out,
/// mirroring `build_cache_vectors_consistency`.
#[test]
fn cloister_schema_encodes_byte_identical_frames() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cloister = match manifest_dir.ancestors().nth(4).map(|w| w.join("cloister")) {
        Some(p) if p.join("wire/cross-check-fixtures.capnp").is_file() => p,
        _ => {
            eprintln!(
                "SKIP: cloister checkout with wire/cross-check-fixtures.capnp not found \
                 alongside LLO. Cross-repo gate needs cloister checked out (not a failure)."
            );
            return;
        }
    };
    for (name, _, _) in VECTOR_BLAKE3_PINS {
        let const_name = net_vector_values::const_name(name);
        let out = Command::new("capnp")
            .arg("eval")
            .arg("-I")
            .arg("..")
            .arg("--no-standard-import")
            .arg("wire/cross-check-fixtures.capnp")
            .arg(&const_name)
            .arg("-b")
            .current_dir(&cloister)
            .output()
            .expect("spawn capnp eval");
        assert!(
            out.status.success(),
            "capnp eval failed for cloister const {const_name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let committed = read_committed("reference", name);
        assert_eq!(
            out.stdout, committed,
            "CROSS-REPO WIRE DRIFT: cloister's wire/cloister.capnp encodes {const_name} \
             differently from LLO's net.capnp pinned vector {name}.bin. The schemas have \
             diverged — reconcile before shipping either side.",
        );
    }
}

fn read_committed(form: &str, name: &str) -> Vec<u8> {
    let path = vectors_dir().join(form).join(format!("{name}.bin"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
