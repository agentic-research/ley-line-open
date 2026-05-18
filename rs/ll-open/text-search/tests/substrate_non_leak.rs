//! Substrate non-leak gate.
//!
//! Any [`TextSearchEngine`] is sidecar by contract: its storage MUST NOT
//! live inside an arena directory, and re-indexing MUST NOT advance the
//! Σ Merkle-CAS root. This gate asserts the first half directly via the
//! trait's `storage_path()` accessor, against every engine impl this
//! crate ships.
//!
//! The second half (root non-advance under indexing) is a daemon-level
//! invariant — it'll move into `leyline-cli-lib`'s integration tests once
//! the `op_text_search` wiring lands. Today this crate has no daemon
//! context to assert against.

use std::path::Path;

use leyline_text_search::TextSearchEngine;
use leyline_text_search::null::NullEngine;

#[cfg(feature = "engine-witchcraft")]
use leyline_text_search::witchcraft::WitchcraftStub;

/// Assert an engine's storage path, if any, is OUTSIDE the given arena dir.
/// `None` is trivially passing — there is no storage to leak.
fn assert_storage_outside_arena(engine: &dyn TextSearchEngine, arena: &Path) {
    match engine.storage_path() {
        None => {
            // No storage at all — can't leak. Acceptable for the stub
            // engines that ship today.
        }
        Some(p) => {
            assert!(
                !p.starts_with(arena),
                "text-search engine storage path `{}` is inside the arena directory `{}`. \
                 This violates the substrate sidecar contract — engines MUST own their \
                 own storage, not write into the arena (which would couple their bytes \
                 to the Σ Merkle root and break ADR-0014's canonical-byte stability).",
                p.display(),
                arena.display(),
            );
        }
    }
}

#[test]
fn null_engine_storage_is_outside_arena() {
    // NullEngine has no storage at all; trivially passes. Pin the
    // invocation anyway so this gate has at least one always-on engine
    // exercising the path — a future engine that introduces real storage
    // will inherit the same harness.
    let arena = tempfile::tempdir().expect("tempdir");
    let engine = NullEngine::new();
    assert_storage_outside_arena(&engine, arena.path());
}

#[cfg(feature = "engine-witchcraft")]
#[test]
fn witchcraft_stub_storage_is_outside_arena() {
    // The stub returns None today. When the real engine lands and
    // returns Some(path), this gate is what catches a misconfigured
    // engine pointed at the arena dir.
    let arena = tempfile::tempdir().expect("tempdir");
    let engine = WitchcraftStub::new();
    assert_storage_outside_arena(&engine, arena.path());
}
