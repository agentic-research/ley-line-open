//! The mount round-trip (bead ley-line-open-aed167).
//!
//! `mount` shipped through `task install:full+mount` with no test at any
//! level: two build checks proved the FUSE and NFS backends link, and nothing
//! ever read a byte through a mounted projection. `flush_node` was a silent
//! no-op in every shipped build (ley-line-open-918a75) and every entry
//! stat'ed at parse time (ley-line-open-ca51fa); both are the same defect —
//! a shipped surface nobody exercised.
//!
//! What a mount presents, established by this test rather than assumed: the
//! projection's tree. A source file is a DIRECTORY whose children are its
//! syntax nodes (`util.go/package_clause/…`); the leaves are regular files
//! whose bytes are the token text in `nodes.record`. There is no entry that
//! serves a source file's whole bytes — the file row is presented as a
//! directory, so `nodes.size` on that row is unobservable through the mount
//! and only its mtime reaches `stat`.
//!
//! This test parses a fixture and mounts it exactly as the daemon does:
//! `cmd_serve::mount_backend` on the platform's default backend — our NFS
//! server under the kernel's `mount_nfs` on macOS, FUSE on Linux. It asserts
//! what the kernel hands back:
//!
//!   1. the bytes of a leaf read through the mount equal the graph's own
//!      `read_content` for that node AND equal the token in the source;
//!   2. the source file's entry is a directory whose `st_mtime` is the
//!      file's filesystem mtime — ca51fa's observable half. Reverting #381's
//!      `cmd_parse.rs` stamp (`mtime: now()`) fails this.
//!
//! On macOS these tests mounted over FUSE through fuse-t, a backend the
//! daemon does not use there, and every flake they had was fuse-t's: its
//! per-mount server always listened on port 52100 and outlived the unmount
//! by seconds, so the next mount collided with it (bead
//! ley-line-open-a384f4). The shipped macOS mount went untested by them.
//!
//! The fixture's mtime is pinned to a fixed past instant so that a writer
//! stamping "now" cannot pass by sharing the second. There is no skip path:
//! a runner that cannot mount fails, because a gate that silently passes
//! where it cannot run is the vacuous pass the feature-reachability gate
//! exists to prevent.

#![cfg(feature = "mount")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use leyline_cli_lib::cmd_serve::{Mounted, default_backend, mount_backend};
use leyline_fs::graph::{Graph, Node, SqliteGraphAdapter};
use tempfile::TempDir;

mod common;

/// 2001-09-09T01:46:40Z — exact to the second and a value no clock produces
/// during a test run.
const PINNED_MTIME_SECS: u64 = 1_000_000_000;

/// The leaf this test reads: `package main` → the `package_identifier`
/// token under the file's `package_clause`.
const LEAF: &str = "util.go/package_clause/package_identifier";
const LEAF_TOKEN: &[u8] = b"main";

/// Write a Go fixture and pin the file's mtime.
fn create_fixture() -> TempDir {
    let dir = TempDir::new().expect("create fixture dir");
    let path = dir.path().join("util.go");
    fs::write(
        &path,
        b"package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n",
    )
    .expect("write util.go");
    let file = fs::File::options()
        .write(true)
        .open(&path)
        .expect("reopen util.go");
    file.set_modified(UNIX_EPOCH + Duration::from_secs(PINNED_MTIME_SECS))
        .expect("pin util.go's mtime");
    dir
}

/// Parse `src` into a projection and hand it back as a graph to mount: a
/// writable adapter over the serialized SQLite image.
fn projection_graph(src: &Path) -> SqliteGraphAdapter {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src, Some("go"), None)
        .expect("cold parse fixture");
    let image = conn.serialize("main").expect("serialize projection");
    let mut adapter =
        SqliteGraphAdapter::new_writable(image.as_ref()).expect("graph from projection image");
    adapter.set_source_writer(mount_writer());
    adapter
}

/// The one write path (ADR-0040 D3), as the daemon and `serve` install it.
fn mount_writer() -> Arc<dyn leyline_fs::graph::SourceWriter> {
    Arc::new(leyline_cli_lib::source_write::MountWriter {
        grant: leyline_cli_lib::source_write::Grant::Whole,
        checkout: None,
    })
}

/// How long the mount may take to appear in the mount table once
/// `mount_backend` returns (a FUSE attach is asynchronous).
const ATTACH_DEADLINE: Duration = Duration::from_secs(10);

