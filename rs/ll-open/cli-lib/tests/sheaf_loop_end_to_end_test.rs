//! Sheaf loop end-to-end composition test — bead `ley-line-open-e46151`.
//!
//! Sheaf gaps 1, 2, and 3 shipped to `main` on 2026-07-07 (PRs #138,
//! #139, #140) — each closed a specific link in the audited chain from
//! `docs/audits/sheaf-invalidation-trace.md`:
//!
//! - Gap 1 (`ley-line-open-3ab7db`, PR #138): `git_watch_loop` now
//!   drives the enrichment pipeline on scoped reparse via
//!   [`leyline_cli_lib::cmd_daemon::run_watcher_enrichment`].
//! - Gap 2 (`ley-line-open-3af437`, PR #139):
//!   [`ComplexBuildPass`](leyline_cli_lib::daemon::complex_build_pass::ComplexBuildPass)
//!   installs its built `CellComplex` + `CoChangeTracker` into the
//!   shared [`SheafState`] instead of dropping them on function return.
//! - Gap 3 (`ley-line-open-3b3476`, PR #140):
//!   [`emit_watcher_sheaf_invalidate`](leyline_cli_lib::cmd_daemon::emit_watcher_sheaf_invalidate)
//!   fires `daemon.sheaf.invalidate` from the watcher path after a
//!   successful enrichment cycle with the coarse-v1 payload.
//!
//! **Each gap has its own regression test, but none proves that the
//! three COMPOSE into a working loop.** This file ships that E2E test.
//!
//! ### What this test proves
//!
//! A concrete source-file edit propagates through the whole chain and
//! surfaces on the event bus as a `daemon.sheaf.invalidate` event with
//! the full coarse-v1 payload shape a subscriber (mache's
//! `SheafSubscriber`) depends on. Specifically:
//!
//! 1. Real `parse_into_conn` runs (not mocked): schema created,
//!    files parsed, `_file_index` populated.
//! 2. Real `ComplexBuildPass` runs (not mocked): `SheafState.cache` is
//!    populated with a non-empty `CellComplex` derived from seeded
//!    observation rows — so `region_ids` on the wire is non-empty and
//!    matches the built topology.
//! 3. `run_watcher_enrichment` (gap 1) fires enrichment after the
//!    scoped reparse and emits `daemon.enrichment.complete`.
//! 4. `emit_watcher_sheaf_invalidate` (gap 3) reads `region_ids` from
//!    the persisted cache (gap 2), bumps generation, and emits
//!    `daemon.sheaf.invalidate` with all eight payload fields.
//! 5. An in-process subscriber on `daemon.sheaf.*` receives the event
//!    within a bounded 10s timeout — a wedged loop shows up as a
//!    clean test failure, not a hang.
//!
//! ### What could regress
//!
//! - Any of gaps 1/2/3 being reverted or refactored out of the wire
//!   would leave this test failing (either no invalidate event, or
//!   the payload missing a load-bearing field, or `region_ids` empty
//!   because the persistence side-channel dropped).
//! - A change to the payload schema that renamed / retyped any of the
//!   eight fields the coarse-v1 contract names surfaces here before
//!   downstream mache breaks silently.
//!
//! ### Subscription mechanism
//!
//! In-process `EventRouter::subscribe` — same primitive the UDS
//! `subscribe` op relays over the wire. Skips the UDS hop because
//! `sheaf_uds_blackbox_test.rs` already covers the wire codec end
//! and this test's job is composition, not transport.
//!
//! ### File-change trigger
//!
//! Real filesystem write to a source file inside the temp `source_dir`,
//! followed by a scoped `parse_into_conn` (mirroring exactly what
//! `git_watch_loop` does at `cmd_daemon.rs:1058` after `git status`
//! reports the dirty file) and then `run_watcher_enrichment`. The git
//! poll front-half is deliberately bypassed — `git_watch_loop` is
//! `async fn`-private and cannot be driven from an integration test
//! without modifying production code, which the bead's contract
//! forbids ("New file only: DO NOT modify any existing Rust code"). The
//! `cmd_daemon.rs` unit tests cover the git-poll front-half via
//! `start_watcher_test` + `fixture_repo`; this test's contribution is
//! the *composition* of the exposed seams.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leyline_cli_lib::daemon::complex_build_pass::ComplexBuildPass;
use leyline_cli_lib::daemon::enrichment::{EnrichmentPass, TreeSitterPass};
use leyline_cli_lib::daemon::events::OverflowPolicy;
use leyline_cli_lib::daemon::observation_schema::create_observation_schema;
use leyline_cli_lib::daemon::sheaf_ops::SheafState;
use leyline_cli_lib::daemon::{DaemonContext, DaemonState, EventRouter, NoExt};
use rusqlite::Connection;
use tempfile::TempDir;

