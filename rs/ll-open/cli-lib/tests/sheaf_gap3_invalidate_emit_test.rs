//! Sheaf gap 3 (bead `ley-line-open-3b3476`) — proves the watcher path
//! emits `daemon.sheaf.invalidate` after a successful enrichment cycle.
//!
//! Prior state (audited in `docs/audits/sheaf-invalidation-trace.md`
//! link 5): the daemon only emitted `sheaf.invalidate` from the
//! consumer-driven `op_sheaf_invalidate` handler — nothing in the
//! source-change path told mache (or any other consumer) to evict
//! region caches. The "sheaves as the moat" claim depended on the
//! consumer closing the loop themselves.
//!
//! This test pins the wire. Regression it catches:
//!
//! * `emit_watcher_sheaf_invalidate` publishes on the
//!   `daemon.sheaf.invalidate` topic after `daemon.enrichment.complete`.
//!   Falsified by a router subscribe that must observe both topics in
//!   order within a 5-second budget.
//! * The payload carries `region_ids`, `changed_files`, `current_root`,
//!   `generation`, and `timestamp_ms` — the load-bearing fields the
//!   audit's Gap 3 recommendation named. Falsified by absence of any
//!   field.
//! * The `generation` field advances strictly (post > prior) because
//!   the emit bumps the SheafCache generation counter. Falsified by a
//!   refactor that dropped the `bump_generation()` call.
//! * On the failure path (a pass returns Err) NO `daemon.sheaf.invalidate`
//!   fires — a false-signal emit on a broken cycle would tell consumers
//!   to evict when nothing has actually changed.
//!
//! The tests bypass the git-polling front-half of `git_watch_loop` and
//! call the seam (`run_watcher_enrichment`) directly, matching the
//! gap 1 test's structure — the git subprocess + tokio task lifecycle
//! is separately covered by the audit and by `run_watcher_enrichment`
//! being invoked from the watcher after a successful reparse.

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

/// Mock enrichment pass that increments an `AtomicUsize` per call.
/// Mirrors the gap 1 test's `CountingPass` but lives here so the two
/// tests don't share fixtures (loose coupling — each test suite
/// answers a single question).
struct CountingPass {
    name: &'static str,
    invocations: Arc<AtomicUsize>,
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
        Ok(EnrichmentStats {
            pass_name: self.name.to_string(),
            files_processed: changed_files.map(|s| s.len() as u64).unwrap_or(0),
            items_added: 0,
            duration_ms: 0,
            skipped: Vec::new(),
        })
    }
}

/// Mock pass that always errors — used to prove the sheaf-invalidate
/// emit is gated on enrichment success.
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
        anyhow::bail!("intentional gap-3 test enrichment failure");
    }
}

/// Fresh arena + controller. Matches the shape used by the gap 1 test
/// so `read_root_hex` finds a well-formed controller and returns a
/// deterministic hex root (all-zeros sentinel for a fresh controller
/// per `ops::read_root_hex_is_zero_sentinel_for_fresh_controller`).
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

