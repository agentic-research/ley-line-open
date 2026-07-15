//! Black-box regression test for ley-line-open-d03e7d.
//!
//! The bug: `sheaf_invalidate` returned `invalidated: []` to every UDS
//! consumer in v0.4.0 because the cascade was gated on the local
//! `entries` map being populated — and no daemon op populates `entries`,
//! so for mache / cloister / any external process the cascade response
//! was always empty. The fix (cache.rs `on_change`) decouples the
//! returned list from the in-process entries map.
//!
//! This test pins the fix from the *consumer's* point of view: it
//! spawns a real UDS daemon and drives every interaction through the
//! wire (no `cache.put` back-doors, no `ctx.sheaf` reach-arounds).
//! When the d03e7d regression returns, this is the test that catches it.
//!
//! Falsifiability gate 1 (from the bead):
//!   "spawn daemon, push topology with f32 stalks (engages δ⁰), call
//!    sheaf_invalidate with a stalk that breaks an agreement coord.
//!    Assert `invalidated` contains AT LEAST the changed region AND a
//!    cascade neighbor. ❌ fail if `invalidated: []`."

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leyline_cli_lib::daemon::{
    DaemonContext, DaemonState, EventRouter, NoExt, sheaf_ops::SheafState, socket,
};
use leyline_core::{Controller, create_arena};
use tempfile::TempDir;

/// Stand up the bare-minimum `DaemonContext` needed to host the sheaf
/// ops over UDS. No reparse pipeline, no source dir, no live db — just
/// the controller + sheaf state. Anything else would invite false
/// positives from neighboring subsystems.
fn build_blackbox_ctx(dir: &Path) -> Arc<DaemonContext> {
    use parking_lot::{Mutex, RwLock};

    let arena_path = dir.join("blackbox.arena");
    let ctrl_path = dir.join("blackbox.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).expect("create arena");
    let mut ctrl = Controller::open_or_create(&ctrl_path).expect("open ctrl");
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
        .expect("set arena");
    drop(ctrl);

    // File-backed WAL LiveDb — pool needs a real file (bead
    // `ley-line-open-f0239d`).
    let live_db_path = ctrl_path.with_extension("live.db");
    let live_db = leyline_cli_lib::daemon::db_pool::LiveDb::open_fresh_for_test(&live_db_path);

    Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
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
        text_search: std::sync::Arc::new(leyline_text_search::null::NullEngine::new()),
        sheaf: Arc::new(SheafState::new()),
    })
}

/// Spawn the daemon UDS listener and block until it actually accepts.
/// Returns the bound socket path (may differ from the input on some
/// platforms — the inner spawn handles the disambiguation).
async fn spawn_blackbox_socket(ctx: Arc<DaemonContext>, sock_path: PathBuf) -> PathBuf {
    use tokio::net::UnixStream;

    let path = socket::spawn(ctx, sock_path);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(&path).await.is_ok() {
            return path;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "UDS listener at {} did not accept within 2s",
        path.display()
    );
}

/// One round-trip over the daemon's UDS: connect, write `body`, read
/// one response line, parse JSON. Deliberately small — every regression
/// in this test must come from the wire, not from a fancy client.
async fn uds_call(sock: &Path, body: &str) -> serde_json::Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(sock).await.expect("UDS connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    writer.write_all(body.as_bytes()).await.expect("UDS write");
    if !body.ends_with('\n') {
        writer.write_all(b"\n").await.expect("UDS newline");
    }
    let line = lines
        .next_line()
        .await
        .expect("UDS read")
        .expect("response line");
    serde_json::from_str(&line).expect("response is JSON")
}

