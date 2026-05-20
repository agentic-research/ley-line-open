//! Characterization tests for the Group A inline `blake3::hash(...)` sites
//! in `leyline-cli-lib`. Pins the exact byte-input → root-byte-output
//! mapping at each retrofit candidate so the future migration onto the Σ
//! substrate's `BlobStore` trait (`rs/ll-core/core/src/substrate.rs:145`)
//! can be verified as a zero-change refactor.
//!
//! Group A site covered here (per `docs/audits/2026-05-blake3-retrofit-audit.md`):
//!
//! - Site 1 — `rs/ll-open/cli-lib/src/cmd_load.rs:63` (`load_into_arena`)
//!   - Pinned via exact-hash assertion + the `set_arena_with_root`
//!     write-side path.
//!
//! Sibling tests (NOT in this file):
//!
//! - Site 2 — `cmd_daemon.rs:734` (`snapshot_to_arena`) → already pinned
//!   exactly by `integration.rs::snapshot_populates_current_root_with_blake3_of_db_bytes`
//!   in this same crate's test suite.
//! - Sites 3 + 4 — `fs/src/lib.rs:82` (`verify_arena_root`) and
//!   `graph.rs:954` (`flush_to_arena`) → `fs/tests/characterization_blake3_sites.rs`.
//!
//! ## Hard rule (Phase 0+1 invariant)
//!
//! This test must keep passing through Phases 2–4 (impl + migration + lint
//! gate). It is the zero-change contract. If the future retrofit needs to
//! relax this assertion, that's a behavior change — file a new design bead
//! first.

use leyline_cli_lib::cmd_load::load_into_arena;
use leyline_core::{Controller, create_arena};
use tempfile::TempDir;

/// `load_into_arena` must publish `current_root = blake3::hash(db_bytes)`
/// for whatever bytes the caller hands it.
///
/// Mirrors the contract pinned at `snapshot_to_arena` by
/// `snapshot_populates_current_root_with_blake3_of_db_bytes` in
/// `integration.rs`. After Phase 3 migration onto `BlobStore::put`, the
/// resulting `Hash` must wrap a `[u8; 32]` byte-equal to
/// `blake3::hash(db_bytes)`.
#[test]
fn load_into_arena_pins_root_to_blake3_of_input_bytes() {
    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("load.arena");
    let ctrl_path = dir.path().join("load.ctrl");
    let arena_size: u64 = 4 * 1024 * 1024;

    // Pre-create arena + register it with the controller.
    // `load_into_arena` requires `arena_path` to be non-empty on the
    // controller (`ensure!` at cmd_load.rs:42-46); the existing
    // `load_errors_when_arena_path_unset` test pins the negative side.
    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    {
        let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
        ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
            .unwrap();
    }

    // Deterministic input — chosen so the assertion is reproducible
    // across machines, OS versions, and rusqlite versions. Don't use a
    // SQLite-serialized blob here; that's the snapshot site's contract
    // and may legitimately vary across rusqlite versions. `load_into_arena`
    // is content-format-agnostic, so a raw byte string is the correct
    // input shape.
    let db_bytes: Vec<u8> =
        b"characterization-pin: load_into_arena MUST publish blake3 of input".to_vec();

    // Independently compute the expected root.
    let expected_root: [u8; 32] = blake3::hash(&db_bytes).into();

    // Load.
    load_into_arena(&ctrl_path, &db_bytes).expect("load_into_arena should succeed");

    // Re-open Controller; assert byte-for-byte equality.
    let r = Controller::open_or_create(&ctrl_path).unwrap();
    assert_eq!(
        r.current_root(),
        expected_root,
        "load_into_arena MUST publish current_root = blake3::hash(input bytes); \
         got first 8 hex = {}, expected first 8 hex = {}",
        hex_short(&r.current_root()),
        hex_short(&expected_root),
    );
    assert_ne!(
        r.current_root(),
        [0u8; 32],
        "post-load current_root must not be the zero sentinel"
    );
}

/// Idempotence pin (paired with the exact-hash pin above): loading the
/// same bytes twice produces the same root. Mirrors the
/// `snapshot_idempotent_root_for_same_db_state` test for `snapshot_to_arena`.
///
/// Retrofit invariant (Phase 2): `BlobStore::put(b)` called twice with the
/// same bytes returns equal `Hash`. This is the substrate's `idempotent`
/// axiom (`substrate.rs:145-180`).
#[test]
fn load_into_arena_is_idempotent_for_same_bytes() {
    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("load-idem.arena");
    let ctrl_path = dir.path().join("load-idem.ctrl");
    let arena_size: u64 = 4 * 1024 * 1024;

    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    {
        let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
        ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
            .unwrap();
    }

    let db_bytes: Vec<u8> = b"same bytes, same hash, same root, every time".to_vec();

    load_into_arena(&ctrl_path, &db_bytes).unwrap();
    let root_first = Controller::open_or_create(&ctrl_path)
        .unwrap()
        .current_root();

    load_into_arena(&ctrl_path, &db_bytes).unwrap();
    let root_second = Controller::open_or_create(&ctrl_path)
        .unwrap()
        .current_root();

    assert_eq!(
        root_first, root_second,
        "load_into_arena MUST be idempotent for identical inputs"
    );
}

fn hex_short(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    use std::fmt::Write;
    for b in &bytes[..4] {
        let _ = write!(s, "{b:02x}");
    }
    s
}
