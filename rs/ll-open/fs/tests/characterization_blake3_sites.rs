//! Characterization tests for the Group A inline `blake3::hash(...)` sites
//! in `leyline-fs`. Pins the exact byte-input → root-byte-output mapping at
//! each retrofit candidate so the future migration onto the Σ substrate's
//! `BlobStore` trait (`rs/ll-core/core/src/substrate.rs:145`) can be
//! verified as a zero-change refactor.
//!
//! Group A sites covered here (per `docs/audits/2026-05-blake3-retrofit-audit.md`):
//!
//! - Site 3 — `rs/ll-open/fs/src/lib.rs:82` (`verify_arena_root`)
//!   - Pinned via positive + negative + empty-arena triple below.
//! - Site 4 — `rs/ll-open/fs/src/graph.rs:954` (`flush_to_arena`)
//!   - Pinned via direct active-buffer read + exact-hash assertion below.
//!
//! Group A sites NOT covered here (covered elsewhere):
//!
//! - Site 1 — `cmd_load.rs:63` (`load_into_arena`)
//!   → `cli-lib/tests/characterization_blake3_sites.rs` (this PR).
//! - Site 2 — `cmd_daemon.rs:734` (`snapshot_to_arena`)
//!   → already pinned exactly by
//!   `cli-lib/tests/integration.rs:1164-1206` —
//!   `snapshot_populates_current_root_with_blake3_of_db_bytes`. No new
//!   test needed; that one is the canonical pin.
//!
//! ## Why these tests
//!
//! Existing tests (`arena_flush_e2e.rs`) verify behavior at a coarser grain:
//! "root advanced", "root non-zero". A retrofit that computed a *different*
//! 32-byte hash for the same input would still pass those tests and silently
//! corrupt the substrate. The tests below pin the **exact** mapping
//! `bytes → root = blake3::hash(bytes)`, so any retrofit producing different
//! bytes fails immediately and visibly.
//!
//! ## Hard rule (Phase 0+1 invariant)
//!
//! These tests must keep passing through Phases 2–4 (impl + migration + lint
//! gate). They are the zero-change contract. If a future retrofit needs to
//! relax these assertions, that's a behavior change, not a refactor — file
//! a new design bead first.

use anyhow::Result;
use bytemuck;
use memmap2::Mmap;
use rusqlite::Connection;
use std::fs::File;

use leyline_core::{ArenaHeader, Controller, layout};
use leyline_fs::graph::{Graph, HotSwapGraph, SqliteGraphAdapter};
use leyline_schema::create_schema;

/// Build a small SQLite DB and return its serialized bytes.
/// Used by every test below to keep the input deterministic.
fn seed_db() -> Vec<u8> {
    let conn = Connection::open_in_memory().unwrap();
    create_schema(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) \
         VALUES ('docs', '', 'docs', 1, 0, 1000, NULL);",
    )
    .unwrap();
    conn.serialize("main").unwrap().to_vec()
}

/// Read the active buffer's data slice from an arena file. Returns the
/// exact bytes that `verify_arena_root` (`fs/src/lib.rs:82`) hashes.
///
/// Used by `flush_to_arena_pins_root_*` to compute an *independent* hash
/// for comparison against `Controller.current_root`. If the assertion
/// holds, the retrofit's `BlobStore::put` impl is producing the canonical
/// BLAKE3 of the same bytes.
fn read_active_buffer_bytes(arena_path: &std::path::Path) -> Result<Vec<u8>> {
    let file = File::open(arena_path)?;
    let file_size = file.metadata()?.len();
    let mmap = unsafe { Mmap::map(&file)? };

    let header_bytes = &mmap[..std::mem::size_of::<ArenaHeader>()];
    let header: &ArenaHeader = bytemuck::from_bytes(header_bytes);
    let offset = header
        .active_buffer_offset(file_size)
        .ok_or_else(|| anyhow::anyhow!("malformed arena header"))? as usize;
    let data_size = header.data_size as usize;

    Ok(mmap[offset..offset + data_size].to_vec())
}

// =========================================================================
// Site 4 — flush_to_arena exact-hash pin
// =========================================================================