/// The regression test. Drives the daemon entirely through UDS JSON ops
/// and asserts the cascade response contains BOTH the changed region
/// AND its neighbor when the change moves an agreement coord — the
/// exact shape `invalidated: []` would fail.
#[tokio::test]
async fn sheaf_invalidate_returns_non_empty_cascade_over_uds() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock_path = dir.path().join("sheaf-blackbox.sock");
    let sock = spawn_blackbox_socket(ctx, sock_path).await;

    // ── Step 1: push topology with f32 stalks → engages δ⁰ mode ──
    //
    // Two regions, one edge, agreement_dim=2. Stalks share the first
    // two coords [1.0, 0.5] (the agreement subspace) so the initial
    // δ⁰ baseline is zero on this edge.
    let topology = uds_call(
        &sock,
        r#"{"op":"sheaf_set_topology","node_stalk_dim":4,"regions":[{"id":0,"hash":"aa","data":[1.0,0.5,0.0,0.0]},{"id":1,"hash":"bb","data":[1.0,0.5,9.0,9.0]}],"restrictions":[{"a":0,"b":1,"boundary_hash":"11","co_change_rate":0.5,"weights":[1.0],"agreement_dim":2}]}"#,
    )
    .await;
    assert_eq!(
        topology["ok"], true,
        "sheaf_set_topology must succeed: {topology}"
    );
    assert_eq!(
        topology["delta_zero_mode"], true,
        "δ⁰ mode must engage with node_stalk_dim>0 + f32 data + agreement_dim>0: {topology}"
    );

    // ── Step 2: invalidate with a stalk that breaks the agreement ──
    //
    // Region 0's new coord [0] flips from 1.0 → 99.0. That's IN the
    // agreement subspace — projection moves, δ⁰ moves, cascade must
    // reach region 1. With the d03e7d bug present (cascade gated on
    // local `entries`), the response would be `invalidated: []`
    // because no daemon op populates `entries`.
    let invalidate = uds_call(
        &sock,
        r#"{"op":"sheaf_invalidate","regions":[0],"stalks":[{"id":0,"hash":"ee","data":[99.0,0.5,42.0,7.0]}]}"#,
    )
    .await;

    let invalidated: Vec<u32> = invalidate["invalidated"]
        .as_array()
        .unwrap_or_else(|| panic!("invalidated must be an array; full response: {invalidate}"))
        .iter()
        .map(|v| {
            v.as_u64()
                .unwrap_or_else(|| panic!("invalidated entries must be u64-coercible; got {v:?}"))
                as u32
        })
        .collect();

    // The regression check. Both bullets must hold; either failure is
    // d03e7d coming back.
    assert!(
        !invalidated.is_empty(),
        "REGRESSION (d03e7d): sheaf_invalidate returned empty over UDS — \
         cascade is gated on local entries again; full response: {invalidate}"
    );
    assert!(
        invalidated.contains(&0),
        "cascade must contain the changed region 0; got {invalidated:?} (full: {invalidate})"
    );
    assert!(
        invalidated.contains(&1),
        "cascade must reach neighbor 1 when agreement coord moves; \
         got {invalidated:?} (full: {invalidate})"
    );

    // ── Step 3: sheaf_status corroborates the generation tick ──
    //
    // A passing `invalidated` cascade should be paired with a >0
    // generation; checking both pins the wire shape, not just one
    // field.
    let status = uds_call(&sock, r#"{"op":"sheaf_status"}"#).await;
    let generation: u64 = status["generation"]
        .as_str()
        .unwrap_or_else(|| panic!("generation must be Int64-as-string; full: {status}"))
        .parse()
        .expect("generation parses as u64");
    assert!(
        generation >= 1,
        "generation must reflect the invalidate call; got {generation} (full: {status})"
    );
}

/// Heuristic-only path (no f32 data, no agreement_dim) must also return
/// a non-empty cascade when the XOR pre-filter fires. Confirms the fix
/// applies to both δ⁰ mode and the legacy hash-only mode — the bug
/// affected both, since both went through the same `entries` gate.
#[tokio::test]
async fn sheaf_invalidate_heuristic_mode_non_empty_cascade_over_uds() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock_path = dir.path().join("sheaf-heuristic.sock");
    let sock = spawn_blackbox_socket(ctx, sock_path).await;

    // Heuristic-only topology: hashes only, no f32 data, no agreement_dim.
    let topology = uds_call(
        &sock,
        r#"{"op":"sheaf_set_topology","regions":[{"id":0,"hash":"aa"},{"id":1,"hash":"bb"}],"restrictions":[{"a":0,"b":1,"boundary_hash":"11","co_change_rate":0.5,"weights":[1.0]}]}"#,
    )
    .await;
    assert_eq!(
        topology["delta_zero_mode"], false,
        "heuristic-only path must advertise delta_zero_mode=false: {topology}"
    );

    // Flip region 0's hash — XOR boundary check now disagrees with the
    // stored boundary_hash, so the cascade must include region 1.
    let invalidate = uds_call(
        &sock,
        r#"{"op":"sheaf_invalidate","regions":[0],"stalks":[{"id":0,"hash":"ff"}]}"#,
    )
    .await;
    let invalidated: Vec<u32> = invalidate["invalidated"]
        .as_array()
        .unwrap_or_else(|| panic!("invalidated must be an array; full: {invalidate}"))
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();

    assert!(
        invalidated.contains(&0),
        "cascade must include changed region 0; got {invalidated:?} (full: {invalidate})"
    );
    assert!(
        invalidated.contains(&1),
        "XOR cascade must reach neighbor 1 when boundary hash moves; \
         got {invalidated:?} (full: {invalidate})"
    );
}

