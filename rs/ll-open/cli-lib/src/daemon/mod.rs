//! Open daemon: UDS socket + event router + extensible ops.

pub mod arena_lock;
pub mod arena_owner;
pub mod auth;
pub mod client;
#[cfg(feature = "sheaf")]
pub mod complex_build_pass;
pub mod db_pool;
#[cfg(feature = "vec")]
pub mod embed;
pub mod enrichment;
pub mod events;
pub mod execution;
pub mod execution_contract;
pub mod ext;
#[cfg(feature = "hdc")]
pub mod hdc_enrich;
#[cfg(feature = "hdc")]
pub mod hdc_pass;
#[cfg(feature = "lsp")]
pub mod lsp_pass;
pub mod mcp;
pub mod observation_schema;
pub mod ops;
pub mod session_observation_pass;
#[cfg(feature = "sheaf")]
pub mod sheaf_ablation;
#[cfg(feature = "sheaf")]
pub mod sheaf_ops;
pub mod socket;
pub mod sql_read_only;
#[cfg(feature = "vec")]
pub mod vec_index;
pub mod version;
pub mod wire;

#[cfg(feature = "vec")]
use parking_lot::Mutex;
use parking_lot::RwLock;
#[cfg(feature = "vec")]
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

pub use events::EventRouter;

/// Where a running daemon's context is published for code that serves its
/// store outside the socket ([`DaemonExt::on_context`]); empty until the
/// daemon has built it.
pub type ContextSlot = Arc<std::sync::OnceLock<Arc<DaemonContext>>>;
pub use execution::ExecutionDaemonExt;
pub use ext::{DaemonExt, NoExt};

/// Wall-clock millis since UNIX_EPOCH. Used by the daemon's `last_*_ms`
/// state fields, embed-priority records, and pass-outcome timestamps —
/// anywhere we need a comparable timestamp that can survive a JSON
/// round-trip.
///
/// Returns 0 if the system clock is before 1970 (effectively impossible
/// in practice; the fallback exists so callers don't have to plumb a
/// Result through cold paths). Callers that need *monotonic* ordering
/// should use a separate counter — wall-clock is wrong for ordering
/// across NTP steps. See `daemon::embed::next_priority` for the
/// monotonic-counter pattern.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Lifecycle phase of the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonPhase {
    /// Bringing up the living db (warm-start deserialize or cold-start open).
    Initializing,
    /// Running the tree-sitter parse pass.
    Parsing,
    /// Running enrichment passes (LSP, embeddings, etc.).
    Enriching,
    /// Idle and ready to serve queries.
    Ready,
    /// Stuck — last lifecycle step failed. Message describes the failure.
    Error(String),
}

impl DaemonPhase {
    /// Short stable string for JSON serialization.
    pub fn as_str(&self) -> &str {
        match self {
            DaemonPhase::Initializing => "initializing",
            DaemonPhase::Parsing => "parsing",
            DaemonPhase::Enriching => "enriching",
            DaemonPhase::Ready => "ready",
            DaemonPhase::Error(_) => "error",
        }
    }
}

/// Status of one enrichment pass: when it last ran, the basis (parse_version it
/// ran against), and any recent error.
#[derive(Debug, Clone, Default)]
pub struct PassStatus {
    /// Wall-clock millis-since-epoch when the pass last completed.
    pub last_run_at_ms: Option<i64>,
    /// `parse_version` the pass last ran against (causal basis).
    pub basis: Option<u64>,
    /// Last error message, cleared on next successful run.
    pub error: Option<String>,
}

/// Snapshot of daemon lifecycle state, returned by `op_status`.
#[derive(Debug, Clone)]
pub struct DaemonState {
    pub phase: DaemonPhase,
    pub head_sha: Option<String>,
    pub last_reparse_at_ms: Option<i64>,
    pub enrichment: HashMap<String, PassStatus>,
}

impl DaemonState {
    pub fn initializing() -> Self {
        Self {
            phase: DaemonPhase::Initializing,
            head_sha: None,
            last_reparse_at_ms: None,
            enrichment: HashMap::new(),
        }
    }
}

