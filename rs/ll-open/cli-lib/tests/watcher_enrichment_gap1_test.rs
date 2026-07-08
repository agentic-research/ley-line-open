//! Sheaf gap 1 (bead `ley-line-open-3ab7db`) — proves the watcher path
//! drives the enrichment pipeline.
//!
//! Prior state (audited in `docs/audits/sheaf-invalidation-trace.md`
//! link 3): `git_watch_loop` ran a scoped `parse_into_conn` and
//! stopped. Enrichment (HDC re-encode, complex-build, LSP) fired only
//! on consumer-invoked `op_enrich`. The sheaf-driven cascade the moat
//! rests on was consumer-owned, not source-change-owned.
//!
//! This test pins the wire. The regression it catches:
//!
//! * The `run_watcher_enrichment` helper (invoked by `git_watch_loop`
//!   after `daemon.reparse.complete`) actually calls the registered
//!   enrichment passes — falsified by an `AtomicUsize`-counting mock
//!   pass whose count MUST advance by exactly one per invocation.
//! * The helper emits `daemon.enrichment.complete` on the router bus
//!   after the pipeline succeeds — falsified by a router subscribe
//!   that must observe the topic within a 5-second budget.
//! * The helper emits `daemon.enrichment.failed` (not `.complete`)
//!   when a pass errors, and it does not crash the watcher — falsified
//!   by a mock pass that always returns Err.
//!
//! The tests bypass the git-polling front-half of `git_watch_loop`
//! (git subprocess + filesystem race + tokio task lifecycle are noisy
//! to drive from a black-box test) and call the seam directly. The
//! wire we care about is "after reparse succeeds, does enrichment
//! fire and emit its completion event?" — that's what these tests
//! answer. The upstream half is separately covered by the audit and
//! by the existing `daemon.reparse.complete` emission at
//! `cmd_daemon.rs:1069`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use leyline_cli_lib::daemon::enrichment::{EnrichmentPass, EnrichmentStats};
use leyline_cli_lib::daemon::events::OverflowPolicy;
use leyline_cli_lib::daemon::{DaemonContext, DaemonState, EventRouter, NoExt};
use rusqlite::Connection;
use tempfile::TempDir;

/// Mock enrichment pass that increments an `AtomicUsize` on every
/// `run` invocation and echoes the `changed_files` slice back so tests
/// can assert scoping too.
struct CountingPass {
    name: &'static str,
    invocations: Arc<AtomicUsize>,
    last_scope: Arc<std::sync::Mutex<Option<Vec<String>>>>,
}

impl EnrichmentPass for CountingPass {
    fn name(&self) -> &str {
        self.name
    }

    fn reads(&self) -> &[&str] {
        &[]
    }

    fn writes(&self) -> &[&str] {
        &[]
    }

    fn run(
        &self,
        _conn: &Connection,
        _source_dir: &Path,
        changed_files: Option<&[String]>,
    ) -> Result<EnrichmentStats> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        *self.last_scope.lock().unwrap() = changed_files.map(|s| s.to_vec());
        Ok(EnrichmentStats {
            pass_name: self.name.to_string(),
            files_processed: changed_files.map(|s| s.len() as u64).unwrap_or(0),
            items_added: 0,
            duration_ms: 0,
            skipped: Vec::new(),
        })
    }
}

/// Mock pass that always errors — used to prove the `.failed` path
/// fires without crashing the watcher.
struct FailingPass {
    invocations: Arc<AtomicUsize>,
}

impl EnrichmentPass for FailingPass {
    fn name(&self) -> &str {
        "failing"
    }
    fn reads(&self) -> &[&str] {
        &[]
    }
    fn writes(&self) -> &[&str] {
        &[]
    }
    fn run(
        &self,
        _conn: &Connection,
        _source_dir: &Path,
        _changed_files: Option<&[String]>,
    ) -> Result<EnrichmentStats> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("intentional watcher-enrichment failure");
    }
}

/// Fresh arena + controller, matches other tests in this suite.
fn fresh_arena(dir: &Path) -> PathBuf {
    use leyline_core::{Controller, create_arena};
    let arena_path = dir.join("test.arena");
    let ctrl_path = dir.join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).expect("create arena");
    let mut ctrl = Controller::open_or_create(&ctrl_path).expect("open ctrl");
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
        .expect("set arena");
    drop(ctrl);
    ctrl_path
}