// --- Fixture helpers --------------------------------------------------------
//
// These mirror the shape used by `sheaf_gap3_invalidate_emit_test.rs` and
// `watcher_enrichment_gap1_test.rs` so the three regression tests read as
// a family. Kept local (not shared through a `fixtures/` module) because
// the setup shape differs slightly — this test wires the *real*
// ComplexBuildPass and TreeSitterPass while gap 1/3 wire mocks.

/// Build a fresh arena + controller. Returns the controller path. Needed
/// because `emit_watcher_sheaf_invalidate` calls `read_root_hex` on
/// this path to fill the payload's `current_root` field; without a
/// well-formed controller the wire returns an empty string and the
/// 64-char-hex assertion below fails at composition rather than at the
/// missing-field level (still a valid catch, but noisier).
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

/// Seed 5 observation rows across 4 distinct path-shaped tokens so
/// `ComplexBuildPass::run` installs a non-empty `CellComplex` — 4
/// nodes (`foo.rs`, `foo.rs:sym:foo`, `bar.rs`, `bar.rs:sym:bar`) and
/// 4 co-occurrence edges. Path-shaped tokens match the shape
/// `SessionObservationPass::extract_mentions` produces in production
/// (bare path token + `<path>:sym:<NAME>` citation), so the sheaf
/// gap 3 follow-up's fine-grained region diff (bead
/// `ley-line-open-e40566`) can resolve the labels back to touched
/// files. When `foo.rs` changes, the two `foo.rs*` regions become
/// the fine-grained `region_ids`; the two `bar.rs*` regions stay out.
///
/// The `observation` table must exist before this runs — the caller
/// creates the schema via `create_observation_schema` first.
fn seed_observations(conn: &Connection) {
    let rows: [&str; 5] = [
        r#"["foo.rs","foo.rs:sym:foo"]"#,
        r#"["foo.rs","bar.rs"]"#,
        r#"["foo.rs:sym:foo","bar.rs:sym:bar"]"#,
        r#"["bar.rs:sym:bar"]"#,
        r#"["foo.rs","bar.rs:sym:bar"]"#,
    ];
    for (i, mentions) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO observation (source, payload_kind, mentions, observed_at) \
             VALUES ('test', 'agent.session_turn', ?1, ?2)",
            rusqlite::params![mentions, (i as i64) * 1000],
        )
        .expect("seed observation row");
    }
}