/// Build a minimal DaemonContext with the given enrichment passes and
/// an optional pre-installed sheaf complex. Returning the router lets
/// tests subscribe to whichever topics they care about.
fn build_ctx(
    dir: &Path,
    source_dir: PathBuf,
    passes: Vec<Box<dyn EnrichmentPass>>,
    seed_regions: &[u32],
) -> (Arc<DaemonContext>, Arc<EventRouter>) {
    use std::sync::{Mutex, RwLock};

    let ctrl_path = fresh_arena(dir);
    let router = EventRouter::new(64);
    let sheaf = Arc::new(leyline_cli_lib::daemon::sheaf_ops::SheafState::new());
    sheaf.set_emitter(router.emitter());

    // Seed the SheafState with a CellComplex whose node IDs the emit
    // helper will surface as `region_ids`. Without this the watcher-
    // driven invalidate would fire with an empty region set — still a
    // valid contract, but not the "current-topology → coarse cascade"
    // shape the gap-3 v1 payload documents. Tests that want the
    // "no complex installed yet" case pass an empty slice.
    if !seed_regions.is_empty() {
        use leyline_sheaf::complex::CellComplex;
        let mut cx = CellComplex::new(1);
        for &rid in seed_regions {
            cx.add_node(rid, vec![0.0]);
        }
        let tracker = leyline_sheaf::CoChangeTracker::default();
        sheaf.install_complex(cx, tracker);
    }

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

/// Await the next event on a router subscribe channel with a timeout.
/// Turns a wedged bus into a clean test failure.
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

/// Success path: after `run_watcher_enrichment` returns, the watcher-
/// driven `daemon.sheaf.invalidate` lands on the bus with a payload
/// that names region IDs, changed files, the current substrate root,
/// a bumped generation, and a timestamp.
///
/// This is the load-bearing "moat is now driven from LLO, not from
/// mache-polling" pin. If this test breaks, the gap 3 wire is severed
/// and every downstream claim about "region-precise invalidation
/// inside the daemon" is again paper.
#[tokio::test]
async fn watcher_emits_sheaf_invalidate_with_region_payload() {
    let dir = TempDir::new().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("foo.rs"), b"fn main() {}\n").unwrap();

    // Two seeded region IDs — the emit must surface both. The
    // specific IDs (7 and 42) are arbitrary; the pin is that whatever
    // was in the complex when the emit fired is what the consumer
    // sees on the wire.
    let seed_regions = vec![7u32, 42u32];

    let invocations = Arc::new(AtomicUsize::new(0));
    let pass = Box::new(CountingPass {
        name: "counting",
        invocations: invocations.clone(),
    });

    let (ctx, router) = build_ctx(dir.path(), source_dir.clone(), vec![pass], &seed_regions);

    // Subscribe to both topics so we can pin the ordering
    // (enrichment.complete before sheaf.invalidate) and the payload
    // shape on one bus.
    let (_sub_id, mut rx, _replay, _gap) = router
        .subscribe(
            &[
                "daemon.enrichment.*".to_string(),
                "daemon.sheaf.*".to_string(),
            ],
            None,
            0,
            OverflowPolicy::DropOldest,
            16,
        )
        .await;

    let emitter = router.emitter();
    let changed = vec!["foo.rs".to_string()];

    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(&ctx, &source_dir, &changed, &emitter);

    // Pin the enrichment happened (defence against a refactor that
    // silently drops the pipeline call — same guard as gap 1).
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "watcher-driven enrichment must invoke the registered pass"
    );

    // Pin ordering: enrichment.complete fires first, sheaf.invalidate
    // second. Consumers subscribed to both use this ordering to know
    // "enrichment finished writing → now evict caches".
    let first = recv_event(&mut rx, Duration::from_secs(5), "first watcher event").await;
    assert_eq!(
        first.topic, "daemon.enrichment.complete",
        "first event on the wire must be daemon.enrichment.complete \
         (gap 1 emit); got {:?}",
        first.topic
    );

    let second = recv_event(&mut rx, Duration::from_secs(5), "daemon.sheaf.invalidate").await;
    assert_eq!(
        second.topic, "daemon.sheaf.invalidate",
        "second event on the wire must be daemon.sheaf.invalidate \
         (gap 3 emit); got {:?}",
        second.topic
    );

    let payload = &second.data;

    // Pin `region_ids`: the emit surfaces the current CellComplex's
    // node IDs. Order-agnostic since the complex's `nodes` field is a
    // `Vec<u32>` populated in insertion order — the assertion sorts
    // both sides to avoid coupling to insertion order.
    let mut got_regions: Vec<u32> = payload
        .get("region_ids")
        .expect("payload must include `region_ids`")
        .as_array()
        .expect("`region_ids` must be a JSON array")
        .iter()
        .map(|v| v.as_u64().expect("region id must be a u64") as u32)
        .collect();
    got_regions.sort();
    let mut want_regions = seed_regions.clone();
    want_regions.sort();
    assert_eq!(
        got_regions, want_regions,
        "`region_ids` must equal the seeded complex's node set"
    );

    // Pin `count` mirrors `region_ids.len()` — matches the existing
    // `sheaf.invalidate` payload convention so subscribers use one
    // parse path.
    assert_eq!(
        payload
            .get("count")
            .and_then(|v| v.as_u64())
            .expect("`count` must be a u64"),
        seed_regions.len() as u64,
        "`count` must mirror region_ids length"
    );

    // Pin `scope` sentinel — V1 emits `"all-known"` because the
    // daemon lacks a file→region map. A refactor that flips to
    // `"diff"` MUST also implement the diff — this catches an
    // accidental mode-string change without behavior.
    assert_eq!(
        payload
            .get("scope")
            .and_then(|v| v.as_str())
            .expect("`scope` must be a string"),
        "all-known",
        "V1 scope sentinel must be \"all-known\""
    );

    // Pin `changed_files` echoes the scope the caller passed in.
    let files_arr = payload
        .get("changed_files")
        .expect("payload must include `changed_files`")
        .as_array()
        .expect("`changed_files` must be a JSON array");
    assert_eq!(files_arr.len(), 1, "one changed file expected");
    assert_eq!(
        files_arr[0].as_str(),
        Some("foo.rs"),
        "`changed_files` must round-trip the input"
    );

    // Pin `current_root` is present and hex-shaped. The fresh
    // controller returns the all-zeros sentinel per
    // `read_root_hex_is_zero_sentinel_for_fresh_controller` in
    // ops.rs. Anything non-empty proves the wire reached
    // `read_root_hex` successfully.
    let root_hex = payload
        .get("current_root")
        .and_then(|v| v.as_str())
        .expect("`current_root` must be a string");
    assert_eq!(
        root_hex.len(),
        64,
        "`current_root` must be a 64-char hex string; got {} chars: {root_hex:?}",
        root_hex.len()
    );
    assert!(
        root_hex.chars().all(|c| c.is_ascii_hexdigit()),
        "`current_root` must be lowercase hex; got {root_hex:?}"
    );

    // Pin generation advance: `bump_generation()` must move the
    // counter strictly forward. Quoted-string encoding matches the
    // capnp_json convention documented on `op_sheaf_invalidate`.
    let prior: u64 = payload
        .get("prior_generation")
        .and_then(|v| v.as_str())
        .expect("`prior_generation` must be a quoted string")
        .parse()
        .expect("`prior_generation` must parse as u64");
    let generation: u64 = payload
        .get("generation")
        .and_then(|v| v.as_str())
        .expect("`generation` must be a quoted string")
        .parse()
        .expect("`generation` must parse as u64");
    assert!(
        generation > prior,
        "watcher-driven emit must strictly advance the cache generation \
         (prior={prior}, generation={generation}) — bump_generation() is \
         the invariant that keeps monotonic-counter consumers correct \
         across watcher-driven + consumer-driven invalidates"
    );

    // Pin timestamp: quoted i64, plausible (positive) millis-since-
    // epoch. Loose bound — just prove it exists and parses.
    let ts: i64 = payload
        .get("timestamp_ms")
        .and_then(|v| v.as_str())
        .expect("`timestamp_ms` must be a quoted string")
        .parse()
        .expect("`timestamp_ms` must parse as i64");
    assert!(ts > 0, "`timestamp_ms` must be positive; got {ts}");
}

