//! Black-box regression test for ley-line-open-5caa59.
//!
//! The bug: the daemon's UDS connection loop wrote op responses to the
//! wire but never drained `ConnectionState.event_rx`, so pushed events
//! emitted onto the bus AFTER subscribe never reached the client. Replay
//! events appended to the subscribe response masked the gap — a
//! subscriber that polled briefly after registering saw the replay batch
//! and assumed the live push worked.
//!
//! Surfaced by mache PR #384 c14c43 end-to-end validation, 2026-05-18.
//! The mache `tools/sheaf-subscribe-probe/main.go` reproduced it against
//! any pre-fix daemon (a sheaf_invalidate emitted no `sheaf.invalidate`
//! event on the wire even when the cascade math ran correctly).
//!
//! This file drives the regression entirely through the UDS wire — no
//! in-process `router.emitter()` short-cuts. Each test pins a separate
//! invariant of the writer-task + event-relay fix in `socket.rs`:
//!
//!   1. `emit_op_pushed_event_reaches_uds_subscriber` — the core
//!      regression: publisher conn issues `emit` AFTER another conn's
//!      subscribe is ack'd, subscriber reads the live event line. Pre-
//!      fix: subscriber times out (event_rx never drained).
//!   2. `pushed_events_arrive_in_emit_order` — pins that ten serial
//!      emits arrive on the subscriber in monotonic emit order. Guards
//!      against future refactors that swap the relay's single-receiver
//!      `recv` loop for a `select!` or parallel writes.
//!   3. `resubscribe_replaces_prior_subscription_cleanly` — pins the
//!      "second subscribe drops the first" contract: old topic emits
//!      no longer deliver, new topic emits do, exactly once.
//!   4. `sheaf_invalidate_event_reaches_uds_subscriber` — the bead's
//!      exact reproduction: subscribe `**`, push topology + invalidate
//!      from a second conn, assert the `sheaf.invalidate` line arrives.
//!
//! Every test subscribes BEFORE any emit/invalidate fires, so none of
//! them rely on the EventLog replay batch that masked the bug.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leyline_cli_lib::daemon::{
    DaemonContext, DaemonState, EventRouter, NoExt, sheaf_ops::SheafState, socket,
};
use leyline_core::{Controller, create_arena};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Minimal daemon context — controller + sheaf state, no reparse, no
/// source dir. Same shape as sheaf_uds_blackbox_test.rs's helper so a
/// regression in shared bring-up surfaces in both files at once.
fn build_blackbox_ctx(dir: &Path) -> Arc<DaemonContext> {
    use std::sync::{Mutex, RwLock};

    let arena_path = dir.join("blackbox.arena");
    let ctrl_path = dir.join("blackbox.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).expect("create arena");
    let mut ctrl = Controller::open_or_create(&ctrl_path).expect("open ctrl");
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
        .expect("set arena");
    drop(ctrl);

    let router = EventRouter::new(16);
    let sheaf = Arc::new(SheafState::new());
    sheaf.set_emitter(router.emitter());

    Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router,
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
        sheaf,
    })
}

/// Spawn the UDS listener and wait until it accepts at least one
/// connection — otherwise a test that races startup picks up the
/// pre-bind error instead of the regression we care about.
async fn spawn_blackbox_socket(ctx: Arc<DaemonContext>, sock_path: PathBuf) -> PathBuf {
    let path = socket::spawn(ctx, sock_path);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(&path).await.is_ok() {
            return path;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "UDS listener at {} did not accept within 2s",
        path.display()
    );
}

/// Persistent client over UDS — used by the subscriber side which has
/// to hold the connection open across multiple `read_line` calls.
struct UdsConn {
    writer: tokio::net::unix::OwnedWriteHalf,
    reader: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
}

impl UdsConn {
    async fn connect(sock: &Path) -> Self {
        let stream = UnixStream::connect(sock).await.expect("UDS connect");
        let (reader, writer) = stream.into_split();
        UdsConn {
            writer,
            reader: BufReader::new(reader).lines(),
        }
    }

    async fn send(&mut self, body: &str) {
        self.writer
            .write_all(body.as_bytes())
            .await
            .expect("UDS write");
        if !body.ends_with('\n') {
            self.writer.write_all(b"\n").await.expect("UDS newline");
        }
    }