/// How long ONE kernel-facing call may block before the mount is declared
/// wedged. A read or stat on a FUSE/NFS mount whose server has stopped
/// answering never returns — the kernel waits for the server — so no
/// deadline checked between calls can end it. This one is enforced from
/// outside the call (bead ley-line-open-667b3b).
const KERNEL_OP_DEADLINE: Duration = Duration::from_secs(30);

/// Run a kernel-facing filesystem call on its own thread and wait for it
/// with a deadline. On timeout the mountpoint is force-unmounted and the
/// test fails with a diagnosis naming the mount, instead of hanging
/// `task ci` until someone notices. The blocked thread is released by the
/// unmount.
fn with_kernel_deadline<T: Send + 'static>(
    what: &str,
    mount: &Path,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = mpsc::channel();
    let call = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(KERNEL_OP_DEADLINE) {
        Ok(v) => v,
        // The call returned by panicking (a failed read, say): surface that
        // failure as it is, not as a wedge.
        Err(mpsc::RecvTimeoutError::Disconnected) => std::panic::resume_unwind(
            call.join()
                .expect_err("a call that sent nothing and ended has panicked"),
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let diagnosis = force_unmount(mount);
            panic!(
                "{what} on the mount at {} did not return within {KERNEL_OP_DEADLINE:?}: \
                 the mount is wedged. {diagnosis}",
                mount.display()
            );
        }
    }
}

/// Force-unmount the mountpoint, so a failed or wedged run leaves nothing
/// mounted. Returns a diagnosis line for the failure message. Idempotent:
/// nothing mounted is a no-op.
fn force_unmount(mount: &Path) -> String {
    let unmount = if cfg!(target_os = "macos") {
        Command::new("umount")
            .args(["-f", &mount.display().to_string()])
            .output()
    } else {
        Command::new("fusermount")
            .args(["-uz", &mount.display().to_string()])
            .output()
    };
    let unmount = match unmount {
        Ok(o) if o.status.success() => "force-unmounted".to_string(),
        Ok(o) => format!(
            "unmount exited {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => format!("unmount could not run: {e}"),
    };
    format!("mountpoint {}: {unmount}.", mount.display())
}

/// Whether the kernel's mount table names `mount` (canonical path) as a
/// mountpoint. This asks the mount table, not the filesystem: it touches
/// nothing under the mount, so polling it cannot disturb an attach in
/// progress.
#[cfg(target_os = "macos")]
fn is_mounted(mount: &Path) -> bool {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(mount.as_os_str().as_bytes()).expect("mount path has no NUL");
    // SAFETY: statfs(2) writes a `struct statfs` into the zeroed buffer we
    // own; `c` is a valid NUL-terminated path for the call's duration.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    // SAFETY: the kernel NUL-terminates f_mntonname within its fixed array.
    let on = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) };
    on.to_bytes() == mount.as_os_str().as_bytes()
}

#[cfg(not(target_os = "macos"))]
fn is_mounted(mount: &Path) -> bool {
    let want = mount.display().to_string();
    fs::read_to_string("/proc/mounts")
        .map(|t| {
            t.lines()
                .any(|l| l.split(' ').nth(1) == Some(want.as_str()))
        })
        .unwrap_or(false)
}

/// Wait until `mount` is in the mount table, polling the table (hf-mount's
/// readiness check) rather than the filesystem, yielding between polls and
/// never sleeping (`sleep_in_tests`). `mount_nfs` returns attached; a FUSE
/// attach completes after `mount_backend` returns. `mount` must be
/// canonical: the table holds the resolved path (`/private/var/...` on
/// macOS).
fn wait_until_mounted(mount: &Path) {
    let m = mount.to_path_buf();
    with_kernel_deadline("mount-table poll", mount, move || {
        let deadline = Instant::now() + ATTACH_DEADLINE;
        while !is_mounted(&m) {
            assert!(
                Instant::now() < deadline,
                "{} was not attached within {ATTACH_DEADLINE:?}",
                m.display()
            );
            std::thread::yield_now();
        }
    })
}

/// The mount's own warnings and errors — leyline-fs logs why it answered a
/// kernel call with EIO — go to stderr, which cargo prints for a failing
/// test. Without a logger they were discarded, and a failure through the
/// mount said only "Input/output error".
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, meta: &log::Metadata<'_>) -> bool {
        meta.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
        }
    }
    fn flush(&self) {}
}