/// Bring up a minimal DaemonContext with the given enrichment passes and
/// a source_dir pointing at a scratch directory. Enough to feed
/// `run_watcher_enrichment` its whole call surface.
fn build_ctx(
    dir: &Path,
    source_dir: PathBuf,
    passes: Vec<Box<dyn EnrichmentPass>>,
) -> (Arc<DaemonContext>, Arc<EventRouter>) {
    use std::sync::{Mutex, RwLock};

    let ctrl_path = fresh_arena(dir);
    let router = EventRouter::new(64);
    let sheaf = Arc::new(leyline_cli_lib::daemon::sheaf_ops::SheafState::new());
    sheaf.set_emitter(router.emitter());

    // Pre-create `_meta` — `execute_pass` inside `enrichment::run_all`
    // bumps `<pass>_version` in `_meta` after a successful pass. In
    // production the schema is initialized by `parse_into_conn`; in
    // this black-box test we skip that step, so we scaffold the table
    // ourselves. Without this the CountingPass's Ok(...) still causes
    // `run_all` to bail with "no such table: _meta", masquerading the
    // success path as a failure.
    let live_db = Connection::open_in_memory().unwrap();
    live_db
        .execute_batch("CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT);")
        .unwrap();

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: router.clone(),
        live_db: Mutex::new(live_db),
        enrich_inflight: Arc::new(Mutex::new(std::collections::HashSet::new())),
        source_dir: Some(source_dir),
        lang_filter: None,
        enrichment_passes: passes,
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
    });
    (ctx, router)
}

/// Await the next event on the router's subscribe channel with a
/// timeout — turns a wedged bus into a clean test failure instead of
/// hanging the suite.
async fn recv_event(
    rx: &mut tokio::sync::mpsc::Receiver<leyline_cli_lib::daemon::events::Event>,
    timeout: Duration,
    context: &str,
) -> leyline_cli_lib::daemon::events::Event {
    tokio::time::timeout(timeout, rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {context} (>{timeout:?})"))
        .unwrap_or_else(|| panic!("event channel closed while waiting for {context}"))
}

/// Successful path: after `run_watcher_enrichment` returns, the mock
/// pass has been invoked exactly once (scoped to the changed files) and
/// a `daemon.enrichment.complete` event has landed on the bus.
///
/// This is the core "watcher drives enrichment" contract for gap 1.
#[tokio::test]
async fn watcher_enrichment_fires_pass_and_emits_complete_event() {
    let dir = TempDir::new().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).unwrap();
    // Write a source file so the "modify one source file" premise is
    // observable to any pass that inspects the filesystem — even though
    // our CountingPass doesn't care, this makes the fixture honest to
    // the scenario the bead describes.
    std::fs::write(source_dir.join("foo.rs"), b"fn main() {}\n").unwrap();

    let invocations = Arc::new(AtomicUsize::new(0));
    let last_scope: Arc<std::sync::Mutex<Option<Vec<String>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let pass = Box::new(CountingPass {
        name: "counting",
        invocations: invocations.clone(),
        last_scope: last_scope.clone(),
    });

    let (ctx, router) = build_ctx(dir.path(), source_dir.clone(), vec![pass]);

    // Subscribe BEFORE invoking the helper so the completion event
    // travels the live-push path, not the replay batch.
    let (_sub_id, mut rx, _replay, _gap) = router
        .subscribe(
            &["daemon.enrichment.*".to_string()],
            None,
            0,
            OverflowPolicy::DropOldest,
            16,
        )
        .await;

    let emitter = router.emitter();
    let changed = vec!["foo.rs".to_string()];

    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(&ctx, &source_dir, &changed, &emitter);

    // Pin 1: the pipeline was actually invoked.
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "watcher-driven enrichment must invoke each registered pass exactly \
         once per run (gap 1 wire: reparse → enrichment)"
    );

    // Pin 2: it was invoked with the scoped changed_files, not None.
    // Regression against a refactor that accidentally drops the scope.
    let scope_seen = last_scope.lock().unwrap().clone();
    assert_eq!(
        scope_seen,
        Some(vec!["foo.rs".to_string()]),
        "watcher-driven enrichment must forward the changed_files scope \
         to each pass so incremental passes stay incremental"
    );

    // Pin 3: the daemon.enrichment.complete event lands on the bus.
    // Consumers subscribe to it as the "enrichment finished, safe to
    // query" signal — Gap 3 (bead ley-line-open-3b3476) hooks off it.
    let evt = recv_event(
        &mut rx,
        Duration::from_secs(5),
        "daemon.enrichment.complete",
    )
    .await;
    assert_eq!(
        evt.topic, "daemon.enrichment.complete",
        "watcher enrichment must emit daemon.enrichment.complete after \
         a successful pipeline run; got topic {:?}",
        evt.topic
    );

    // Pin 4: payload carries the changed_files so consumers can scope
    // their downstream work. Not asserting the full shape (deliberately
    // loose to let the payload evolve) but the field must exist.
    let changed_files_field = evt.data.get("changed_files").expect(
        "daemon.enrichment.complete payload must include `changed_files` so \
         consumers can scope their cascade (see gap 3 hook)",
    );
    let files_arr = changed_files_field
        .as_array()
        .expect("`changed_files` must be a JSON array");
    assert_eq!(
        files_arr.len(),
        1,
        "`changed_files` should carry the scope passed to the helper"
    );
    assert_eq!(
        files_arr[0].as_str(),
        Some("foo.rs"),
        "`changed_files` must contain the file we told the helper about"
    );

    // Pin 5: the passes array is present so consumers can inspect
    // per-pass timings for observability. Also loose — just require the
    // field.
    assert!(
        evt.data.get("passes").is_some(),
        "daemon.enrichment.complete must carry a `passes` array for \
         observability; missing entirely"
    );
}

