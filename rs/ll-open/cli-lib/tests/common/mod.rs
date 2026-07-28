//! Shared test scaffolding for `leyline-cli-lib` integration tests.
//!
//! ## Why this module exists
//!
//! `DaemonContext` has feature-gated fields (`vec_index`, `embedder`,
//! `embed_queue`, `text_search`), and Rust will not let you initialise a field
//! that does not exist. So every test that builds the struct literal had to
//! repeat the `#[cfg(feature = ...)]` shape itself — production's conditional
//! shape duplicated across every construction site. That is what the
//! `cfg_feature_in_tests` smell rule flags, and its description names this
//! module as the fix: *"a constructor or builder that owns the cfg ONCE —
//! `tests/common/mod.rs` with a `daemon_context(..)` helper — not a cfg per
//! call site."*
//!
//! The cfg lives here, once. Callers get a context and never see it.

#![allow(dead_code)] // each test binary uses a different subset

use std::path::Path;
use std::sync::Arc;

use leyline_cli_lib::daemon::{
    DaemonContext, DaemonState, EventRouter, NoExt, sheaf_ops::SheafState,
};
use leyline_core::{Controller, create_arena};

/// A minimal daemon context: controller + sheaf state, no reparse, no source
/// dir. `dir` should be a `TempDir` path the caller keeps alive.
pub fn daemon_context(dir: &Path) -> Arc<DaemonContext> {
    use parking_lot::{Mutex, RwLock};

    let arena_path = dir.join("test.arena");
    let ctrl_path = dir.join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).expect("create arena");
    let mut ctrl = Controller::open_or_create(&ctrl_path).expect("open ctrl");
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
        .expect("set arena");
    drop(ctrl);

    let router = EventRouter::new(16);
    let sheaf = Arc::new(SheafState::new());
    sheaf.set_emitter(router.emitter());

    // File-backed WAL LiveDb — the pool needs a real file (bead
    // `ley-line-open-f0239d`).
    let live_db_path = ctrl_path.with_extension("live.db");
    let live_db = leyline_cli_lib::daemon::db_pool::LiveDb::open_fresh_for_test(&live_db_path);

    Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router,
        live_db,
        enrich_inflight: Arc::new(Mutex::new(std::collections::HashSet::new())),
        source_dir: None,
        lang_filter: None,
        enrichment_passes: vec![],
        state: Arc::new(RwLock::new(DaemonState::initializing())),
        #[cfg(feature = "vec")]
        vec_index: {
            leyline_cli_lib::daemon::vec_index::register_vec();
            Arc::new(leyline_cli_lib::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        },
        #[cfg(feature = "vec")]
        embedder: Arc::new(leyline_cli_lib::daemon::embed::ZeroEmbedder { dim: 4 }),
        #[cfg(feature = "vec")]
        embed_queue: Arc::new(Mutex::new(std::collections::BinaryHeap::new())),
        #[cfg(feature = "text-search")]
        text_search: Arc::new(leyline_text_search::null::NullEngine::new()),
        sheaf,
    })
}

/// Block until `path` accepts a connection, or fail after `attempts`.
///
/// Replaces a fixed `thread::sleep` after spawning a listener. A wall-clock
/// sleep as a stop condition is a race against the scheduler: under load the
/// machine loses and the test fails with nothing broken (`sleep_in_tests`).
/// Polling the actual condition is both faster in the common case and cannot
/// flake — it waits exactly as long as the listener takes.
pub fn wait_for_uds(path: &Path, attempts: u32) -> std::os::unix::net::UnixStream {
    for _ in 0..attempts {
        if let Ok(s) = std::os::unix::net::UnixStream::connect(path) {
            return s;
        }
        std::thread::yield_now();
    }
    panic!(
        "socket at {} never accepted a connection after {attempts} attempts",
        path.display(),
    );
}