/// Shared state passed to all op handlers.
pub struct DaemonContext {
    pub ctrl_path: PathBuf,
    pub ext: Arc<dyn DaemonExt>,
    pub router: Arc<EventRouter>,
    /// The living database — owned for the daemon's lifetime.
    /// File-backed WAL sqlite (bead `ley-line-open-98fb67` sub-bead 15a);
    /// reader pool + dedicated writer (bead `ley-line-open-f0239d` sub-
    /// bead 15b). All queries go through this via `with_read` (checks
    /// out a reader connection from the pool) or `with_write` (locks
    /// the exclusive writer). See `daemon::db_pool::LiveDb`.
    ///
    /// **Why `parking_lot::Mutex`, not `tokio::sync::Mutex`** (5fea4e):
    /// `rusqlite::Connection` is `!Send` (it holds a raw `*mut sqlite3`
    /// that's bound to the thread that opened it). `MutexGuard<Connection>`
    /// is therefore also `!Send`. Holding the guard across an `.await`
    /// would require Send for the future returned by the async block —
    /// which is a compile error. **Rust's type system enforces "no
    /// `.await` while holding the lock" for free.** Long-running operations
    /// (parse, enrich) MUST do their work synchronously while the guard is
    /// held; if the work needs to await, drop the guard first.
    ///
    /// The downside is that long sync work blocks the tokio worker thread.
    /// Mitigations:
    /// - The publisher (`cmd_daemon::publish_living_db`) reads through the
    ///   pool under its own lock, never the writer, and its dirty check
    ///   (`publish_if_dirty`) try_locks the writer and skips while it is
    ///   held, so it doesn't queue behind a long reparse.
    /// - Future op_reparse refactor: wrap the lock+work in
    ///   `tokio::task::spawn_blocking` to move the blocking work to the
    ///   blocking pool. Tracked as a follow-up under 5fea4e.
    pub live_db: db_pool::LiveDb,
    /// Per-file in-flight set for lazy LSP enrichment dedup (606e64).
    /// When `try_enrich_file(ctx, file)` is called and `file` is already
    /// in this set, the caller skips enrichment (the in-flight call will
    /// complete and populate `_lsp` rows for the next query).
    ///
    /// Without this gate, N concurrent `lsp_hover` calls on the same
    /// un-enriched file would each spawn an LSP server and write to
    /// `_lsp` (causing duplicate spawns, wasted work, and possible
    /// PRIMARY KEY collisions on the symbol_kind table).
    ///
    /// The gate is "skip-without-waiting" — concurrent callers get the
    /// current (possibly empty) data and must retry on the client side.
    /// A waiting variant (Notify-based) is more invasive and tracked as
    /// a follow-up; this fix prevents the duplicate-work pathology
    /// without introducing async coordination on the sync path.
    pub enrich_inflight: Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
    /// Source directory being tracked (if --source was given).
    pub source_dir: Option<PathBuf>,
    /// Language filter for parsing.
    pub lang_filter: Option<String>,
    /// Registered enrichment passes (tree-sitter + extension passes).
    pub enrichment_passes: Vec<Box<dyn enrichment::EnrichmentPass>>,
    /// Lifecycle state (phase, head_sha, last_reparse_at, per-pass status).
    /// Shared via `Arc` so background tasks and the run_daemon scope can both
    /// mutate it.
    pub state: Arc<RwLock<DaemonState>>,
    /// Sidecar vector index used by `op_vec_search`. Populated when the
    /// `vec` feature is enabled.
    #[cfg(feature = "vec")]
    pub vec_index: Arc<vec_index::VectorIndex>,
    /// Unstructured-text search engine used by `op_text_search`. Defaults
    /// to `NullEngine` (returns `Error::NotImplemented` so clients see a
    /// structured error, not "unknown op"); private extensions install a
    /// real engine via [`DaemonExt::text_search_engine`]. Independent
    /// from the single-vector `vec_search` path — the two are
    /// complementary retrieval surfaces.
    #[cfg(feature = "text-search")]
    pub text_search: Arc<dyn leyline_text_search::TextSearchEngine>,
    /// Embedder used to vectorize text (queries + node content). Defaults to
    /// `ZeroEmbedder`; private extensions override via `DaemonExt::embedder`.
    #[cfg(feature = "vec")]
    pub embedder: Arc<dyn embed::Embedder>,
    /// Priority queue of node ids to re-embed. Query ops push when a node is
    /// touched; the background drainer pops batches and refreshes the
    /// VectorIndex.
    #[cfg(feature = "vec")]
    pub embed_queue: Arc<Mutex<BinaryHeap<embed::EmbedTask>>>,
    /// Sheaf cache + co-change tracker for the `sheaf_*` ops. Topology
    /// is pushed in via `sheaf_set_topology`; invalidations come through
    /// `sheaf_invalidate`; defect / status / learned-weights are read-
    /// only queries. The cache emits `sheaf.topology` and
    /// `sheaf.invalidate` events on the bus once `EventRouter::emitter`
    /// is wired in via `sheaf.set_emitter(...)`.
    #[cfg(feature = "sheaf")]
    pub sheaf: Arc<sheaf_ops::SheafState>,
}