/// Failure path: a pass that returns Err triggers `daemon.enrichment.failed`
/// and does NOT crash the watcher. The pipeline is best-effort by design
/// — reparse already wrote to the db, so a broken pass must not roll
/// back the reparse's progress.
#[tokio::test]
async fn watcher_enrichment_emits_failed_when_pass_errors() {
    let dir = TempDir::new().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).unwrap();

    let invocations = Arc::new(AtomicUsize::new(0));
    let pass = Box::new(FailingPass {
        invocations: invocations.clone(),
    });

    let (ctx, router) = build_ctx(dir.path(), source_dir.clone(), vec![pass]);

    let (_sub_id, mut rx, _replay, _gap) = router
        .subscribe(
            &["daemon.enrichment.*".to_string()],
            None,
            0,
            OverflowPolicy::DropOldest,
            16,
        )
        .await;

    let emitter = router.emitter();
    let changed = vec!["foo.rs".to_string()];

    // Must not panic — the whole contract of gap 1's error path is that
    // enrichment is best-effort and the watcher survives faults.
    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(&ctx, &source_dir, &changed, &emitter);

    // The pass was invoked once (even though it errored).
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "failing pass still gets invoked once before its Err propagates"
    );

    // The event bus received `.failed`, not `.complete`.
    let evt = recv_event(&mut rx, Duration::from_secs(5), "daemon.enrichment.failed").await;
    assert_eq!(
        evt.topic, "daemon.enrichment.failed",
        "failing pass must produce daemon.enrichment.failed, not \
         .complete; got topic {:?}",
        evt.topic
    );
    let err_msg = evt
        .data
        .get("error")
        .and_then(|v| v.as_str())
        .expect("failed payload must include error string");
    assert!(
        err_msg.contains("intentional watcher-enrichment failure"),
        "error payload must surface the pass's Err message so operators \
         can diagnose from event logs; got {err_msg:?}"
    );
}

/// Empty scope path: an empty `changed_files` still triggers the
/// pipeline (with a None scope, matching the run_all contract). This
/// guards against a refactor that early-returns when the scope is
/// empty — an empty dirty set after a HEAD change is a legitimate
/// signal to run enrichment across the repo.
#[tokio::test]
async fn watcher_enrichment_with_empty_scope_falls_back_to_full_run() {
    let dir = TempDir::new().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).unwrap();

    let invocations = Arc::new(AtomicUsize::new(0));
    let last_scope: Arc<std::sync::Mutex<Option<Vec<String>>>> =
        Arc::new(std::sync::Mutex::new(Some(vec!["placeholder".to_string()])));
    let pass = Box::new(CountingPass {
        name: "counting",
        invocations: invocations.clone(),
        last_scope: last_scope.clone(),
    });

    let (ctx, router) = build_ctx(dir.path(), source_dir.clone(), vec![pass]);
    let emitter = router.emitter();

    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(&ctx, &source_dir, &[], &emitter);

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "empty changed_files must still invoke the pipeline (falls back \
         to full-repo enrichment via a None scope)"
    );
    assert_eq!(
        *last_scope.lock().unwrap(),
        None,
        "empty changed_files must map to a None scope inside the pass \
         (not Some(&[])) — otherwise passes that key on Some vs None \
         mis-classify a HEAD-change-only tick as a targeted incremental"
    );
}