fn init_log() {
    static LOG: StderrLog = StderrLog;
    if log::set_logger(&LOG).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
}

/// A mount made by a test the way the daemon makes one, attached before it
/// is returned. Unmounting drops the daemon's own guard ([`Mounted`]); on a
/// panic the mountpoint is first force-unmounted, so a failed assertion
/// cannot strand a mount.
struct TestMount {
    mounted: Option<Mounted>,
    path: PathBuf,
    _dir: TempDir,
}

impl TestMount {
    async fn new(graph: Arc<dyn Graph>) -> TestMount {
        init_log();
        let dir = TempDir::new().expect("create mountpoint");
        let path = dir.path().canonicalize().expect("canonical mountpoint");
        // Port 0: the NFS server takes a free port, so no mount can collide
        // with another's server.
        let mounted = mount_backend(graph, &path, &default_backend(), 0)
            .await
            .unwrap_or_else(|e| panic!("mount on {}: {e:#}", default_backend()));
        wait_until_mounted(&path);
        TestMount {
            mounted: Some(mounted),
            path,
            _dir: dir,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Unmount through the daemon's guard.
    fn unmount(&mut self) {
        drop(self.mounted.take());
    }
}

impl Drop for TestMount {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let diagnosis = force_unmount(&self.path);
            eprintln!("mount_round_trip: cleaned up after a failure: {diagnosis}");
        }
        self.unmount();
    }
}

/// Read `rel` through an attached mount, once, under the kernel deadline.
fn read_through_mount(mount: &Path, rel: &str) -> Vec<u8> {
    let path = mount.join(rel);
    with_kernel_deadline("read", mount, move || {
        fs::read(&path).unwrap_or_else(|e| panic!("read {} through the mount: {e}", path.display()))
    })
}

/// `stat(2)` through the mount, under the kernel deadline.
fn stat_through_mount(mount: &Path, rel: &str) -> fs::Metadata {
    let path = mount.join(rel);
    with_kernel_deadline("stat", mount, move || {
        fs::metadata(&path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
    })
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).expect("post-epoch").as_secs()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mounted_projection_serves_its_leaves_and_stats_its_files() {
    let src = create_fixture();
    let graph: Arc<dyn Graph> = Arc::new(projection_graph(src.path()));

    // What the graph says the leaf holds — the daemon's `read_content` op
    // reads exactly this. The mount must agree with it byte for byte.
    let mut via_graph = vec![0u8; 64];
    let n = graph
        .read_content(LEAF, &mut via_graph, 0)
        .expect("read_content on the leaf");
    via_graph.truncate(n);
    assert_eq!(
        via_graph, LEAF_TOKEN,
        "read_content on {LEAF} must return the source token"
    );

    let mounted = TestMount::new(graph).await;
    let mount = mounted.path().to_path_buf();

    // 1. Bytes through the kernel.
    let via_mount = read_through_mount(&mount, LEAF);
    assert_eq!(
        via_mount, LEAF_TOKEN,
        "bytes read through the mount must be the source token"
    );
    assert_eq!(
        via_mount, via_graph,
        "the mount and read_content must agree"
    );
    let leaf_meta = stat_through_mount(&mount, LEAF);
    assert!(leaf_meta.is_file(), "a token leaf is a regular file");
    assert_eq!(
        leaf_meta.len(),
        LEAF_TOKEN.len() as u64,
        "a leaf's st_size is its token length"
    );

    // 2. The source file's entry: a directory of syntax nodes whose mtime is
    //    the file's own, not the parse time (ca51fa).
    let file_meta = stat_through_mount(&mount, "util.go");
    assert!(
        file_meta.is_dir(),
        "a source file is presented as a directory of its syntax nodes"
    );
    assert_eq!(
        secs(file_meta.modified().expect("st_mtime")),
        PINNED_MTIME_SECS,
        "st_mtime of the source file's entry must be the file's filesystem mtime, \
         not the parse time"
    );
}

/// Write `data` to `rel` through the mount (truncate, write, fsync), under
/// the kernel deadline.
fn write_through_mount(mount: &Path, rel: &str, data: &'static [u8]) {
    let path = mount.join(rel);
    with_kernel_deadline("write", mount, move || {
        use std::io::Write;
        let mut f = fs::File::options()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap_or_else(|e| panic!("open {} for write: {e}", path.display()));
        f.write_all(data)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        f.sync_all()
            .unwrap_or_else(|e| panic!("fsync {}: {e}", path.display()));
    })
}

/// The leaf's `nodes.record` as the daemon's own readers see it.
fn leaf_record(conn: &rusqlite::Connection) -> Vec<u8> {
    let nid = leyline_schema::resolve_path(conn, LEAF)
        .expect("resolve leaf")
        .expect("leaf must exist");
    let record: String = conn
        .query_row("SELECT record FROM nodes WHERE nid = ?1", [nid], |r| {
            r.get(0)
        })
        .expect("leaf record");
    record.into_bytes()
}

/// `util.go`'s stored source bytes (`source_blobs`, ADR-0040's ring 0) as
/// the daemon's own readers see them. Unlike the leaf's record, which a
/// write updates at once, these change only when the write is committed.
fn stored_source(conn: &rusqlite::Connection) -> String {
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT b.blob_bytes FROM _source s \
             JOIN source_blobs b ON b.blob_hash = s.content_hash \
             WHERE s.id = 'util.go'",
            [],
            |r| r.get(0),
        )
        .expect("util.go's stored source");
    String::from_utf8(bytes).expect("util.go is UTF-8")
}