impl DaemonContext {
    /// Acquire a read-scoped connection from the reader pool.
    ///
    /// Bead `ley-line-open-f0239d` sub-bead 15b: reads check out a
    /// connection from the r2d2 pool with `query_only=ON`. Writes on
    /// the returned connection fail-loud with `SQLITE_READONLY`. The
    /// pool holds N shared reader connections (default `min(10,
    /// available_parallelism())`); concurrent readers make progress
    /// without serializing at the Rust level — that's what unlocks the
    /// ~600× p99 read improvement from the WAL experiment report.
    ///
    /// Use `with_read` for pure SELECT queries. Use `with_write` for
    /// anything that mutates (INSERT/UPDATE/DELETE/BEGIN/COMMIT/DDL).
    ///
    /// **Locking discipline (5fea4e)**: the returned `Connection` is
    /// held under a `!Send` guard for the closure's lifetime — do NOT
    /// `.await` inside the closure. If the work needs to await, drop
    /// the guard first, do the await, then reacquire.
    pub fn with_read<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T>,
    {
        // Pool checkout blocks up to r2d2's default `connection_timeout`
        // (30s) if all readers are in-flight. That's structurally
        // impossible in practice (pool size ≥ 2, connections return on
        // Drop at closure exit), but `?` surfaces the timeout cleanly
        // if it ever hits — better than a silent hang.
        let conn = self
            .live_db
            .reader_pool
            .get()
            .map_err(|e| anyhow::anyhow!("reader pool checkout failed: {e}"))?;
        f(&conn)
    }

    /// Acquire the exclusive writer connection.
    ///
    /// Bead `ley-line-open-f0239d` sub-bead 15b: writes hold the single
    /// `Mutex<Connection>` on the writer. SQLite WAL serializes writers
    /// at the file level; this `Mutex` mirrors that constraint at the
    /// Rust level. Concurrent `with_write` callers queue on the mutex,
    /// matching what SQLite would do at the file lock. See `with_read`
    /// for the read path.
    ///
    /// **Locking discipline (5fea4e)**: the returned `Connection` is
    /// held under a `!Send` guard for the closure's lifetime — do NOT
    /// `.await` inside the closure. If the work needs to await, drop
    /// the guard first, do the await, then reacquire.
    ///
    /// This is the live db's write boundary for events (bead
    /// `ley-line-open-851f00`): the writer's row-change count and schema
    /// version are read before and after `f`, and a difference marks the
    /// enclosing [`DaemonContext::observe_op`] as changed. Whether a closure
    /// wrote is what happened on the connection, not what the op is called.
    pub fn with_write<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T>,
    {
        let guard = self.live_db.writer.lock();
        let before = WriteMark::of(&guard);
        let out = f(&guard);
        if WriteMark::of(&guard).differs_from(before) {
            op_scope::note_change();
        }
        out
    }

    /// The arena's publish boundary for events (bead
    /// `ley-line-open-851f00`): run `publish`, which may replace the
    /// published arena, and mark the enclosing
    /// [`DaemonContext::observe_op`] as changed when the published root
    /// (`Controller.current_root`) moved. A publish of the bytes already
    /// published changes nothing a reader sees.
    pub(crate) fn with_publish<T>(&self, publish: impl FnOnce() -> Result<T>) -> Result<T> {
        let before = published_root(&self.ctrl_path);
        let out = publish();
        if root_moved(before, published_root(&self.ctrl_path)) {
            op_scope::note_change();
        }
        out
    }

