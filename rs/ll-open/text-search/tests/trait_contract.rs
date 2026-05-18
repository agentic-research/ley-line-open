//! Trait-contract tests exercised through `dyn TextSearchEngine`.
//!
//! These pin the daemon's user-facing assumptions — the dispatch path
//! holds `Arc<dyn TextSearchEngine>`, never a concrete engine, so the
//! contract must survive trait-object dispatch.

use leyline_text_search::null::NullEngine;
use leyline_text_search::{Error, TextSearchEngine};

#[cfg(feature = "engine-witchcraft")]
use leyline_text_search::witchcraft::WitchcraftStub;

fn engines() -> Vec<(&'static str, Box<dyn TextSearchEngine>)> {
    let mut v: Vec<(&'static str, Box<dyn TextSearchEngine>)> =
        vec![("null", Box::new(NullEngine::new()))];

    #[cfg(feature = "engine-witchcraft")]
    v.push(("witchcraft-stub", Box::new(WitchcraftStub::new())));

    v
}

#[test]
fn dyn_dispatch_preserves_not_implemented_variant() {
    // When the daemon calls into the trait via `Arc<dyn TextSearchEngine>`,
    // the NotImplemented variant must still match — i.e. the trait is
    // object-safe AND the error type round-trips through dispatch
    // unchanged. A refactor that wrapped the trait return in `anyhow::Error`
    // would lose the variant and make the daemon's structured error path
    // collapse to a string.
    for (label, engine) in engines() {
        let err = engine
            .search("hello", 5)
            .expect_err("every shipped engine today is a no-backend stub — search MUST error");
        assert!(
            matches!(err, Error::NotImplemented(_)),
            "engine {label}: expected Error::NotImplemented, got {err:?}",
        );
    }
}

#[test]
fn is_empty_uses_default_impl_via_len() {
    // is_empty has a default impl in the trait. Make sure dyn dispatch
    // routes through it (not a hidden override) by asserting the error
    // it returns is exactly len's error — the NotImplemented payload
    // for `len`, not for `is_empty`. If a future override is added,
    // update this test to assert the new variant.
    let null = NullEngine::new();
    let err = null
        .is_empty()
        .expect_err("null::len errors → is_empty errors");
    if let Error::NotImplemented(op) = err {
        assert_eq!(
            op, "len",
            "is_empty must delegate to len; getting a different op name here \
             means someone added an is_empty override without updating this gate",
        );
    } else {
        panic!("expected NotImplemented");
    }
}