    /// Read one line from the connection, bounded by `timeout`. Returns
    /// `None` on timeout (so the caller can assert the regression
    /// rather than hang the suite).
    async fn read_line(&mut self, timeout: Duration) -> Option<String> {
        tokio::time::timeout(timeout, self.reader.next_line())
            .await
            .ok()?
            .ok()?
    }
}

/// Generic regression: subscribe on conn A, emit from conn B, assert
/// conn A reads the live event line. The `emit` op is a clean way to
/// inject an event without touching sheaf/snapshot/git machinery — if
/// THIS path is broken, the UDS subscriber contract is broken
/// regardless of which subsystem fires.
#[tokio::test]
async fn emit_op_pushed_event_reaches_uds_subscriber() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock = spawn_blackbox_socket(ctx, dir.path().join("event-push.sock")).await;

    // ── Subscriber on conn A — registers BEFORE the publish fires, so
    //    the event must travel the live push path, not replay.
    let mut sub = UdsConn::connect(&sock).await;
    sub.send(r#"{"op":"subscribe","topics":["regression.5caa59"]}"#)
        .await;
    let sub_resp = sub
        .read_line(Duration::from_secs(2))
        .await
        .expect("subscribe response timed out");
    let sub_resp_json: serde_json::Value =
        serde_json::from_str(&sub_resp).expect("subscribe response not JSON");
    assert_eq!(sub_resp_json.get("ok"), Some(&serde_json::json!(true)));

    // ── Publisher on conn B — emits a single event AFTER the subscribe
    //    is acknowledged. This guarantees the event post-dates the log
    //    snapshot returned in the subscribe replay; the only path for
    //    it to reach the subscriber is live push.
    let mut pub_conn = UdsConn::connect(&sock).await;
    pub_conn
        .send(
            r#"{"op":"emit","topic":"regression.5caa59","source":"test","data":{"marker":"live-push"}}"#,
        )
        .await;
    let pub_resp = pub_conn
        .read_line(Duration::from_secs(2))
        .await
        .expect("emit response timed out");
    let pub_resp_json: serde_json::Value =
        serde_json::from_str(&pub_resp).expect("emit response not JSON");
    assert_eq!(pub_resp_json.get("ok"), Some(&serde_json::json!(true)));

    // ── The regression-pinning assertion.
    //
    // Pre-fix: this read_line times out — the daemon never drains
    // `event_rx` and the dispatcher's `try_send` into the per-subscriber
    // mpsc accumulates with no consumer. Post-fix: the connection's
    // writer task forwards the event line within tens of milliseconds.
    let evt_line = sub
        .read_line(Duration::from_secs(2))
        .await
        .expect("ley-line-open-5caa59 regression: subscriber never received pushed event");
    let evt: serde_json::Value = serde_json::from_str(&evt_line).expect("event line not JSON");
    assert_eq!(
        evt.get("event"),
        Some(&serde_json::json!(true)),
        "wire payload missing `event: true` discriminator"
    );
    assert_eq!(
        evt.get("topic").and_then(|v| v.as_str()),
        Some("regression.5caa59"),
    );
    assert_eq!(
        evt.get("data")
            .and_then(|d| d.get("marker"))
            .and_then(|m| m.as_str()),
        Some("live-push"),
        "payload did not round-trip through the dispatch chain",
    );
}

