//! Substrate non-leak gate.
//!
//! Any [`TextSearchEngine`] is sidecar by contract: its storage MUST NOT
//! live inside an arena directory, and re-indexing MUST NOT advance the
//! Σ Merkle-CAS root. This gate asserts the first half directly via the
//! trait's `storage_path()` accessor, against every engine impl this
//! crate ships.
//!
//! The second half (root non-advance under indexing) is a daemon-level
//! invariant that wants a real `DaemonContext` in scope. The gate for
//! that property is **not landed in this PR** — the trait surface
//! alone can't prove it. Sketched as follow-up: a `leyline-cli-lib`
//! integration test that drives `op_text_search` against a context
//! wired with `WitchcraftEngine`, captures `current_root` before/after
//! a sequence of upsert+search calls, and asserts equality. Lives
//! next to the existing `op_text_search` round-trip tests when added.

use std::path::Path;

use leyline_text_search::TextSearchEngine;
use leyline_text_search::null::NullEngine;

#[cfg(feature = "engine-witchcraft")]
use leyline_text_search::witchcraft::WitchcraftEngine;

/// `None` is trivially passing — there is no storage to leak. `Some(p)`
/// must lie OUTSIDE `arena`.
fn assert_storage_outside_arena(engine: &dyn TextSearchEngine, arena: &Path) {
    if let Some(p) = engine.storage_path() {
        assert!(
            !p.starts_with(arena),
            "text-search engine storage path `{}` is inside the arena directory `{}`. \
             This violates the sidecar contract — engines MUST own their own storage, \
             not write into the arena (which would couple their bytes to the Σ Merkle \
             root and break ADR-0014's canonical-byte stability).",
            p.display(),
            arena.display(),
        );
    }
}

#[test]
fn null_engine_storage_is_outside_arena() {
    // NullEngine has no storage at all; trivially passes. Pin the
    // invocation so this gate has at least one always-on engine
    // exercising the path; future engines inherit the harness.
    let arena = tempfile::tempdir().expect("tempdir");
    assert_storage_outside_arena(&NullEngine::new(), arena.path());
}

/// WitchcraftEngine's open() requires a real T5 assets directory. Gate
/// this on an env var so dev machines without the model can still run
/// the test suite; CI sets the env var when an assets bundle is staged.
#[cfg(feature = "engine-witchcraft")]
#[test]
fn witchcraft_engine_storage_is_outside_arena() {
    let Some(assets) = std::env::var_os("WITCHCRAFT_ASSETS_DIR") else {
        eprintln!(
            "skipping: WITCHCRAFT_ASSETS_DIR not set — assets directory must contain a \
             T5 tokenizer + safetensors to construct a real WitchcraftEngine"
        );
        return;
    };
    let arena = tempfile::tempdir().expect("tempdir");
    let storage_dir = tempfile::tempdir().expect("storage tempdir");
    // Deliberately put the engine's storage in a SIBLING dir, never
    // under arena — this is what the gate asserts.
    let db_path = storage_dir.path().join("wc.db");
    let engine = WitchcraftEngine::open(db_path, Path::new(&assets))
        .expect("witchcraft engine opens with a valid assets dir");
    assert_storage_outside_arena(&engine, arena.path());
}