/// Bead ley-line-open-9d2302 falsifiability gate:
/// `sheaf_update_topology` returns the touched ∪ radius-1 subset, NOT the
/// whole graph. Pin this by seeding a 10-region star (center=0, leaves=1..9)
/// and applying a 1-region delta that only touches the center. The
/// affected list must contain center+leaves (radius-1 from center =
/// every leaf), but the daemon must NOT report the test back as having
/// re-emitted the entire complex's invalidation cascade — the response
/// shape is "affected subset" not "everyone".
///
/// The strong check: invalidated.len() == 10 (center + 9 leaves) when
/// the center moves; if the impl regresses to set_topology semantics the
/// test still passes here (size matches), but if it regresses to
/// "everyone" semantics on a complex with EXTRA disconnected regions the
/// test catches it.
#[tokio::test]
async fn update_topology_over_uds_returns_affected_subset_not_whole_graph() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock_path = dir.path().join("sheaf-update.sock");
    let sock = spawn_blackbox_socket(ctx, sock_path).await;

    // Step 1: seed 12 regions. A 10-region star (center=0, leaves=1..9)
    // is connected; regions 10 and 11 are intentionally isolated. After
    // an update that touches only the center, the affected list must
    // include center + every leaf (10 regions) but NOT 10 or 11.
    //
    // δ⁰ mode engaged: every stalk is 4D, every edge agreement_dim=2.
    let mut regions = String::from("[");
    for id in 0..12u32 {
        if id > 0 {
            regions.push(',');
        }
        regions.push_str(&format!(
            r#"{{"id":{id},"hash":"{id:02x}","data":[1.0,0.5,{id}.0,0.0]}}"#
        ));
    }
    regions.push(']');

    let mut restrictions = String::from("[");
    for leaf in 1..10u32 {
        if leaf > 1 {
            restrictions.push(',');
        }
        restrictions.push_str(&format!(
            r#"{{"a":0,"b":{leaf},"boundary_hash":"{:02x}","co_change_rate":0.5,"weights":[1.0],"agreement_dim":2}}"#,
            0u8 ^ (leaf as u8)
        ));
    }
    restrictions.push(']');

    let topology = uds_call(
        &sock,
        &format!(
            r#"{{"op":"sheaf_set_topology","node_stalk_dim":4,"regions":{regions},"restrictions":{restrictions}}}"#
        ),
    )
    .await;
    assert_eq!(topology["ok"], true, "seed must succeed: {topology}");
    assert_eq!(
        topology["delta_zero_mode"], true,
        "δ⁰ mode must engage: {topology}"
    );

    // Step 2: incremental update that ONLY changes region 0's stalk.
    // The affected set should be {0, 1, 2, ..., 9} — center plus every
    // leaf neighbour. Regions 10 and 11 (disconnected) must NOT appear.
    let update = uds_call(
        &sock,
        r#"{"op":"sheaf_update_topology","node_stalk_dim":4,"delta":{"updated_stalks":[{"region_id":0,"stalk":[99.0,0.5,0.0,0.0]}]}}"#,
    )
    .await;

    assert_eq!(
        update["ok"], true,
        "sheaf_update_topology must succeed: {update}"
    );

    let affected: Vec<u32> = update["affected_regions"]
        .as_array()
        .unwrap_or_else(|| panic!("affected_regions must be an array; full: {update}"))
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();

    // The center (0) and its 9 leaves must all be in the affected set.
    assert!(
        affected.contains(&0),
        "touched region 0 must appear in affected; got {affected:?}"
    );
    for leaf in 1..10u32 {
        assert!(
            affected.contains(&leaf),
            "radius-1 neighbour {leaf} must appear in affected; got {affected:?}"
        );
    }

    // The strong claim: the disconnected regions must NOT appear. If the
    // op regresses to "rebuild everything" semantics it will return all
    // 12 regions; this check catches that.
    assert!(
        !affected.contains(&10),
        "disconnected region 10 must NOT appear in affected; got {affected:?} (full: {update})"
    );
    assert!(
        !affected.contains(&11),
        "disconnected region 11 must NOT appear in affected; got {affected:?} (full: {update})"
    );
    assert_eq!(
        affected.len(),
        10,
        "affected set must be exactly {{center + 9 leaves}}; got {affected:?} (full: {update})"
    );

    // Step 3: generation must advance, defect_after must be finite.
    let generation: u64 = update["generation"]
        .as_str()
        .unwrap_or_else(|| panic!("generation must be Int64-as-string; full: {update}"))
        .parse()
        .expect("generation parses as u64");
    assert!(
        generation >= 1,
        "generation must advance after the update; got {generation}"
    );
    let defect_after = update["defect_after"]
        .as_f64()
        .unwrap_or_else(|| panic!("defect_after must be numeric; full: {update}"));
    assert!(
        defect_after.is_finite() && defect_after >= 0.0,
        "defect_after must be a finite non-negative number; got {defect_after}"
    );
}