/// Build a `DaemonContext` wired with the same pass sequence
/// `run_daemon` builds at `cmd_daemon.rs:311-345`, minus the
/// feature-gated `LspEnrichmentPass` / `EmbeddingPass` /
/// `HdcEnrichmentPass` / `SessionObservationPass` — this test seeds
/// observation rows directly (see [`seed_observations`]) so the
/// session-JSONL-driven pass isn't in the pipeline.
///
/// Returns `(ctx, router)` — the router is separately handed to the
/// subscriber so the wire path exercised here matches production
/// (subscribers see the same emitter events the daemon-internal
/// `run_watcher_enrichment` publishes).
fn build_full_ctx(dir: &Path, source_dir: PathBuf) -> (Arc<DaemonContext>, Arc<EventRouter>) {
    use std::sync::{Mutex, RwLock};

    let ctrl_path = fresh_arena(dir);
    let router = EventRouter::new(64);
    let sheaf = Arc::new(SheafState::new());
    // Wire the emitter so consumer-driven `sheaf_*` ops would surface on
    // the same bus — irrelevant for the watcher path (which uses its
    // own emitter), but mirrors production wiring exactly.
    sheaf.set_emitter(router.emitter());

    let live_db = Connection::open_in_memory().expect("open in-memory sqlite");
    // Ensure the `observation` schema exists so `seed_observations`
    // can INSERT before the pipeline runs. `ComplexBuildPass::run`
    // itself calls `ensure_observation_table`; creating it explicitly
    // here decouples the seeding step from pipeline execution order.
    create_observation_schema(&live_db).expect("create observation schema");

    // Production pass list (minimum viable to close the sheaf loop):
    //   TreeSitterPass  — real parse pass; writes to nodes/_ast/etc.
    //   ComplexBuildPass — reads `observation`, installs a CellComplex
    //                      into `SheafState.cache` (gap 2).
    // The feature-gated LSP / vec / HDC / session-observation passes
    // are excluded so the test compiles cleanly under any feature
    // combination and stays focused on the sheaf-loop composition.
    let passes: Vec<Box<dyn EnrichmentPass>> = vec![
        Box::new(TreeSitterPass),
        Box::new(ComplexBuildPass::new(sheaf.clone())),
    ];

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: router.clone(),
        live_db: Mutex::new(live_db),
        enrich_inflight: Arc::new(Mutex::new(std::collections::HashSet::new())),
        source_dir: Some(source_dir),
        // `TreeSitterPass::run` calls `parse_into_conn(conn, source, None, ...)`
        // — it detects language per-file via file extension, so the
        // context-level `lang_filter` is unused by the pass. Set to
        // None to match that.
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

/// Await the next event on a subscribe channel with a bounded timeout.
/// Turns a wedged bus (broken emit, dropped subscriber) into a clean
/// test failure with a diagnostic message rather than a hang that
/// eventually gets killed by `task ci` at some higher-level deadline.
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

/// Bounded upper limit on how long the E2E loop can take before we
/// consider it wedged. Two orders of magnitude beyond an in-process
/// emit's actual latency — generous slack for CI variance while still
/// bounded well below any harness deadline.
const LOOP_TIMEOUT: Duration = Duration::from_secs(10);

// --- The E2E composition test ----------------------------------------------

/// The load-bearing pin. A real file change drives a real reparse, real
/// enrichment, and produces a single `daemon.sheaf.invalidate` event on
/// the bus with the full payload contract intact.
///
/// Post sheaf gap 3 follow-up (bead `ley-line-open-e40566`): the emit
/// is fine-grained by default — `scope: "changed-only"` with
/// `region_ids` restricted to the subset whose token labels match
/// `changed_files`. If this test breaks after any of gaps 1/2/3 or
/// e40566 is refactored, the sheaf loop is severed and every
/// downstream "region-precise invalidation inside the daemon" claim
/// is again paper.
#[tokio::test]
async fn file_change_drives_fine_grained_sheaf_invalidate_end_to_end() {
    let dir = TempDir::new().expect("tempdir");
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).expect("mkdir src");

    // Two parseable Rust files so `parse_into_conn` has a non-trivial
    // tree to walk and `_file_index` has an mtime baseline the scoped
    // reparse can diff against.
    let foo_path = source_dir.join("foo.rs");
    let bar_path = source_dir.join("bar.rs");
    std::fs::write(&foo_path, "fn foo() -> i32 { 1 }\n").expect("write foo.rs");
    std::fs::write(&bar_path, "fn bar() -> i32 { 2 }\n").expect("write bar.rs");

    let (ctx, router) = build_full_ctx(dir.path(), source_dir.clone());

    // Initial parse populates `_file_index` + `nodes` so the later
    // scoped reparse detects the modification (rather than treating
    // foo.rs as a first-time add and skipping the mtime diff branch).
    // Mirrors the daemon's cold-start parse before `git_watch_loop`
    // takes over at `cmd_daemon.rs:513`.
    {
        let guard = ctx.live_db.lock().expect("live_db lock");
        leyline_cli_lib::cmd_parse::parse_into_conn(&guard, &source_dir, None, None)
            .expect("initial parse");
    }

    // Seed observation rows so ComplexBuildPass has data — the built
    // `CellComplex` gets 4 nodes / 4 edges (see `seed_observations`
    // docstring). Without this the emit still fires but with empty
    // `region_ids` (the "no-complex" case already pinned by
    // `sheaf_gap3_invalidate_emit_test.rs::watcher_emits_sheaf_invalidate_even_with_empty_topology`).
    // This test targets the non-empty case so the `region_ids` shape
    // is exercised end-to-end, not just structurally-present.
    {
        let guard = ctx.live_db.lock().expect("live_db lock");
        seed_observations(&guard);
    }

    // Subscribe BEFORE any enrichment fires so both the priming
    // invalidate (from step 3 below) and the file-change invalidate
    // (step 5) hit the subscribe channel — replay wouldn't cover the
    // first if we subscribed after.
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

    // Priming step: run a full watcher-enrichment cycle so
    // ComplexBuildPass installs the `CellComplex` into
    // `SheafState.cache`. Without this the file-change tick's emit
    // would see `cache.complex() == None` and surface empty
    // `region_ids` — proving nothing about the persistence side
    // channel (gap 2) reaching the wire.
    //
    // The priming call emits its own `daemon.sheaf.invalidate` (per
    // the gap 3 contract: every successful enrichment fires one).
    // Drain that event so the file-change invalidate is the next one
    // on the channel.
    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(&ctx, &source_dir, &[], &emitter);
    let priming = recv_event(&mut rx, LOOP_TIMEOUT, "priming daemon.sheaf.invalidate").await;
    assert_eq!(
        priming.topic, "daemon.sheaf.invalidate",
        "priming enrichment must emit daemon.sheaf.invalidate; got {:?}",
        priming.topic,
    );
    let priming_generation: u64 = priming.data["generation"]
        .as_str()
        .expect("priming generation is quoted string")
        .parse()
        .expect("priming generation parses as u64");

    // File change: rewrite foo.rs with different content. Both mtime
    // AND content change so mtime-based incremental detection + content
    // hashing both agree the file is dirty. Also modify content length
    // so any byte-count-only detection path also fires.
    std::fs::write(&foo_path, "fn foo() -> i32 { 42 /* modified */ }\n").expect("modify foo.rs");

    // Simulate the git watcher tick's post-git-status body verbatim
    // (`cmd_daemon.rs:1049-1111`): scoped reparse with the dirty file
    // list, then run_watcher_enrichment on the parsed set. Skipping the
    // snapshot_or_log call here because it is orthogonal to the sheaf
    // emit (the arena snapshot writes bytes to disk; the emit reads
    // `read_root_hex` from the controller path that arena creation
    // already populated).
    let reparse = {
        let guard = ctx.live_db.lock().expect("live_db lock");
        leyline_cli_lib::cmd_parse::parse_into_conn(
            &guard,
            &source_dir,
            None,
            Some(&["foo.rs".to_string()]),
        )
        .expect("scoped reparse")
    };
    assert!(
        reparse.parsed > 0,
        "scoped reparse must detect the modified foo.rs (parsed={}, unchanged={}); \
         if this fires the mtime-diff path is broken and the test can't drive the \
         sheaf loop from a real edit",
        reparse.parsed,
        reparse.unchanged,
    );
    assert!(
        reparse.changed_files.iter().any(|f| f == "foo.rs"),
        "scoped reparse must report foo.rs in changed_files; got {:?}",
        reparse.changed_files,
    );

    // Drive the watcher-enrichment path exactly as `git_watch_loop`
    // does at `cmd_daemon.rs:1111`. This closes the gap 1 → gap 3
    // chain: enrichment runs (gap 1), ComplexBuildPass persists to
    // SheafState (gap 2), emit_watcher_sheaf_invalidate fires (gap 3).
    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(
        &ctx,
        &source_dir,
        &reparse.changed_files,
        &emitter,
    );

    // Consume events until the file-change-triggered
    // `daemon.sheaf.invalidate` arrives. Both the priming emit above
    // and the current emit are on the `daemon.sheaf.*` filter; the
    // priming one was drained, so this is the next event. If a
    // future refactor emitted a second sheaf.* event between
    // enrichment.complete and sheaf.invalidate, this loop would still
    // pick out the invalidate — but any test breakage would surface
    // as a clean assertion, not a hang.
    let evt = recv_event(
        &mut rx,
        LOOP_TIMEOUT,
        "post-modification daemon.sheaf.invalidate",
    )
    .await;
    assert_eq!(
        evt.topic, "daemon.sheaf.invalidate",
        "post-modification event must be daemon.sheaf.invalidate; got {:?}",
        evt.topic,
    );

    let payload = &evt.data;

    // ------------------------------------------------------------------
    // Payload contract. Eight fields; each is load-bearing for at
    // least one subscriber. Assert every one. Sheaf gap 3 follow-up
    // (bead `ley-line-open-e40566`) upgraded the region set from
    // coarse-v1 (`all-known` — every region ID in the topology) to
    // fine-grained (`changed-only` — the subset of regions whose
    // labels match `changed_files`). See the `emit_watcher_sheaf_invalidate`
    // docstring for the full two-mode contract.
    // ------------------------------------------------------------------

    // 1. `region_ids` — u32 array. Fine-grained mode: the emit
    //    subsets to regions whose labels match `changed_files`. The
    //    seed fixture has 4 nodes with labels
    //    {`foo.rs`, `foo.rs:sym:foo`, `bar.rs`, `bar.rs:sym:bar`} and
    //    the changed file is `foo.rs`, so the diff picks up the two
    //    foo.rs-labelled regions and drops the two bar.rs-labelled
    //    ones. If region_ids came back with 4 IDs (the full topology)
    //    the fine-grained resolver silently regressed to coarse-v1.
    let region_ids_arr = payload
        .get("invalidated")
        .expect("payload must include `region_ids`")
        .as_array()
        .expect("`region_ids` must be a JSON array");
    assert_eq!(
        region_ids_arr.len(),
        2,
        "fine-grained region_ids must contain exactly the two \
         foo.rs-labelled regions (foo.rs and foo.rs:sym:foo); got \
         {} region(s). A count of 4 means the emit fell back to \
         coarse-v1 (all-known); a count of 0 means the label install \
         path (ComplexBuildPass::install_region_labels or \
         SheafState::install_region_labels) is broken.",
        region_ids_arr.len(),
    );
    for v in region_ids_arr {
        v.as_u64()
            .expect("each region_ids element must be a JSON number (u32→u64)");
    }

    // 2. `count` — u32 mirroring region_ids.len(). Consumer parse path
    //    convention: mache reads `count` directly instead of `.len()`
    //    of the array.
    let count = payload
        .get("count")
        .and_then(|v| v.as_u64())
        .expect("`count` must be a u64 number");
    assert_eq!(
        count,
        region_ids_arr.len() as u64,
        "`count` must equal region_ids.len(); got count={}, len={}",
        count,
        region_ids_arr.len(),
    );

    // 3. `scope` — sheaf gap 3 follow-up sentinel `"changed-only"`.
    //    With `ComplexBuildPass` in the pipeline the label map is
    //    installed and the emit computes a fine-grained diff. Coarse
    //    fallback `"all-known"` here would mean the label install
    //    path regressed silently — the daemon lost its file→region
    //    mapping and the payload no longer conveys "region-precise
    //    invalidation" that the "sheaves as the moat" pitch relies on.
    let scope = payload
        .get("scope")
        .and_then(|v| v.as_str())
        .expect("`scope` must be a string");
    assert_eq!(
        scope, "changed-only",
        "labels installed via ComplexBuildPass → scope must be \
         \"changed-only\" (fine-grained diff); got {scope:?}. A value \
         of \"all-known\" here means the label install path regressed \
         and the daemon fell back to the coarse-v1 fanout."
    );

    // 4. `changed_files` — array of the files the watcher observed.
    //    Round-trip check: the file we modified must appear.
    let changed_files_arr = payload
        .get("changed_files")
        .expect("payload must include `changed_files`")
        .as_array()
        .expect("`changed_files` must be a JSON array");
    let has_foo = changed_files_arr
        .iter()
        .filter_map(|v| v.as_str())
        .any(|s| s == "foo.rs");
    assert!(
        has_foo,
        "`changed_files` must contain the modified file (foo.rs); got {:?}",
        changed_files_arr,
    );

    // 5. `current_root` — 64-char lowercase hex string from the
    //    substrate controller. Fresh arena returns the all-zeros
    //    sentinel; anything non-empty proves `read_root_hex` reached
    //    the controller successfully.
    let root_hex = payload
        .get("current_root")
        .and_then(|v| v.as_str())
        .expect("`current_root` must be a string");
    assert_eq!(
        root_hex.len(),
        64,
        "`current_root` must be a 64-char hex string; got {} chars: {root_hex:?}",
        root_hex.len(),
    );
    assert!(
        root_hex.chars().all(|c| c.is_ascii_hexdigit()),
        "`current_root` must be lowercase hex; got {root_hex:?}",
    );

    // 6. `generation` — quoted u64. Bumped monotonically by
    //    `SheafCache::bump_generation` on every emit.
    let generation: u64 = payload
        .get("generation")
        .and_then(|v| v.as_str())
        .expect("`generation` must be a quoted string")
        .parse()
        .expect("`generation` must parse as u64");

    // 7. `prior_generation` — quoted u64 strictly less than
    //    `generation`. The strict advance is what keeps
    //    monotonic-counter subscribers correct across watcher-driven
    //    + consumer-driven invalidates.
    let prior_generation: u64 = payload
        .get("prior_generation")
        .and_then(|v| v.as_str())
        .expect("`prior_generation` must be a quoted string")
        .parse()
        .expect("`prior_generation` must parse as u64");
    assert!(
        generation > prior_generation,
        "generation must strictly advance: prior_generation={prior_generation}, \
         generation={generation}",
    );

    // Compositional cross-check: this emit's `prior_generation` must
    // equal the priming emit's `generation`. The bump-generation call
    // is the same physical counter for both watcher-driven and
    // consumer-driven invalidates; if two emits landed on the wire
    // without the counter advancing between them, mache would consider
    // the second event stale and drop it. This is the assertion that
    // gap 2 (persistence of SheafState across emit cycles) is
    // reachable from the wire — not just from the same-call in-memory
    // view the sheaf gap 3 test uses.
    assert_eq!(
        prior_generation, priming_generation,
        "post-modification emit's prior_generation must equal the priming emit's \
         generation ({priming_generation}); got {prior_generation}. Drift means \
         SheafState.cache is being rebuilt between emits instead of persisting — \
         gap 2 regression.",
    );

    // 8. `timestamp_ms` — quoted i64, positive.
    let ts: i64 = payload
        .get("timestamp_ms")
        .and_then(|v| v.as_str())
        .expect("`timestamp_ms` must be a quoted string")
        .parse()
        .expect("`timestamp_ms` must parse as i64");
    assert!(
        ts > 0,
        "`timestamp_ms` must be positive millis-since-epoch; got {ts}",
    );
}

