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
    use std::sync::{Mutex, RwLock};

    let arena_path = dir.join("blackbox.arena");
    let ctrl_path = dir.join("blackbox.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).expect("create arena");
    let mut ctrl = Controller::open_or_create(&ctrl_path).expect("open ctrl");
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
        .expect("set arena");
    drop(ctrl);

    Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db: Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
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