/// Pin the in-order delivery invariant. The dispatcher assigns a
/// monotonic `seq` per event and the per-subscriber `mpsc` is FIFO, so
/// subscribers must observe events in emit order — cascade-replay logic
/// downstream (and the `since` resume protocol) leans on this. A
/// regression that swapped the relay task for a `select!` over multiple
/// receivers or added parallel writes would break ordering; this test
/// is the canary.
#[tokio::test]
async fn pushed_events_arrive_in_emit_order() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock = spawn_blackbox_socket(ctx, dir.path().join("event-order.sock")).await;

    let mut sub = UdsConn::connect(&sock).await;
    sub.send(r#"{"op":"subscribe","topics":["order.test"]}"#)
        .await;
    sub.read_line(Duration::from_secs(2))
        .await
        .expect("subscribe response timed out");

    // Emit ten events serially from one publisher conn. Serialised
    // sends ⇒ deterministic dispatcher seq ⇒ deterministic wire order.
    let mut pubc = UdsConn::connect(&sock).await;
    for i in 0..10 {
        pubc.send(&format!(
            r#"{{"op":"emit","topic":"order.test","source":"test","data":{{"idx":{i}}}}}"#,
        ))
        .await;
        pubc.read_line(Duration::from_secs(2))
            .await
            .expect("emit response timed out");
    }

    let mut observed: Vec<u64> = Vec::with_capacity(10);
    while observed.len() < 10 {
        let line = sub
            .read_line(Duration::from_secs(2))
            .await
            .expect("event read timed out before 10 events arrived");
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("event") != Some(&serde_json::json!(true)) {
            continue;
        }
        if v.get("topic").and_then(|t| t.as_str()) != Some("order.test") {
            continue;
        }
        let idx = v
            .get("data")
            .and_then(|d| d.get("idx"))
            .and_then(|n| n.as_u64())
            .expect("data.idx missing");
        observed.push(idx);
    }
    assert_eq!(
        observed,
        (0..10).collect::<Vec<u64>>(),
        "events arrived out of emit order — dispatcher/relay ordering regression",
    );
}