/// Reaper end-to-end gate (bead `ley-line-open-9c867f`, GC item 3).
///
/// Asserts the wire contract for `sheaf_reap`: pure-observational query
/// that returns the regions whose boundary signal moved since the last
/// baseline refresh. Two-phase:
///
/// 1. Push topology + stalks that agree → call reap → expect empty
///    (baseline matches current section).
/// 2. Mutate a stalk via `sheaf_invalidate` (which refreshes baseline
///    side-effectfully via `on_change`) — wait, no. `on_change` doesn't
///    refresh baseline; it only marks `entries` invalid. To get the
///    reaper to see drift, we need a stalk update WITHOUT a baseline
///    refresh between then and the reap call. `set_topology` does
///    refresh, so we can't use it. The closest "push new stalk without
///    re-baselining" path is to push the topology with stalk S0, then
///    push it AGAIN with stalk S1 — actually that re-baselines too.
///
///    Cleanest end-to-end: push topology with agreeing stalks (baseline
///    captures zero defect), then send a NEW set_topology call with
///    drifted stalks (baseline re-captures, so reap sees zero again),
///    then... hmm. Actually a non-stalk-mutation invalidate sequence
///    via `sheaf_invalidate` does NOT refresh baseline — it only
///    updates the cache's stalk hashes and triggers on_change. The
///    complex's stalk f32 data DOES get updated through the cache's
///    `set_stalk_value`. So: drift the stalks via invalidate (with
///    data), then call reap — the baseline is stale relative to the
///    new complex data, so reap should see something.
///
/// This test pins exactly that: invalidate-then-reap shows the drift.
#[tokio::test]
async fn sheaf_reap_observes_drift_over_uds() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock_path = dir.path().join("sheaf-reap.sock");
    let sock = spawn_blackbox_socket(ctx, sock_path).await;

    // ── Step 1: push topology with agreeing stalks → baseline zero ──
    //
    // Same shape as the existing invalidate test: stalk_dim=4,
    // agreement_dim=2, both regions share [1.0, 0.5] in the agreement
    // subspace. set_topology refreshes baseline as part of its δ⁰
    // engagement, so the baseline captures the current (consistent)
    // section.
    let topology = uds_call(
        &sock,
        r#"{"op":"sheaf_set_topology","node_stalk_dim":4,"regions":[{"id":0,"hash":"aa","data":[1.0,0.5,0.0,0.0]},{"id":1,"hash":"bb","data":[1.0,0.5,9.0,9.0]}],"restrictions":[{"a":0,"b":1,"boundary_hash":"11","co_change_rate":0.5,"weights":[1.0],"agreement_dim":2}]}"#,
    )
    .await;
    assert_eq!(
        topology["ok"], true,
        "set_topology must succeed: {topology}"
    );
    assert_eq!(
        topology["delta_zero_mode"], true,
        "δ⁰ mode must engage: {topology}"
    );

    // ── Step 2: reap on the stable section → empty + finite defect ──
    let reap_initial = uds_call(&sock, r#"{"op":"sheaf_reap"}"#).await;
    let initial_reclaim: Vec<u32> = reap_initial["reclaimable"]
        .as_array()
        .unwrap_or_else(|| {
            panic!("reclaimable must be an array on the initial call; full: {reap_initial}")
        })
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    assert!(
        initial_reclaim.is_empty(),
        "reap on stable section must return empty over UDS; got {initial_reclaim:?} \
         (full: {reap_initial})"
    );

    // ── Step 3: drift region 0's stalk via sheaf_invalidate ──────────
    //
    // sheaf_invalidate with `data` propagates through cache.set_stalk_
    // value → cx.set_node_stalk, mutating the complex's internal stalk.
    // It does NOT call refresh_baseline, so the cached baseline now
    // refers to the OLD section — exactly the staleness reap is
    // designed to detect.
    let invalidate = uds_call(
        &sock,
        r#"{"op":"sheaf_invalidate","regions":[0],"stalks":[{"id":0,"hash":"ee","data":[99.0,0.5,42.0,7.0]}]}"#,
    )
    .await;
    assert_eq!(
        invalidate["count"].as_u64().unwrap_or(0) > 0,
        true,
        "invalidate must produce a cascade (validates the d03e7d fix is still live); full: {invalidate}"
    );

    // ── Step 4: reap after drift → non-empty, includes region 0 ──────
    let reap_after = uds_call(&sock, r#"{"op":"sheaf_reap"}"#).await;
    let after_reclaim: Vec<u32> = reap_after["reclaimable"]
        .as_array()
        .unwrap_or_else(|| panic!("reclaimable must be an array after drift; full: {reap_after}"))
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    assert!(
        !after_reclaim.is_empty(),
        "reap after stalk drift must return non-empty; got {after_reclaim:?} (full: {reap_after})"
    );
    assert!(
        after_reclaim.contains(&0),
        "reap must include the drifted region 0; got {after_reclaim:?} (full: {reap_after})"
    );
    let defect_after = reap_after["reaped_at_defect"].as_f64().unwrap_or_else(|| {
        panic!("reaped_at_defect must be numeric on δ⁰ path; full: {reap_after}")
    });
    assert!(
        defect_after.is_finite() && defect_after >= 0.0,
        "reaped_at_defect must be finite + non-negative when complex is attached; got {defect_after}"
    );
}

/// Helper: parse the capnp-json Int64-as-string form into u64. The
/// generation field is `UInt64` which the capnp-json codec emits as a
/// JSON string (JS Number can't carry 53+ bit precision losslessly).
fn parse_u64_field(v: &serde_json::Value, label: &str, ctx: &serde_json::Value) -> u64 {
    v.as_str()
        .unwrap_or_else(|| panic!("{label} must be Int64-as-string; full: {ctx}"))
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("{label} must parse as u64; full: {ctx}"))
}