    /// Run one daemon op and emit exactly one `daemon.<op>` event,
    /// `{"op": <op>}`, when a write boundary it crossed
    /// ([`DaemonContext::with_write`], [`DaemonContext::with_publish`])
    /// changed state; an op that changed nothing emits nothing (bead
    /// `ley-line-open-851f00`). Every transport's op dispatch runs through
    /// here, so the socket and MCP emit the same events.
    ///
    /// An op run inside another's scope folds its change into the outer op,
    /// which emits once. The scope is per thread: a write the op hands to
    /// another thread is that thread's own op (lazy enrichment observes
    /// itself as `enrich`).
    pub(crate) fn observe_op<T>(&self, op: &str, run: impl FnOnce() -> T) -> T {
        let scope = op_scope::Scope::enter();
        let out = run();
        if scope.exit() {
            self.router.emitter().emit(
                &format!("daemon.{op}"),
                "leyline",
                serde_json::json!({"op": op}),
            );
        }
        out
    }
}

/// What the live db's writer connection looks like to an observer of
/// changes: SQLite's row-change counter (INSERT/UPDATE/DELETE) and the
/// schema cookie (DDL). `None` when the schema cookie cannot be read; an
/// unreadable mark counts as a change, since a missed event leaves a
/// subscriber stale and a spurious one costs a re-read.
#[derive(Clone, Copy, PartialEq, Eq)]
struct WriteMark(Option<(u64, i64)>);

impl WriteMark {
    fn of(conn: &rusqlite::Connection) -> Self {
        let schema = conn.pragma_query_value(None, "schema_version", |r| r.get::<_, i64>(0));
        Self(schema.ok().map(|s| (conn.total_changes(), s)))
    }

    fn differs_from(self, before: Self) -> bool {
        // Exactly one unreadable side already differs; both unreadable is
        // the case the `is_none` covers.
        self.0.is_none() || self != before
    }
}

/// Whether a publish moved the published root. An unreadable controller on
/// either side counts as a move, for the reason [`WriteMark`] gives.
fn root_moved(before: Option<Option<[u8; 32]>>, after: Option<Option<[u8; 32]>>) -> bool {
    // As in `WriteMark::differs_from`: one unreadable side already differs.
    before.is_none() || before != after
}

/// The controller's published root, `Some(None)` before any publish, and
/// `None` when the controller cannot be read.
fn published_root(ctrl_path: &std::path::Path) -> Option<Option<[u8; 32]>> {
    leyline_core::Controller::open_or_create(ctrl_path)
        .ok()
        .map(|c| c.published_root())
}

/// The per-thread record of whether the running op changed state.
mod op_scope {
    use std::cell::Cell;

    thread_local! {
        /// `None` outside any op; `Some(changed)` inside one.
        static CHANGED: Cell<Option<bool>> = const { Cell::new(None) };
    }

    /// Mark the running op, if any, as changed.
    pub(super) fn note_change() {
        CHANGED.with(|c| {
            if c.get().is_some() {
                c.set(Some(true));
            }
        });
    }

    /// One op's scope. Dropping it (an unwinding op included) restores the
    /// enclosing scope and folds this op's change into it.
    pub(super) struct Scope {
        outer: Option<bool>,
    }

    impl Scope {
        pub(super) fn enter() -> Self {
            Self {
                outer: CHANGED.with(|c| c.replace(Some(false))),
            }
        }