/// ADR-0040 D4, falsifier F2 (bead `ley-line-open-f2ee9f`; closes bead
/// `192018`): a write through the daemon's mount is durable. Before D4 the
/// mount edited a private copy of the arena and published from it, so the
/// daemon's next snapshot of `live.db` republished the old bytes, the mount
/// hot-swapped onto them and the edit was gone; a restart warm-started
/// without it. Now the mount writes through the daemon's single writer into
/// `live.db`, so after a reparse of ANOTHER file the edit is still what the
/// mount, the daemon's readers and a cold reopen of `live.db` all return.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mount_write_survives_a_reparse_of_another_file_and_a_restart() {
    let src = create_fixture();
    fs::write(
        src.path().join("other.go"),
        b"package main\n\nfunc other() int { return 1 }\n",
    )
    .expect("write other.go");

    let home = TempDir::new().expect("daemon dir");
    let ctx = common::daemon_context(home.path());
    let live_db_path = ctx.ctrl_path.with_extension("live.db");
    ctx.with_write(|conn| {
        leyline_cli_lib::cmd_parse::parse_into_conn(conn, src.path(), Some("go"), None).map(|_| ())
    })
    .expect("cold parse into live.db");

    let source = Arc::new(leyline_cli_lib::daemon::live_graph::LiveDbSource::new(
        ctx.clone(),
    ));
    let mut adapter = SqliteGraphAdapter::live(source).expect("live-backed adapter");
    adapter.set_source_writer(mount_writer());
    let graph: Arc<dyn Graph> = Arc::new(adapter);
    let mut mounted = TestMount::new(graph).await;
    let mount = mounted.path().to_path_buf();
    assert_eq!(
        read_through_mount(&mount, LEAF),
        LEAF_TOKEN,
        "baseline: the mount serves the parsed token"
    );

    const EDITED: &[u8] = b"edited";
    write_through_mount(&mount, LEAF, EDITED);
    assert_eq!(
        read_through_mount(&mount, LEAF),
        EDITED,
        "the mount reads back its own write"
    );

    // The fsync committed the write (NFS: the client's COMMIT; FUSE: the
    // fsync itself), so when it returns the daemon's readers see the edit
    // in the leaf and in the stored source. Before bead 3c4893 the NFS
    // server committed 200 ms after writes went quiet, and this waited.
    let (record, source) = ctx
        .with_read(|conn| Ok((leaf_record(conn), stored_source(conn))))
        .expect("daemon read");
    assert_eq!(
        String::from_utf8_lossy(&record),
        String::from_utf8_lossy(EDITED),
        "the daemon's readers see the mount's write"
    );
    assert!(
        source.starts_with("package edited\n"),
        "the stored source holds the write once its fsync returns; it is {source:?}"
    );

    // The daemon reparses a DIFFERENT file and republishes, which is exactly
    // the sequence that used to drop the edit.
    ctx.with_write(|conn| {
        leyline_cli_lib::cmd_parse::parse_into_conn(
            conn,
            src.path(),
            Some("go"),
            Some(&["other.go".to_string()]),
        )
        .map(|_| ())
    })
    .expect("scoped reparse of other.go");
    ctx.with_write(|conn| leyline_cli_lib::cmd_daemon::snapshot_to_arena(conn, &ctx.ctrl_path))
        .expect("snapshot after the reparse");

    assert_eq!(
        read_through_mount(&mount, LEAF),
        EDITED,
        "the edit survives a reparse of another file and a snapshot"
    );
    let via_daemon = ctx
        .with_read(|conn| Ok(leaf_record(conn)))
        .expect("daemon read");
    assert_eq!(
        via_daemon, EDITED,
        "the daemon's readers still see the mount's write after the reparse"
    );

    // Restart: unmount, drop the daemon context, reopen live.db cold.
    mounted.unmount();
    drop(ctx);
    let conn = rusqlite::Connection::open(&live_db_path).expect("reopen live.db");
    assert_eq!(
        leaf_record(&conn),
        EDITED,
        "a cold reopen of live.db still holds the mount's write"
    );
}