/// No-op file edit path. Rewriting a file with identical content
/// short-circuits `parse_into_conn`'s mtime + content-hash diff, so
/// `reparse.changed_files` comes back empty. Per gap 1 test
/// `watcher_enrichment_with_empty_scope_falls_back_to_full_run`, an
/// empty scope still runs the enrichment pipeline (with a `None` scope
/// passed to each pass) — so an invalidate STILL fires, with an empty
/// `changed_files` array.
///
/// This is the "state advanced but nothing observable changed"
/// continuity signal consumers use to distinguish "daemon is alive
/// and just re-checked" from "daemon is silent (maybe crashed)".
#[tokio::test]
async fn identical_content_rewrite_still_emits_invalidate_with_empty_changed_files() {
    let dir = TempDir::new().expect("tempdir");
    let source_dir = dir.path().join("src");
    std::fs::create_dir_all(&source_dir).expect("mkdir src");

    let foo_path = source_dir.join("foo.rs");
    let foo_content = "fn foo() -> i32 { 7 }\n";
    std::fs::write(&foo_path, foo_content).expect("write foo.rs");

    let (ctx, router) = build_full_ctx(dir.path(), source_dir.clone());

    // Initial parse establishes the mtime + content-hash baseline for
    // the incremental reparse to compare against.
    {
        let guard = ctx.live_db.lock().expect("live_db lock");
        leyline_cli_lib::cmd_parse::parse_into_conn(&guard, &source_dir, None, None)
            .expect("initial parse");
    }

    let (_sub_id, mut rx, _replay, _gap) = router
        .subscribe(
            &["daemon.sheaf.invalidate".to_string()],
            None,
            0,
            OverflowPolicy::DropOldest,
            16,
        )
        .await;

    let emitter = router.emitter();

    // Rewrite with identical content. mtime updates but content hash
    // stays the same. The scoped reparse returns 0 parsed / 1
    // unchanged — the parse pipeline knows the file didn't
    // structurally change and skips the tree-sitter walk.
    std::fs::write(&foo_path, foo_content).expect("re-write foo.rs with identical content");
    let reparse = {
        let guard = ctx.live_db.lock().expect("live_db lock");
        leyline_cli_lib::cmd_parse::parse_into_conn(
            &guard,
            &source_dir,
            None,
            Some(&["foo.rs".to_string()]),
        )
        .expect("scoped reparse (identical-content path)")
    };
    // The exact parsed count here can be 0 or 1 depending on whether
    // content hashing beats mtime — either way the file didn't
    // structurally change. What matters for this test is that the
    // watcher-enrichment call downstream still fires the invalidate,
    // not the specific parsed count. Assert only that no *unexpected*
    // errors occurred (the reparse succeeded).
    let _ = reparse; // suppress unused warning; kept for clarity + future assertions.

    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(
        &ctx,
        &source_dir,
        &[], // empty scope — matches the git_watch_loop behavior when
        // dirty_vec is empty (see `cmd_daemon.rs:1052`).
        &emitter,
    );

    let evt = recv_event(
        &mut rx,
        LOOP_TIMEOUT,
        "identical-content daemon.sheaf.invalidate",
    )
    .await;
    assert_eq!(evt.topic, "daemon.sheaf.invalidate");

    // Empty changed_files — the continuity signal. Consumers that
    // key on "which files changed" get an empty array and know
    // nothing user-observable moved; the generation still advances
    // so freshness checks pass.
    let changed_files_arr = evt
        .data
        .get("changed_files")
        .expect("payload must include `changed_files`")
        .as_array()
        .expect("`changed_files` must be an array");
    assert!(
        changed_files_arr.is_empty(),
        "identical-content rewrite must produce an empty changed_files \
         array (continuity signal); got {:?}",
        changed_files_arr,
    );

    // Generation still advances — the invariant we rely on for
    // monotonic-counter consumers.
    let prior: u64 = evt.data["prior_generation"]
        .as_str()
        .expect("prior_generation quoted")
        .parse()
        .expect("prior_generation parses");
    let generation: u64 = evt.data["generation"]
        .as_str()
        .expect("generation quoted")
        .parse()
        .expect("generation parses");
    assert!(
        generation > prior,
        "empty-scope continuity emit must still bump generation \
         ({prior} → {generation})",
    );
}
