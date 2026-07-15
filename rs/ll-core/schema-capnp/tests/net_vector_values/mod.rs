//! Shared builders for the leyline-net/v1 conformance vector values
//! (bead `ley-line-open-083344`).
//!
//! One function per vector, mirroring the consts in
//! `rs/ll-core/schema-spec/leyline-net/v1/test-vectors/fixtures.capnp`
//! (which in turn value-mirror cloister's `wire/cross-check-fixtures.capnp`).
//! Consumed by BOTH the generator example (`gen_leyline_net_vectors`)
//! and the drift-gate test (`leyline_net_vectors`) via `#[path]` include,
//! so the two can never disagree about what the values are.
//!
//! Each builder returns `(reference_bytes, canonical_bytes)`:
//!
//! - **reference**: `capnp::serialize::write_message` of the freshly
//!   built single-segment message — declared section sizes, no
//!   trailing-zero truncation. Byte-equal to `capnp eval -b` output and
//!   to cloister's committed `test/wire/fixtures/canonical.ts` arrays
//!   (verified empirically under capnp =0.25.0).
//! - **canonical**: strict canonical form via `set_root_canonical`
//!   (truncates trailing zero data/pointer words) — the T8.10 fixture
//!   discipline used by every other LLO cross-runtime fixture.
//!
//! Build order inside each function is load-bearing for the reference
//! form (allocation order determines pointer-target layout); do not
//! reorder `init_*`/`set_*` calls without regenerating the vectors.

use leyline_schema_capnp::net_capnp::{manifest, tool_call, tool_result};