/// Continuity contract for `prior_generation` on `sheaf_invalidate`
/// (bead `ley-line-open-9d5d7d`, GC item 1).
///
/// Across N successive invalidate calls, the contract is:
///
///   response[i+1].prior_generation == response[i].generation
///
/// This is the cache-coherence signal: a consumer that saw generation
/// K and observes generation M with prior_generation == K knows it
/// did not miss any events. Without this field, a consumer can only
/// detect "the gen advanced" — not "I'm in sync."
///
/// Also pins the initial value: prior_generation on the first ever
/// invalidate equals 0 (the cache's seed generation).
#[tokio::test]
async fn sheaf_invalidate_prior_generation_continuity_over_uds() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock_path = dir.path().join("sheaf-prior-gen.sock");
    let sock = spawn_blackbox_socket(ctx, sock_path).await;

    // Push topology to seed the cache. set_topology doesn't bump
    // generation in the cache (it's the seed call) — verified by
    // observing that the first invalidate's prior_generation is 0.
    let topology = uds_call(
        &sock,
        r#"{"op":"sheaf_set_topology","node_stalk_dim":4,"regions":[{"id":0,"hash":"aa","data":[1.0,0.5,0.0,0.0]},{"id":1,"hash":"bb","data":[1.0,0.5,9.0,9.0]}],"restrictions":[{"a":0,"b":1,"boundary_hash":"11","co_change_rate":0.5,"weights":[1.0],"agreement_dim":2}]}"#,
    )
    .await;
    assert_eq!(
        topology["ok"], true,
        "set_topology must succeed: {topology}"
    );

    // Three successive invalidate calls. We don't care about cascade
    // contents here — only the generation/prior_generation continuity.
    let mut prev_gen: Option<u64> = None;
    for i in 0..3 {
        let body = format!(
            r#"{{"op":"sheaf_invalidate","regions":[0],"stalks":[{{"id":0,"hash":"{i:02x}","data":[{},0.5,42.0,7.0]}}]}}"#,
            (i + 1) as f32 * 10.0,
        );
        let resp = uds_call(&sock, &body).await;
        // `gen` is a reserved keyword on edition 2024; using `g` for the
        // generation snapshot and `prior` for prior_generation keeps the
        // assertions readable without raw-identifier escaping.
        let g = parse_u64_field(&resp["generation"], "generation", &resp);
        let prior = parse_u64_field(&resp["prior_generation"], "prior_generation", &resp);

        match prev_gen {
            None => {
                // First call: prior_generation must equal 0 (the
                // seed generation, before any on_change ever fired).
                assert_eq!(
                    prior, 0,
                    "first invalidate's prior_generation MUST be 0 (cache seed); \
                     got prior={prior} gen={g}; full: {resp}"
                );
            }
            Some(last) => {
                // Continuity: the previous call's generation must
                // EXACTLY equal this call's prior_generation. Any drift
                // means a missed event or a non-monotonic bump.
                assert_eq!(
                    prior, last,
                    "call {i}: prior_generation ({prior}) must equal previous call's generation ({last}); \
                     full: {resp}"
                );
            }
        }
        assert!(
            g > prior,
            "generation ({g}) must strictly exceed prior_generation ({prior}) — \
             on_change always bumps; full: {resp}"
        );
        prev_gen = Some(g);
    }
}