/// `HotSwapGraph::flush_to_arena` must publish `current_root =
/// blake3::hash(active_buffer_bytes)`.
///
/// The existing `arena_flush_e2e::flush_round_trip` test checks that the
/// root *advances*; it does **not** pin the exact hash. A retrofit that
/// produced a different 32-byte value would still pass that test. This
/// test independently re-reads the active buffer after the flush and
/// hashes it, then asserts byte-for-byte equality against
/// `Controller::current_root`.
///
/// Retrofit invariant (Phase 3): after migration, `BlobStore::put(&bytes)`
/// returns a `Hash` whose inner `[u8; 32]` equals `blake3::hash(&bytes)`
/// for the same input. Σ §3.4 locks BLAKE3.
#[test]
fn flush_to_arena_pins_root_to_blake3_of_active_buffer() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ctrl_path = dir.path().join("test.ctrl");
    let arena_path = dir.path().join("test.arena");

    // Each buffer must hold a serialized SQLite DB (~16 KB+).
    let arena_size: u64 = 4096 + 65536 * 2;

    // 1. Seed an initial arena+root so HotSwapGraph::new opens cleanly.
    let initial_bytes = seed_db();
    {
        let mut mmap = layout::create_arena(&arena_path, arena_size)?;
        layout::write_to_arena(&mut mmap, &initial_bytes)?;
    }
    {
        let mut ctrl = Controller::open_or_create(&ctrl_path)?;
        let initial_root: [u8; 32] = blake3::hash(&initial_bytes).into();
        ctrl.set_arena_with_root(arena_path.to_str().unwrap(), arena_size, initial_root)?;
    }

    // 2. Open a writable HotSwapGraph and mutate.
    let graph = HotSwapGraph::new(ctrl_path.clone())?.with_writable();
    graph.create_node("docs", "pinned.txt", false)?;
    graph.write_content("docs/pinned.txt", b"characterization pin", 0)?;

    // 3. Flush.
    graph.flush_to_arena()?;

    // 4. Read the active buffer back independently. The bytes here are
    //    exactly what `flush_to_arena` passed to `blake3::hash` (and to
    //    `write_to_arena`). Their hash MUST equal `current_root`.
    let post_flush_root = Controller::open_or_create(&ctrl_path)?.current_root();
    let active_bytes = read_active_buffer_bytes(&arena_path)?;
    let recomputed: [u8; 32] = blake3::hash(&active_bytes).into();

    assert_eq!(
        recomputed,
        post_flush_root,
        "flush_to_arena MUST publish current_root = blake3::hash(active_buffer_bytes); \
         recomputed first 8 hex = {}, current_root first 8 hex = {}",
        hex_short(&recomputed),
        hex_short(&post_flush_root),
    );

    // 5. Belt + suspenders — `from_arena` must succeed. This is a
    //    transitive pin via `verify_arena_root` (site 3).
    let fresh = SqliteGraphAdapter::from_arena(&ctrl_path)?;
    let children = fresh.list_children("docs")?;
    assert!(
        children.iter().any(|n| n.name == "pinned.txt"),
        "fresh adapter must see the post-flush writes"
    );

    Ok(())
}

// =========================================================================
// Site 3 — verify_arena_root: positive + negative + empty
// =========================================================================

/// Positive case: `from_arena` (which internally calls `verify_arena_root`
/// at `fs/src/lib.rs:82`) succeeds iff `blake3::hash(active_buffer_bytes)
/// == controller.current_root()`.
///
/// Retrofit invariant: after Phase 3, the inline `blake3::hash(data)` call
/// becomes `blob_store.hash_of(data)` (or equivalent trait method). The
/// resulting comparison logic must be byte-for-byte identical.
#[test]
fn verify_arena_root_accepts_matching_blake3() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ctrl_path = dir.path().join("test.ctrl");
    let arena_path = dir.path().join("test.arena");
    let arena_size: u64 = 4096 + 65536 * 2;

    let bytes = seed_db();
    {
        let mut mmap = layout::create_arena(&arena_path, arena_size)?;
        layout::write_to_arena(&mut mmap, &bytes)?;
    }
    {
        let mut ctrl = Controller::open_or_create(&ctrl_path)?;
        let correct_root: [u8; 32] = blake3::hash(&bytes).into();
        ctrl.set_arena_with_root(arena_path.to_str().unwrap(), arena_size, correct_root)?;
    }

    let adapter = SqliteGraphAdapter::from_arena(&ctrl_path)?;
    let node = adapter
        .get_node("docs")?
        .expect("docs node should be readable when root matches");
    assert_eq!(node.name, "docs");

    Ok(())
}