/// (kebab-case vector name, reference bytes, canonical bytes).
pub type Vector = (&'static str, Vec<u8>, Vec<u8>);

fn both<T, F>(f: F) -> (Vec<u8>, Vec<u8>)
where
    T: capnp::traits::Owned,
    F: FnOnce(&mut capnp::message::Builder<capnp::message::HeapAllocator>),
{
    let mut src = capnp::message::Builder::new_default();
    f(&mut src);

    let mut reference = Vec::new();
    capnp::serialize::write_message(&mut reference, &src).unwrap();

    let mut canon = capnp::message::Builder::new_default();
    canon
        .set_root_canonical(
            src.get_root_as_reader::<<T as capnp::traits::Owned>::Reader<'_>>()
                .unwrap(),
        )
        .unwrap();
    let mut canonical = Vec::new();
    capnp::serialize::write_message(&mut canonical, &canon).unwrap();

    (reference, canonical)
}

pub fn manifest_canonical() -> (Vec<u8>, Vec<u8>) {
    both::<manifest::Owned, _>(|b| {
        let mut m: manifest::Builder = b.init_root();
        m.set_sequence(42);
        m.set_public_key(&[0x11u8; 32]);
        m.set_signature(&[0x22u8; 64]);
        m.set_content_hash(&[0x33u8; 32]);
    })
}

pub fn manifest_zero_sequence() -> (Vec<u8>, Vec<u8>) {
    // contentHash = SHA-256 of the empty string.
    const EMPTY_SHA256: [u8; 32] = [
        0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9,
        0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52,
        0xb8, 0x55,
    ];
    both::<manifest::Owned, _>(|b| {
        let mut m: manifest::Builder = b.init_root();
        m.set_sequence(0);
        m.set_public_key(&[0u8; 32]);
        m.set_signature(&[0u8; 64]);
        m.set_content_hash(&EMPTY_SHA256);
    })
}

pub fn tool_call_basic() -> (Vec<u8>, Vec<u8>) {
    both::<tool_call::Owned, _>(|b| {
        let mut t: tool_call::Builder = b.init_root();
        t.set_upstream_id("rosary");
        t.set_tool_name("rsry_status");
        t.set_arguments_json(b"{}");
    })
}

pub fn tool_call_empty() -> (Vec<u8>, Vec<u8>) {
    both::<tool_call::Owned, _>(|b| {
        let mut t: tool_call::Builder = b.init_root();
        t.set_upstream_id("");
        t.set_tool_name("");
        // argumentsJson intentionally unset (defaulted empty Data).
    })
}

pub fn tool_call_with_args() -> (Vec<u8>, Vec<u8>) {
    both::<tool_call::Owned, _>(|b| {
        let mut t: tool_call::Builder = b.init_root();
        t.set_upstream_id("leyline");
        t.set_tool_name("lsp_hover");
        t.set_arguments_json(br#"{"col":5,"file":"/x/foo.rs","line":10}"#);
    })
}

pub fn tool_result_empty() -> (Vec<u8>, Vec<u8>) {
    both::<tool_result::Owned, _>(|b| {
        let mut t: tool_result::Builder = b.init_root();
        t.reborrow().init_content(0);
        t.set_is_error(false);
    })
}

pub fn tool_result_error_empty() -> (Vec<u8>, Vec<u8>) {
    both::<tool_result::Owned, _>(|b| {
        let mut t: tool_result::Builder = b.init_root();
        t.reborrow().init_content(0);
        t.set_is_error(true);
    })
}

pub fn tool_result_text() -> (Vec<u8>, Vec<u8>) {
    both::<tool_result::Owned, _>(|b| {
        let t: tool_result::Builder = b.init_root();
        let c = t.init_content(1);
        c.get(0).init_body().set_text("hello world");
    })
}

pub fn tool_result_resource() -> (Vec<u8>, Vec<u8>) {
    both::<tool_result::Owned, _>(|b| {
        let t: tool_result::Builder = b.init_root();
        let c = t.init_content(1);
        c.get(0).init_body().set_resource(b"opaque");
    })
}

pub fn tool_result_binary() -> (Vec<u8>, Vec<u8>) {
    both::<tool_result::Owned, _>(|b| {
        let t: tool_result::Builder = b.init_root();
        let c = t.init_content(1);
        let mut bin = c.get(0).init_body().init_binary();
        bin.set_data(&[0x89, 0x50, 0x4e, 0x47]);
        bin.set_mime_type("image/png");
    })
}

pub fn tool_result_mixed() -> (Vec<u8>, Vec<u8>) {
    both::<tool_result::Owned, _>(|b| {
        let t: tool_result::Builder = b.init_root();
        let mut c = t.init_content(4);
        c.reborrow().get(0).init_body().set_text("first");
        {
            let mut bin = c.reborrow().get(1).init_body().init_binary();
            bin.set_data(&[1, 2, 3]);
            bin.set_mime_type("application/octet-stream");
        }
        c.reborrow().get(2).init_body().set_resource(b"opaque2");
        c.reborrow().get(3).init_body().set_text("last");
    })
}

pub fn tool_result_error_with_text() -> (Vec<u8>, Vec<u8>) {
    both::<tool_result::Owned, _>(|b| {
        let mut t: tool_result::Builder = b.init_root();
        {
            let c = t.reborrow().init_content(1);
            c.get(0)
                .init_body()
                .set_text("tool failed: missing 'file' argument");
        }
        t.set_is_error(true);
    })
}

/// All 12 vectors, in the canonical listing order used by the generator,
/// the drift tests, and `digests.json`.
pub fn all_vectors() -> Vec<Vector> {
    let items: [(&'static str, fn() -> (Vec<u8>, Vec<u8>)); 12] = [
        ("manifest-canonical", manifest_canonical),
        ("manifest-zero-sequence", manifest_zero_sequence),
        ("tool-call-basic", tool_call_basic),
        ("tool-call-empty", tool_call_empty),
        ("tool-call-with-args", tool_call_with_args),
        ("tool-result-empty", tool_result_empty),
        ("tool-result-error-empty", tool_result_error_empty),
        ("tool-result-text", tool_result_text),
        ("tool-result-resource", tool_result_resource),
        ("tool-result-binary", tool_result_binary),
        ("tool-result-mixed", tool_result_mixed),
        ("tool-result-error-with-text", tool_result_error_with_text),
    ];
    items
        .into_iter()
        .map(|(name, f)| {
            let (reference, canonical) = f();
            (name, reference, canonical)
        })
        .collect()
}

/// camelCase const name in `fixtures.capnp` (and cloister's
/// `cross-check-fixtures.capnp`) for a kebab-case vector name.
pub fn const_name(vector_name: &str) -> String {
    let mut out = String::new();
    let mut upper_next = false;
    for ch in vector_name.chars() {
        if ch == '-' {
            upper_next = true;
        } else if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}