/// Same continuity contract on `sheaf_update_topology`. Cross-op
/// continuity (invalidate followed by update) is the consumer's actual
/// usage pattern: they get an invalidate event, evict their cache,
/// later see an update event — `update.prior_generation` must equal
/// the prior `invalidate.generation` to prove no events were missed.
#[tokio::test]
async fn sheaf_update_topology_prior_generation_continuity_over_uds() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock_path = dir.path().join("sheaf-prior-gen-update.sock");
    let sock = spawn_blackbox_socket(ctx, sock_path).await;

    let topology = uds_call(
        &sock,
        r#"{"op":"sheaf_set_topology","node_stalk_dim":4,"regions":[{"id":0,"hash":"aa","data":[1.0,0.5,0.0,0.0]},{"id":1,"hash":"bb","data":[1.0,0.5,9.0,9.0]}],"restrictions":[{"a":0,"b":1,"boundary_hash":"11","co_change_rate":0.5,"weights":[1.0],"agreement_dim":2}]}"#,
    )
    .await;
    assert_eq!(
        topology["ok"], true,
        "set_topology must succeed: {topology}"
    );

    // Step 1: invalidate. Snapshot its generation.
    let inv = uds_call(
        &sock,
        r#"{"op":"sheaf_invalidate","regions":[0],"stalks":[{"id":0,"hash":"11","data":[2.0,0.5,42.0,7.0]}]}"#,
    )
    .await;
    let after_invalidate_gen = parse_u64_field(&inv["generation"], "generation", &inv);
    let after_invalidate_prior =
        parse_u64_field(&inv["prior_generation"], "prior_generation", &inv);
    assert_eq!(
        after_invalidate_prior, 0,
        "first invalidate's prior_generation must be 0; full: {inv}"
    );
    assert!(after_invalidate_gen > 0);

    // Step 2: update_topology adding a new region. Must report
    // prior_generation == after_invalidate_gen (continuity across ops).
    let upd = uds_call(
        &sock,
        r#"{"op":"sheaf_update_topology","node_stalk_dim":4,"delta":{"added_regions":[{"id":2,"hash":"cc","data":[1.0,0.5,0.0,0.0]}],"removed_regions":[],"added_edges":[{"a":1,"b":2,"boundary_hash":"22","co_change_rate":0.5,"weights":[1.0],"agreement_dim":2}],"removed_edges":[],"updated_stalks":[]}}"#,
    )
    .await;
    assert_eq!(upd["ok"], true, "update_topology must succeed: {upd}");
    let after_update_gen = parse_u64_field(&upd["generation"], "generation", &upd);
    let after_update_prior = parse_u64_field(&upd["prior_generation"], "prior_generation", &upd);

    assert_eq!(
        after_update_prior, after_invalidate_gen,
        "cross-op continuity broken: update.prior_generation ({after_update_prior}) \
         must equal previous invalidate.generation ({after_invalidate_gen}); full: {upd}"
    );
    assert!(
        after_update_gen > after_update_prior,
        "update generation ({after_update_gen}) must strictly exceed prior ({after_update_prior}); full: {upd}"
    );
}