/// A graph that serves `inner` until it is wedged, then blocks every
/// content read until it is released — a server that has stopped
/// answering, in this process, on either backend.
struct Wedge {
    inner: SqliteGraphAdapter,
    wedged: Mutex<bool>,
    released: Condvar,
}

impl Wedge {
    fn set(&self, wedged: bool) {
        *self.wedged.lock().unwrap() = wedged;
        self.released.notify_all();
    }
}

impl Graph for Wedge {
    fn get_node(&self, id: &str) -> anyhow::Result<Option<Node>> {
        self.inner.get_node(id)
    }
    fn lookup_child(&self, parent_id: &str, name: &str) -> anyhow::Result<Option<Node>> {
        self.inner.lookup_child(parent_id, name)
    }
    fn list_children(&self, parent_id: &str) -> anyhow::Result<Vec<Node>> {
        self.inner.list_children(parent_id)
    }
    fn read_content(&self, id: &str, buf: &mut [u8], offset: u64) -> anyhow::Result<usize> {
        let mut wedged = self.wedged.lock().unwrap();
        while *wedged {
            wedged = self.released.wait(wedged).unwrap();
        }
        drop(wedged);
        self.inner.read_content(id, buf, offset)
    }
}

/// The falsifier for the kernel deadline (bead ley-line-open-667b3b), run
/// by hand: `LEYLINE_MOUNT_WEDGE=1 cargo test -p leyline-cli-lib --features
/// mount --test mount_round_trip -- --ignored --nocapture`. It mounts,
/// wedges the server so the kernel's read waits on it forever, and asserts
/// the read fails within the deadline with the diagnosis — and that
/// afterwards nothing is mounted. Without the deadline this test, like the
/// gate, never finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "falsifier; takes the full kernel deadline; needs LEYLINE_MOUNT_WEDGE=1"]
async fn a_wedged_mount_fails_within_the_deadline_and_cleans_up() {
    if std::env::var_os("LEYLINE_MOUNT_WEDGE").is_none() {
        panic!("set LEYLINE_MOUNT_WEDGE=1 to run the wedge falsifier");
    }
    let src = create_fixture();
    let wedge = Arc::new(Wedge {
        inner: projection_graph(src.path()),
        wedged: Mutex::new(false),
        released: Condvar::new(),
    });
    let mut mounted = TestMount::new(wedge.clone()).await;
    let mount = mounted.path().to_path_buf();
    // The leaf is not read before the wedge, so no client cache can answer.
    wedge.set(true);

    let started = Instant::now();
    let outcome = std::panic::catch_unwind(|| read_through_mount(&mount, LEAF));
    let elapsed = started.elapsed();
    let msg = match outcome {
        Err(payload) => payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "<non-string panic>".to_string()),
        Ok(bytes) => panic!("a wedged mount served {bytes:?}; the server was not wedged"),
    };
    assert!(
        msg.contains("did not return within") && msg.contains("force-unmounted"),
        "the failure must carry the diagnosis; got: {msg}"
    );
    assert!(
        elapsed < KERNEL_OP_DEADLINE + Duration::from_secs(5),
        "failed after {elapsed:?}, past the deadline"
    );
    assert!(
        !is_mounted(&mount),
        "the wedged mount must be gone after the teardown"
    );
    // Release the server's blocked read, then drop the guard: its unmount
    // finds nothing mounted and the drop returns.
    wedge.set(false);
    mounted.unmount();
}
