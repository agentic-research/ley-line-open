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
    build_ctx_with_labels(dir, source_dir, passes, seed_regions, None)
}

/// Same as [`build_ctx`] but also installs a `region_id → token label`
/// map into the SheafState. Load-bearing for the fine-grained-diff
/// tests (sheaf gap 3 follow-up, bead `ley-line-open-e40566`): with
/// labels installed, `emit_watcher_sheaf_invalidate` computes
/// `region_ids` as the subset touched by `changed_files` and emits
/// `scope: "changed-only"` instead of the coarse `"all-known"`
/// fallback. Passing `None` for labels reproduces the original
/// `build_ctx` shape.
fn build_ctx_with_labels(
    dir: &Path,
    source_dir: PathBuf,
    passes: Vec<Box<dyn EnrichmentPass>>,
    seed_regions: &[u32],
    seed_labels: Option<std::collections::HashMap<u32, String>>,
) -> (Arc<DaemonContext>, Arc<EventRouter>) {
    use parking_lot::{Mutex, RwLock};

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
    // Labels install must land AFTER `install_complex` because that
    // call clears any prior labels as part of the fresh-topology
    // contract (see SheafState::install_complex docs).
    if let Some(labels) = seed_labels {
        sheaf.install_region_labels(labels);
    }

    // File-backed WAL live db — pool needs a real file (bead
    // `ley-line-open-f0239d`).
    let live_db_path = ctrl_path.with_extension("live.db");
    let writer = Connection::open(&live_db_path).unwrap();
    let mode: String = writer
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    writer
        .execute_batch("CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT);")
        .unwrap();
    let live_db = leyline_cli_lib::daemon::db_pool::LiveDb::new(writer, &live_db_path, 4).unwrap();

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: router.clone(),
        live_db,
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
        .get("invalidated")
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

    // Pin `scope` sentinel — this fixture installs a complex but NOT
    // a region-label map, so `emit_watcher_sheaf_invalidate` falls
    // back to the coarse-v1 `"all-known"` path per
    // `SheafState::regions_touching_files` returning `None`. Sheaf
    // gap 3 follow-up (bead `ley-line-open-e40566`) pins this
    // fallback: when a mache-pushed topology (which never carries
    // labels) is active, or when the daemon hasn't yet run an
    // enrichment pass that installs labels, the emit MUST NOT lie
    // and claim to have computed a diff. A refactor that flipped
    // this path to `"changed-only"` without also computing the
    // subset would silently regress consumers.
    assert_eq!(
        payload
            .get("scope")
            .and_then(|v| v.as_str())
            .expect("`scope` must be a string"),
        "all-known",
        "no-labels fixture must fall back to \"all-known\""
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
        .get("invalidated")
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

// ─────────────────────────────────────────────────────────────────────
// Sheaf gap 3 follow-up (bead `ley-line-open-e40566`): fine-grained
// region diff. The three tests below prove the `scope: "changed-only"`
// path — when the daemon has labels installed, the payload's
// `region_ids` MUST contain only the regions actually touched by
// `changed_files`, NOT the full topology.
// ─────────────────────────────────────────────────────────────────────

/// Fine-grained happy path: three-region topology, one file changes,
/// and the emit surfaces only the region whose label points at that
/// file. Load-bearing regression pin for bead `ley-line-open-e40566`:
/// a refactor that dropped the label install, or one that emitted
/// every-known-region without consulting the labels, would fail this
/// assertion.
#[tokio::test]
async fn watcher_emits_fine_grained_region_ids_when_labels_are_installed() {
    let dir = TempDir::new().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).unwrap();

    // Three regions in the topology (IDs 10, 20, 30). Labels tie:
    //   region 10 ⇢ "src/foo.rs"
    //   region 20 ⇢ "src/foo.rs:sym:frobnicate"  (path:sym:NAME token)
    //   region 30 ⇢ "src/bar.rs"
    // Changing only `src/foo.rs` MUST invalidate {10, 20} — the
    // path-prefixed sym token and the bare path token both share the
    // file prefix. Region 30 lives in a different file and stays out
    // of the diff.
    let seed_regions = vec![10u32, 20u32, 30u32];
    let mut labels: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    labels.insert(10, "src/foo.rs".to_string());
    labels.insert(20, "src/foo.rs:sym:frobnicate".to_string());
    labels.insert(30, "src/bar.rs".to_string());

    let invocations = Arc::new(AtomicUsize::new(0));
    let pass = Box::new(CountingPass {
        name: "counting",
        invocations,
    });

    let (ctx, router) = build_ctx_with_labels(
        dir.path(),
        source_dir.clone(),
        vec![pass],
        &seed_regions,
        Some(labels),
    );

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
    let changed = vec!["src/foo.rs".to_string()];

    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(&ctx, &source_dir, &changed, &emitter);

    let evt = recv_event(
        &mut rx,
        Duration::from_secs(5),
        "daemon.sheaf.invalidate (fine-grained)",
    )
    .await;
    assert_eq!(evt.topic, "daemon.sheaf.invalidate");
    let payload = &evt.data;

    // Load-bearing assertion #1: `scope` must be `"changed-only"` —
    // this is the wire signal that consumers use to trust the
    // subsetting. A refactor that computed the diff but forgot to
    // flip the scope tag would leave mache treating the payload
    // like an `all-known` (safe over-eviction) even though the
    // daemon shipped a precise diff.
    assert_eq!(
        payload
            .get("scope")
            .and_then(|v| v.as_str())
            .expect("`scope` must be a string"),
        "changed-only",
        "labels installed + touched-file diff computed → scope must be `changed-only`"
    );

    // Load-bearing assertion #2: `region_ids` must be exactly the
    // subset the labels resolved — NOT the full seed set. This is
    // the payload's raison d'être. If the emit fell back to
    // `cx.nodes.clone()` (all-known) it would carry {10, 20, 30};
    // the labels resolver correctly restricts to {10, 20}.
    let mut got: Vec<u32> = payload
        .get("invalidated")
        .expect("payload must include `region_ids`")
        .as_array()
        .expect("`region_ids` must be a JSON array")
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![10u32, 20u32],
        "`region_ids` must be the fine-grained diff (regions labelled \
         with `src/foo.rs` or `src/foo.rs:sym:*`), NOT the full \
         seeded topology {{10, 20, 30}}"
    );

    // Count mirrors region_ids.len() — invariant across both scope modes.
    assert_eq!(
        payload.get("count").and_then(|v| v.as_u64()),
        Some(2),
        "count must mirror region_ids.len()=2"
    );

    // Generation still advances even in fine-grained mode —
    // consumers' monotonic-freshness cursors depend on this.
    let prior: u64 = payload["prior_generation"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let generation: u64 = payload["generation"].as_str().unwrap().parse().unwrap();
    assert!(
        generation > prior,
        "fine-grained emit must still bump generation ({prior} → {generation})"
    );
}

/// Fine-grained empty-diff path: labels installed, but the changed
/// file matches no region label. The emit MUST fire with `scope:
/// "changed-only"` and empty `region_ids` — that's the honest
/// "nothing structural touched" signal, and it's what keeps mache
/// from over-evicting on a doc/README edit that doesn't touch any
/// projected region.
///
/// Falsifiability: a refactor that decided "empty diff = fall back to
/// all-known" would break the "daemon reports what it knows" contract
/// this test pins.
#[tokio::test]
async fn watcher_emits_changed_only_with_empty_region_ids_when_no_labels_match() {
    let dir = TempDir::new().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).unwrap();

    let seed_regions = vec![10u32, 20u32];
    let mut labels: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    labels.insert(10, "src/foo.rs".to_string());
    labels.insert(20, "src/bar.rs".to_string());

    let invocations = Arc::new(AtomicUsize::new(0));
    let pass = Box::new(CountingPass {
        name: "counting",
        invocations,
    });

    let (ctx, router) = build_ctx_with_labels(
        dir.path(),
        source_dir.clone(),
        vec![pass],
        &seed_regions,
        Some(labels),
    );

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
    // A file that no label references — the diff should be empty.
    let changed = vec!["docs/README.md".to_string()];

    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(&ctx, &source_dir, &changed, &emitter);

    let evt = recv_event(
        &mut rx,
        Duration::from_secs(5),
        "daemon.sheaf.invalidate (empty-diff)",
    )
    .await;
    assert_eq!(evt.topic, "daemon.sheaf.invalidate");
    let payload = &evt.data;

    // Scope stays `changed-only` — labels ARE installed, we just
    // computed an empty subset. Consumers get the accurate
    // "nothing to evict" signal.
    assert_eq!(
        payload
            .get("scope")
            .and_then(|v| v.as_str())
            .expect("`scope` must be a string"),
        "changed-only",
        "empty diff with labels installed must NOT fall back to all-known"
    );
    let regions = payload
        .get("invalidated")
        .expect("payload must include `region_ids`")
        .as_array()
        .expect("`region_ids` must be a JSON array");
    assert!(
        regions.is_empty(),
        "empty diff must emit empty region_ids; got {regions:?}"
    );

    // Generation still bumps even when nothing was touched — the
    // event's contract is "state advanced by one tick" regardless
    // of whether any region moved.
    let prior: u64 = payload["prior_generation"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let generation: u64 = payload["generation"].as_str().unwrap().parse().unwrap();
    assert!(
        generation > prior,
        "empty-diff emit must still bump generation ({prior} → {generation})"
    );
}

/// Backward-compat pin: when a complex is installed but NO labels are
/// (e.g. mache pushed the topology via `op_sheaf_set_topology`, or the
/// daemon just started and `ComplexBuildPass` hasn't run yet), the
/// emit MUST fall back to the coarse-v1 `scope: "all-known"` shape
/// with every known region ID.
///
/// This is the guarantee that keeps consumers whose region-ID space is
/// consumer-owned (mache Louvain) working during the transition — they
/// can't be broken by an accidental fine-grained mode that would try
/// to match daemon-owned labels against consumer-side region IDs.
#[tokio::test]
async fn watcher_falls_back_to_all_known_when_labels_not_installed() {
    let dir = TempDir::new().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).unwrap();

    // Complex installed, but no labels — matches the mache-pushed
    // topology case.
    let seed_regions = vec![10u32, 20u32, 30u32];

    let invocations = Arc::new(AtomicUsize::new(0));
    let pass = Box::new(CountingPass {
        name: "counting",
        invocations,
    });

    let (ctx, router) = build_ctx_with_labels(
        dir.path(),
        source_dir.clone(),
        vec![pass],
        &seed_regions,
        None, // no labels — forces the fallback path
    );

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
    // Even naming a "real" changed file doesn't help — without
    // labels, the daemon can't diff, so it emits the coarse cascade.
    let changed = vec!["src/foo.rs".to_string()];

    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(&ctx, &source_dir, &changed, &emitter);

    let evt = recv_event(
        &mut rx,
        Duration::from_secs(5),
        "daemon.sheaf.invalidate (fallback)",
    )
    .await;
    let payload = &evt.data;

    // Scope tag: coarse fallback.
    assert_eq!(
        payload
            .get("scope")
            .and_then(|v| v.as_str())
            .expect("`scope` must be a string"),
        "all-known",
        "no labels installed → must fall back to coarse `all-known`"
    );
    // Region set: EVERY seeded region, because the daemon can't
    // diff and safety demands over-eviction.
    let mut got: Vec<u32> = payload
        .get("invalidated")
        .expect("payload must include `region_ids`")
        .as_array()
        .expect("`region_ids` must be a JSON array")
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    got.sort();
    let mut want = seed_regions.clone();
    want.sort();
    assert_eq!(
        got, want,
        "coarse fallback must surface every known region ID"
    );
}