/// No-complex path: when the SheafState has no CellComplex installed
/// (e.g. daemon just started, no enrichment cycle has bumped the
/// state yet) the emit still fires — with an empty `region_ids`
/// array. Consumers use the event as a "state advanced" continuity
/// signal in this case.
#[tokio::test]
async fn watcher_emits_sheaf_invalidate_even_with_empty_topology() {
    let dir = TempDir::new().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).unwrap();

    let invocations = Arc::new(AtomicUsize::new(0));
    let pass = Box::new(CountingPass {
        name: "counting",
        invocations: invocations.clone(),
    });

    // No seeded regions — SheafState.cache().complex() returns None.
    let (ctx, router) = build_ctx(dir.path(), source_dir.clone(), vec![pass], &[]);

    let (_sub_id, mut rx, _replay, _gap) = router
        .subscribe(
            &["daemon.sheaf.*".to_string()],
            None,
            0,
            OverflowPolicy::DropOldest,
            16,
        )
        .await;

    let emitter = router.emitter();
    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(
        &ctx,
        &source_dir,
        &["foo.rs".to_string()],
        &emitter,
    );

    let evt = recv_event(
        &mut rx,
        Duration::from_secs(5),
        "daemon.sheaf.invalidate (empty-topology case)",
    )
    .await;
    assert_eq!(evt.topic, "daemon.sheaf.invalidate");

    let regions = evt
        .data
        .get("region_ids")
        .expect("payload must include `region_ids` even when empty")
        .as_array()
        .expect("`region_ids` must be an array");
    assert!(
        regions.is_empty(),
        "no-complex daemon must emit empty region_ids"
    );

    // `count` should mirror the empty array.
    assert_eq!(
        evt.data.get("count").and_then(|v| v.as_u64()),
        Some(0),
        "count must mirror region_ids.len() = 0"
    );

    // Generation still advances — the emit's contract is "every fire
    // moves the counter" regardless of topology size, so consumers
    // relying on monotonic-generation-as-freshness stay correct.
    let prior: u64 = evt.data["prior_generation"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let generation: u64 = evt.data["generation"].as_str().unwrap().parse().unwrap();
    assert!(
        generation > prior,
        "empty-topology emit must still bump generation ({prior} → {generation})"
    );
}