/// Pin the resubscribe-without-leak invariant. A second subscribe on
/// the same connection MUST replace the prior subscription:
///
///   * `handle_subscribe` removes the old subscriber from the router,
///   * the router drops the old `Subscriber.tx`,
///   * the old relay task's `recv()` returns `None` and it exits,
///   * the new subscribe stashes a fresh `event_rx` which the read
///     loop hands to a new relay.
///
/// Pre-fix, both the subscriber and the relay were structurally absent.
/// Post-fix, a regression that forgot to remove the old subscriber would
/// double-deliver each event; a regression that forgot to spawn the new
/// relay would silently swallow the second subscription's events. This
/// test trips on both — it asserts the second subscription gets exactly
/// one copy of an event under its new pattern, and zero under the old.
#[tokio::test]
async fn resubscribe_replaces_prior_subscription_cleanly() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock = spawn_blackbox_socket(ctx, dir.path().join("resub.sock")).await;

    let mut sub = UdsConn::connect(&sock).await;
    sub.send(r#"{"op":"subscribe","topics":["resub.first"]}"#)
        .await;
    sub.read_line(Duration::from_secs(2))
        .await
        .expect("first subscribe response timed out");

    // Resubscribe to a DIFFERENT topic. Pre-fix the relay didn't exist
    // at all; post-fix the new pattern must take effect AND the old
    // pattern must stop matching.
    sub.send(r#"{"op":"subscribe","topics":["resub.second"]}"#)
        .await;
    sub.read_line(Duration::from_secs(2))
        .await
        .expect("resubscribe response timed out");

    // Emit one event on the OLD topic (must NOT be delivered) then one
    // on the NEW topic (MUST be delivered, exactly once).
    let mut pubc = UdsConn::connect(&sock).await;
    pubc.send(r#"{"op":"emit","topic":"resub.first","source":"test","data":{"marker":"dropped"}}"#)
        .await;
    pubc.read_line(Duration::from_secs(2))
        .await
        .expect("emit first response timed out");
    pubc.send(
        r#"{"op":"emit","topic":"resub.second","source":"test","data":{"marker":"delivered"}}"#,
    )
    .await;
    pubc.read_line(Duration::from_secs(2))
        .await
        .expect("emit second response timed out");

    // Drain until we hit the second-topic event or time out.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut second_count = 0usize;
    let mut first_count = 0usize;
    while std::time::Instant::now() < deadline {
        let line = match sub.read_line(Duration::from_millis(300)).await {
            Some(l) => l,
            None => break,
        };
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("event") != Some(&serde_json::json!(true)) {
            continue;
        }
        match v.get("topic").and_then(|t| t.as_str()) {
            Some("resub.first") => first_count += 1,
            Some("resub.second") => second_count += 1,
            _ => continue,
        }
    }
    assert_eq!(
        first_count, 0,
        "stale subscription delivered an event AFTER resubscribe — relay leak",
    );
    assert_eq!(
        second_count, 1,
        "resubscribe did not install a fresh relay for the new pattern",
    );
}

/// Sheaf-specific regression: the exact reproduction sequence from
/// mache's `tools/sheaf-subscribe-probe/main.go`. Subscribe with "**"
/// on conn A, push topology + invalidate from conn B, assert conn A
/// reads the `sheaf.invalidate` event. Distinct from the emit-op test
/// because sheaf emits go through `SheafState.emit` (a different code
/// path than `emit_external`), so it catches regressions on either side
/// of `set_emitter` wiring.
#[tokio::test]
async fn sheaf_invalidate_event_reaches_uds_subscriber() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock = spawn_blackbox_socket(ctx, dir.path().join("sheaf-push.sock")).await;

    // Subscribe to everything on conn A.
    let mut sub = UdsConn::connect(&sock).await;
    sub.send(r#"{"op":"subscribe","topics":["**"]}"#).await;
    let sub_resp = sub
        .read_line(Duration::from_secs(2))
        .await
        .expect("subscribe response timed out");
    let sub_resp_json: serde_json::Value = serde_json::from_str(&sub_resp).unwrap();
    assert_eq!(sub_resp_json.get("ok"), Some(&serde_json::json!(true)));

    // Push topology + invalidate from conn B. Topology emits
    // `sheaf.topology`; invalidate emits `sheaf.invalidate`. Both are
    // live-push events relative to A's subscribe (A registered before
    // any of these fired, so the event log at subscribe time held no
    // sheaf.* entries to replay).
    let mut ops = UdsConn::connect(&sock).await;
    ops.send(
        r#"{"op":"sheaf_set_topology","node_stalk_dim":4,"regions":[{"id":1,"hash":"aa","data":[1.0,0.5,0.0,0.0]},{"id":2,"hash":"bb","data":[1.0,0.5,9.0,9.0]}],"restrictions":[{"a":1,"b":2,"boundary_hash":"11","co_change_rate":0.5,"weights":[1.0],"agreement_dim":2}]}"#,
    )
    .await;
    let _topology_resp = ops
        .read_line(Duration::from_secs(2))
        .await
        .expect("set_topology response timed out");

    ops.send(
        r#"{"op":"sheaf_invalidate","regions":[1],"stalks":[{"id":1,"hash":"aa-mutated","data":[2.0,0.5,0.0,0.0]}]}"#,
    )
    .await;
    let _invalidate_resp = ops
        .read_line(Duration::from_secs(2))
        .await
        .expect("invalidate response timed out");

    // Drain conn A until we either find `sheaf.invalidate` or time out.
    // Skipping unrelated topics (like sheaf.topology) makes the test
    // robust to whichever order the bus dispatches — we only assert on
    // the specific event the bead failed on.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut found_topic: Option<String> = None;
    while std::time::Instant::now() < deadline {
        let line = match sub.read_line(Duration::from_millis(500)).await {
            Some(l) => l,
            None => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("event") != Some(&serde_json::json!(true)) {
            continue;
        }
        let topic = v.get("topic").and_then(|t| t.as_str()).unwrap_or("");
        if topic == "sheaf.invalidate" {
            // Tag the cascade payload so a future shape change is
            // caught here, not in a downstream consumer.
            let data = v.get("data").expect("event payload missing `data`");
            let invalidated = data
                .get("invalidated")
                .and_then(|i| i.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            assert!(
                invalidated >= 1,
                "sheaf.invalidate event carried empty `invalidated` array \
                 — payload regression (compare to ley-line-open-d03e7d)",
            );

            // u64 fields render as JSON *strings* on the wire to match
            // capnp_json's op-response encoding (JS Number safe-integer
            // ceiling). A regression that emits these as raw numbers
            // forces consumers to handle two encodings for the same
            // field across response + event surfaces.
            let generation = data
                .get("generation")
                .expect("event payload missing `generation`");
            assert!(
                generation.is_string(),
                "sheaf.invalidate `generation` must be a JSON string \
                 (capnp_json u64 convention); got {generation:?}",
            );
            let prior_generation = data
                .get("prior_generation")
                .expect("event payload missing `prior_generation`");
            assert!(
                prior_generation.is_string(),
                "sheaf.invalidate `prior_generation` must be a JSON string \
                 (capnp_json u64 convention); got {prior_generation:?}",
            );

            found_topic = Some(topic.to_string());
            break;
        }
    }
    assert_eq!(
        found_topic.as_deref(),
        Some("sheaf.invalidate"),
        "ley-line-open-5caa59 regression: subscriber never received \
         sheaf.invalidate even though the daemon ran the cascade",
    );
}