/// Negative case: if `current_root` disagrees with the buffer's actual
/// BLAKE3, `from_arena` must bail with the substrate-corruption error
/// declared at `fs/src/lib.rs:84-93`.
///
/// Retrofit invariant: the bail message text may change in Phase 3, but
/// the *behavior* (Result::Err on mismatch) is the zero-change contract.
#[test]
fn verify_arena_root_bails_on_blake3_mismatch() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ctrl_path = dir.path().join("test.ctrl");
    let arena_path = dir.path().join("test.arena");
    let arena_size: u64 = 4096 + 65536 * 2;

    let bytes = seed_db();
    {
        let mut mmap = layout::create_arena(&arena_path, arena_size)?;
        layout::write_to_arena(&mut mmap, &bytes)?;
    }
    {
        let mut ctrl = Controller::open_or_create(&ctrl_path)?;
        // Deliberately publish the WRONG root — flip one bit of the real hash.
        let mut wrong_root: [u8; 32] = blake3::hash(&bytes).into();
        wrong_root[0] ^= 0x01;
        ctrl.set_arena_with_root(arena_path.to_str().unwrap(), arena_size, wrong_root)?;
    }

    let err = SqliteGraphAdapter::from_arena(&ctrl_path)
        .map(|_| ()) // SqliteGraphAdapter isn't Debug; collapse the Ok arm.
        .expect_err("from_arena MUST bail when current_root disagrees with blake3::hash(buf)");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("root mismatch") || msg.contains("substrate corruption"),
        "expected substrate-corruption error text, got: {msg}"
    );

    Ok(())
}

/// Empty-arena case (audit finding F4): an arena with `data_size == 0`
/// is accepted regardless of `current_root` value, including the zero
/// sentinel. Pinned at `fs/src/lib.rs:66-70`.
///
/// Retrofit invariant: this branch must survive migration. A retrofit
/// that requires `BlobStore::hash_of(&[])` to match some non-zero value
/// would inadvertently break the fresh-arena case.
#[test]
fn verify_arena_root_accepts_empty_buffer_with_zero_sentinel() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ctrl_path = dir.path().join("test.ctrl");
    let arena_path = dir.path().join("test.arena");
    let arena_size: u64 = 4096 + 65536 * 2;

    // Create a fresh arena. layout::create_arena initializes the header
    // with data_size = 0; no write_to_arena call.
    let _mmap = layout::create_arena(&arena_path, arena_size)?;

    // Publish the arena path WITHOUT a root advance — current_root stays
    // as the zero sentinel. set_arena is the substrate's "fresh arena"
    // primitive vs set_arena_with_root for the post-write publish.
    {
        let mut ctrl = Controller::open_or_create(&ctrl_path)?;
        ctrl.set_arena(arena_path.to_str().unwrap(), arena_size)?;
    }

    // HotSwapGraph::new on a zero-sentinel arena should produce an empty
    // in-memory graph rather than bailing — the data_size == 0 branch in
    // verify_arena_root is the contract that makes this possible.
    let graph = HotSwapGraph::new(ctrl_path.clone())?;
    let children = graph.list_children("")?;
    assert!(
        children.is_empty(),
        "fresh-arena (data_size=0, zero sentinel) MUST serve an empty graph, not bail"
    );

    Ok(())
}

// =========================================================================
// helpers
// =========================================================================

fn hex_short(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    use std::fmt::Write;
    for b in &bytes[..4] {
        let _ = write!(s, "{b:02x}");
    }
    s
}