        /// Leave the scope. True when this op changed state and is the
        /// outermost, so it is the one that reports the change.
        pub(super) fn exit(self) -> bool {
            let changed = CHANGED.with(|c| c.get()) == Some(true);
            let outermost = self.outer.is_none();
            drop(self);
            changed && outermost
        }
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            CHANGED.with(|c| {
                let inner = c.get() == Some(true);
                c.set(self.outer.map(|outer| outer || inner));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_status_default_is_all_none() {
        // PassStatus uses #[derive(Default)] — all three Option
        // fields must start as None. enrichment.rs:188 calls
        // or_insert_with(PassStatus::default) on first run of any
        // pass; a refactor that hand-rolled Default with phantom
        // values (e.g. last_run_at_ms: Some(0)) would silently make
        // every pass appear to have run before it actually did.
        let s = PassStatus::default();
        assert!(s.last_run_at_ms.is_none());
        assert!(s.basis.is_none());
        assert!(s.error.is_none());
    }

    #[test]
    fn daemon_state_initializing_factory_contract() {
        // DaemonState::initializing() is the canonical "fresh daemon"
        // factory — used at startup (cmd_daemon) and in fixtures
        // (ops, enrichment tests). Four call sites depend on the
        // exact starting state. Pin so a refactor that pre-seeded
        // any field (e.g. set phase=Ready prematurely, or planted a
        // stale head_sha) would surface immediately.
        let s = DaemonState::initializing();
        assert_eq!(s.phase, DaemonPhase::Initializing);
        assert!(s.head_sha.is_none());
        assert!(s.last_reparse_at_ms.is_none());
        assert!(s.enrichment.is_empty(), "enrichment map must start empty");
    }

    #[test]
    fn daemon_phase_as_str_pin() {
        // DaemonPhase::as_str() backs op_status's "phase" field —
        // clients depend on these exact strings ("ready" → daemon is
        // serving queries, "parsing" → wait, etc.). A refactor that
        // shortened or capitalized any variant would break the
        // client-side state machine. Pin all five.
        assert_eq!(DaemonPhase::Initializing.as_str(), "initializing");
        assert_eq!(DaemonPhase::Parsing.as_str(), "parsing");
        assert_eq!(DaemonPhase::Enriching.as_str(), "enriching");
        assert_eq!(DaemonPhase::Ready.as_str(), "ready");
        // Error variant carries a message but `as_str()` returns the
        // bare tag — the message is exposed elsewhere in op_status.
        assert_eq!(DaemonPhase::Error("oops".into()).as_str(), "error");
        assert_eq!(DaemonPhase::Error(String::new()).as_str(), "error");
    }

    /// The `daemon.*` topics `run` emits, fenced by a later event.
    async fn topics_of(ctx: &Arc<DaemonContext>, run: impl FnOnce()) -> Vec<String> {
        let (_id, mut rx, _replay, _gap) = ctx
            .router
            .subscribe(
                &["daemon.**".to_string(), "test.fence".to_string()],
                None,
                u64::MAX,
                events::OverflowPolicy::DropOldest,
                64,
            )
            .await;
        run();
        ctx.router
            .emitter()
            .emit("test.fence", "test", serde_json::json!({}));
        let mut topics = Vec::new();
        loop {
            let ev = rx.recv().await.unwrap();
            if ev.topic == "test.fence" {
                return topics;
            }
            topics.push(ev.topic);
        }
    }

    /// Bead `ley-line-open-851f00`: an op emits once however many write
    /// boundaries it crosses, and only when one of them changed state.
    #[tokio::test]
    async fn observe_op_emits_once_per_changing_op() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctx = socket::tests::test_context(dir.path());
        let write = |sql: &'static str| {
            ctx.with_write(|c| Ok(c.execute_batch(sql)?)).unwrap();
        };

        // Two changing writes and a nested op: one event, the outer op's.
        let topics = topics_of(&ctx, || {
            ctx.observe_op("outer", || {
                write("CREATE TABLE t (x)");
                ctx.observe_op("inner", || write("INSERT INTO t VALUES (1)"));
                write("INSERT INTO t VALUES (2)");
            })
        })
        .await;
        assert_eq!(topics, ["daemon.outer"]);

        // A nested op that changes state reports through the outer one.
        let topics = topics_of(&ctx, || {
            ctx.observe_op("outer", || {
                ctx.observe_op("inner", || write("INSERT INTO t VALUES (3)"))
            })
        })
        .await;
        assert_eq!(topics, ["daemon.outer"]);

        // Reads, and writes that change no row, emit nothing.
        let topics = topics_of(&ctx, || {
            ctx.observe_op("reader", || {
                write("DELETE FROM t WHERE x = 99");
                ctx.with_read(|c| {
                    Ok(c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))?)
                })
                .unwrap();
            })
        })
        .await;
        assert!(topics.is_empty(), "{topics:?}");