/// Failure path: a pass that returns Err triggers
/// `daemon.enrichment.failed` and does NOT emit
/// `daemon.sheaf.invalidate`. The sheaf state is unchanged from the
/// pre-enrichment view, so a "please evict" signal would be a false
/// positive.
#[tokio::test]
async fn watcher_does_not_emit_sheaf_invalidate_on_enrichment_failure() {
    let dir = TempDir::new().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).unwrap();

    let invocations = Arc::new(AtomicUsize::new(0));
    let pass = Box::new(FailingPass {
        invocations: invocations.clone(),
    });

    let (ctx, router) = build_ctx(dir.path(), source_dir.clone(), vec![pass], &[7u32]);

    let (_sub_id, mut rx, _replay, _gap) = router
        .subscribe(
            &[
                "daemon.enrichment.*".to_string(),
                "daemon.sheaf.*".to_string(),
            ],
            None,
            0,
            OverflowPolicy::DropOldest,
            16,
        )
        .await;

    let emitter = router.emitter();
    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(
        &ctx,
        &source_dir,
        &["foo.rs".to_string()],
        &emitter,
    );

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "failing pass still gets invoked once before its Err propagates"
    );

    // The next event must be `.failed`, not `.complete` and not
    // `.invalidate`. Any topic other than `.failed` first is a bug
    // (`.invalidate` would mean the emit fired despite failure —
    // exactly what this test guards against).
    let evt = recv_event(&mut rx, Duration::from_secs(5), "daemon.enrichment.failed").await;
    assert_eq!(
        evt.topic, "daemon.enrichment.failed",
        "failing pass must produce `daemon.enrichment.failed` first; got {:?}",
        evt.topic
    );

    // A brief pause window: if the emit is going to fire (bug), it
    // fires synchronously inside `run_watcher_enrichment`. We already
    // returned from that call, so any pending invalidate event is
    // already on the bus. Poll with a short deadline — a `None`
    // (channel empty within budget) is the pass condition.
    let poll = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
    match poll {
        Err(_) => { /* timeout — no more events; test passes */ }
        Ok(None) => { /* channel closed — also fine */ }
        Ok(Some(spurious)) => {
            panic!(
                "unexpected event after `daemon.enrichment.failed`: {:?}. \
                 The failure path MUST NOT emit `daemon.sheaf.invalidate` — \
                 doing so would tell consumers to evict caches when the \
                 sheaf state is unchanged.",
                spurious.topic
            );
        }
    }
}