        // A write outside any op is the caller's to report.
        let topics = topics_of(&ctx, || write("INSERT INTO t VALUES (4)")).await;
        assert!(topics.is_empty(), "{topics:?}");

        // A publish that moves the published root is a change.
        let topics = topics_of(&ctx, || {
            ctx.observe_op("publish", || {
                ctx.with_publish(|| {
                    crate::cmd_daemon::publish_living_db(&ctx.live_db, &ctx.ctrl_path)
                })
                .unwrap()
            })
        })
        .await;
        assert_eq!(topics, ["daemon.publish"]);
        // The same bytes again move nothing.
        let topics = topics_of(&ctx, || {
            ctx.observe_op("publish", || {
                ctx.with_publish(|| {
                    crate::cmd_daemon::publish_living_db(&ctx.live_db, &ctx.ctrl_path)
                })
                .unwrap()
            })
        })
        .await;
        assert!(topics.is_empty(), "{topics:?}");
    }

    /// An unreadable side of either boundary's comparison counts as a
    /// change: a spurious event costs a re-read, a missed one a stale view.
    #[test]
    fn an_unreadable_mark_counts_as_a_change() {
        let mark = |m: Option<(u64, i64)>| WriteMark(m);
        assert!(!mark(Some((3, 1))).differs_from(mark(Some((3, 1)))));
        assert!(mark(Some((4, 1))).differs_from(mark(Some((3, 1)))));
        assert!(mark(Some((3, 2))).differs_from(mark(Some((3, 1)))));
        assert!(mark(None).differs_from(mark(Some((3, 1)))));
        assert!(mark(Some((3, 1))).differs_from(mark(None)));
        assert!(mark(None).differs_from(mark(None)));

        let root = Some(Some([7u8; 32]));
        assert!(!root_moved(root, root));
        assert!(!root_moved(Some(None), Some(None)));
        assert!(root_moved(Some(None), root));
        assert!(root_moved(root, Some(Some([8u8; 32]))));
        assert!(root_moved(None, root));
        assert!(root_moved(root, None));
        assert!(root_moved(None, None));
    }

    /// An op that unwinds leaves no scope behind: the next op on the thread
    /// is outermost again and reports its own change.
    #[tokio::test]
    async fn an_unwinding_op_restores_the_scope() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctx = socket::tests::test_context(dir.path());
        ctx.with_write(|c| Ok(c.execute_batch("CREATE TABLE t (x)")?))
            .unwrap();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.observe_op("boom", || -> () {
                ctx.with_write(|c| Ok(c.execute_batch("INSERT INTO t VALUES (1)")?))
                    .unwrap();
                panic!("op failed");
            })
        }));
        assert!(unwound.is_err());
        let topics = topics_of(&ctx, || {
            ctx.observe_op("after", || {
                ctx.with_write(|c| Ok(c.execute_batch("INSERT INTO t VALUES (2)")?))
                    .unwrap()
            })
        })
        .await;
        assert_eq!(topics, ["daemon.after"]);
    }

    #[test]
    fn now_ms_is_positive_after_unix_epoch() {
        // We are well past 1970 — now_ms() should always be a large
        // positive number. The unwrap_or(0) fallback only fires if
        // SystemTime::now() is *before* UNIX_EPOCH (impossible
        // outside a deliberately tampered system clock).
        let t = now_ms();
        assert!(t > 1_700_000_000_000, "now_ms should be > 2023, got {t}");
    }

    #[test]
    fn now_ms_is_monotonic_within_a_call_burst() {
        // Wall-clock isnt strictly monotonic across NTP steps, but two
        // calls in immediate succession on the same thread should not
        // observe a backwards step. If this test ever flakes it is
        // either an NTP step in the middle of CI (rare) or a serious
        // platform issue.
        let a = now_ms();
        let b = now_ms();
        let c = now_ms();
        assert!(a <= b && b <= c, "now_ms went backwards: {a} {b} {c}");
    }
}
