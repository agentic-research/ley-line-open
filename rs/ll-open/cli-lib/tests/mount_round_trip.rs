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
//! serves a source file's whole bytes — FUSE reports the file row as a
//! directory of size 4096 (`fuse.rs::node_to_attr`), so `nodes.size` on that
//! row is unobservable through the mount and only its mtime reaches `stat`.
//!
//! This test parses a fixture, mounts it over FUSE the way the daemon does
//! (`SqliteGraphAdapter` under `Arc<dyn Graph>` handed to `mount_fuse`), and
//! asserts what the kernel hands back:
//!
//!   1. the bytes of a leaf read through the mount equal the graph's own
//!      `read_content` for that node AND equal the token in the source;
//!   2. the source file's entry is a directory whose `st_mtime` is the
//!      file's filesystem mtime — ca51fa's observable half. Reverting #381's
//!      `cmd_parse.rs` stamp (`mtime: now()`) fails this.
//!
//! The fixture's mtime is pinned to a fixed past instant so that a writer
//! stamping "now" cannot pass by sharing the second. There is no skip path:
//! a runner without FUSE fails, because a gate that silently passes where it
//! cannot run is the vacuous pass the feature-reachability gate exists to
//! prevent.

#![cfg(feature = "mount")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use leyline_fs::graph::{Graph, SqliteGraphAdapter};
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

/// Parse `src` into a projection and hand it back as the graph the daemon
/// mounts: a writable adapter over the serialized SQLite image.
fn projection_graph(src: &Path) -> Arc<dyn Graph> {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src, Some("go"), None)
        .expect("cold parse fixture");
    let image = conn.serialize("main").expect("serialize projection");
    let mut adapter =
        SqliteGraphAdapter::new_writable(image.as_ref()).expect("graph from projection image");
    adapter.set_source_writer(mount_writer());
    Arc::new(adapter)
}

/// The one write path (ADR-0040 D3), as the daemon and `serve` install it.
fn mount_writer() -> Arc<dyn leyline_fs::graph::SourceWriter> {
    Arc::new(leyline_cli_lib::source_write::MountWriter {
        grant: leyline_cli_lib::source_write::Grant::Whole,
    })
}

/// How long the mount may take to start serving once `mount_fuse` returns
/// (the attach is asynchronous on fuse-t).
const ATTACH_DEADLINE: Duration = Duration::from_secs(10);

/// How long ONE kernel-facing call may block before the mount is declared
/// wedged. A read or stat on a FUSE/NFS mount whose server has stopped
/// answering never returns — the kernel waits for the server — so no
/// deadline checked between calls can end it. This one is enforced from
/// outside the call (bead ley-line-open-667b3b).
const KERNEL_OP_DEADLINE: Duration = Duration::from_secs(30);

/// Run a kernel-facing filesystem call on its own thread and wait for it
/// with a deadline. On timeout the mount is torn down (its server killed,
/// the mountpoint force-unmounted) and the test fails with a diagnosis
/// naming the mount and the server, instead of hanging `task ci` until
/// someone notices. The blocked thread is released by the unmount.
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
            let diagnosis = teardown_mount(mount);
            panic!(
                "{what} on the mount at {} did not return within {KERNEL_OP_DEADLINE:?}: \
                 the FUSE mount is wedged. {diagnosis}",
                mount.display()
            );
        }
    }
}

/// fuse-t serves each mount through a `go-nfsv4 <mountpoint>` process; its
/// pids, if any, by exact mountpoint match.
fn fuse_server_pids(mount: &Path) -> Vec<u32> {
    let pattern = format!("go-nfsv4 {}", mount.display());
    Command::new("pgrep")
        .args(["-f", &pattern])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Kill the mount's server and force-unmount the mountpoint, so a failed
/// or killed run leaves nothing for the next run to trip on. Returns a
/// diagnosis line for the failure message. Idempotent: nothing to kill and
/// nothing mounted is a no-op.
fn teardown_mount(mount: &Path) -> String {
    let pids = fuse_server_pids(mount);
    for pid in &pids {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
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
        Ok(o) if o.status.success() => "unmounted".to_string(),
        Ok(o) => format!(
            "unmount exited {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => format!("unmount could not run: {e}"),
    };
    format!(
        "mountpoint {}; fuse-t server pid(s) {pids:?} killed; {unmount}.",
        mount.display()
    )
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

/// Wait until `mount` is attached. `mount_fuse` returns before the attach
/// completes on fuse-t (it is asynchronous). This used to be found by
/// spinning `read(2)` on a path under the mount until it succeeded — a
/// lookup storm against the mountpoint for the whole attach, which on
/// fuse-t intermittently left the path failing with EIO for the rest of the
/// test (bead ley-line-open-e5c8be). Now the mount table is polled instead
/// (hf-mount's readiness check), yielding between polls, never sleeping
/// (`sleep_in_tests`). `mount` must be canonical: the table holds the
/// resolved path (`/private/var/...` on macOS).
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

/// Whether `pid` is still running. libfuse-t spawns each mount's fuse-t
/// server as a child of this process and never reaps it, so an exited
/// server stays a zombie that `kill(pid, 0)` still finds: reap our own
/// children with `waitpid(WNOHANG)`, and probe anyone else's with
/// `kill(pid, 0)`.
fn alive(pid: u32) -> bool {
    let pid = pid as libc::pid_t;
    let mut status = 0;
    // SAFETY: waitpid with WNOHANG only reaps `pid` if it is our exited
    // child; `status` is a valid out-pointer.
    match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
        0 => true,
        r if r == pid => false,
        // Not our child (ECHILD): ask whether it exists at all.
        // SAFETY: kill(2) with signal 0 only checks existence.
        _ => unsafe { libc::kill(pid, 0) == 0 },
    }
}

/// Wait until every pid in `pids` has exited, polling with `kill(pid, 0)`
/// and yielding between polls. Past `ATTACH_DEADLINE` the survivors are
/// killed and the wait fails, naming `what`.
fn wait_for_exit(pids: &[u32], what: &str) {
    let deadline = Instant::now() + ATTACH_DEADLINE;
    while pids.iter().any(|&p| alive(p)) {
        if Instant::now() >= deadline {
            let left: Vec<u32> = pids.iter().copied().filter(|&p| alive(p)).collect();
            for pid in &left {
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            }
            let msg = format!("{what} {left:?} still running after {ATTACH_DEADLINE:?}; killed");
            if std::thread::panicking() {
                eprintln!("mount_round_trip: {msg}");
                return;
            }
            panic!("{msg}");
        }
        std::thread::yield_now();
    }
}

/// fuse-t servers left by an earlier run of these tests: orphaned (their
/// mounting process is gone, so they were re-parented to pid 1) and serving
/// a `TempDir` mountpoint under the system temp dir. Nothing else's servers
/// match, so a developer's own fuse-t mounts are never touched.
fn stale_test_servers() -> Vec<u32> {
    let tmp = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    let prefix = format!("go-nfsv4 {}/.tmp", tmp.display());
    let pids: Vec<u32> = Command::new("pgrep")
        .args(["-f", &prefix])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    pids.into_iter()
        .filter(|pid| {
            Command::new("ps")
                .args(["-o", "ppid=", "-p", &pid.to_string()])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
                .unwrap_or(false)
        })
        .collect()
}

/// SIGKILL each pid, then wait until each has exited (reaping our own).
fn stop_servers(pids: &[u32], what: &str) {
    for &pid in pids {
        // SAFETY: kill(2) on a fuse-t server pid this test owns or has
        // identified as a stale test server; no memory is involved.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    wait_for_exit(pids, what);
}

/// A FUSE mount made by a test, attached before it is returned.
///
/// fuse-t serves each mount through its own `go-nfsv4` server, and every
/// such server listens on 127.0.0.1:52100 unless that is taken (then 52101).
/// A mount made while an earlier 52100 server is still exiting — the one
/// that served the previous test, or the previous run, whose process has
/// just ended — reuses the address and port of a server that is dying, and
/// fuse-t's log showed that mount failing every time: reads returned EIO and
/// the server closed the FUSE channel under the session ("Short read of
/// FUSE request header (0 < 40)"). A failed run made it worse: its server,
/// killed at the deadline, failed the next run too. That was the macOS
/// mount-test flake (bead ley-line-open-e5c8be). fuse-t 1.2.1 cannot be
/// given another port: its `listen_addr` option moves the server but not
/// the port the kernel is told to mount. And a server does not exit when
/// its mount is unmounted: it lives until its mounting process exits, then
/// takes seconds more to go, so one run's servers were still exiting as the
/// next run's mounts came up. So a `TestMount` stops its own server once its
/// session has unmounted (it serves nothing by then), and before mounting
/// clears any stale server an earlier, crashed run left behind
/// ([`stale_test_servers`]). On a panic it first tears the mount down
/// (kills the server, force-unmounts), so a failed assertion cannot strand
/// a mount and its server.
struct TestMount<S> {
    session: Option<S>,
    servers: Vec<u32>,
    path: PathBuf,
    _dir: TempDir,
}

impl TestMount<()> {
    fn new(graph: Arc<dyn Graph>) -> TestMount<impl Sized> {
        init_log();
        stop_servers(&stale_test_servers(), "stale fuse-t test server(s)");
        let dir = TempDir::new().expect("create mountpoint");
        let path = dir.path().canonicalize().expect("canonical mountpoint");
        let session = leyline_fs::fuse::mount_fuse(graph, &path).expect("mount over FUSE");
        wait_until_mounted(&path);
        TestMount {
            session: Some(session),
            servers: fuse_server_pids(&path),
            path,
            _dir: dir,
        }
    }
}

impl<S> TestMount<S> {
    fn path(&self) -> &Path {
        &self.path
    }

    /// Unmount (the session's drop), then stop the mount's fuse-t server,
    /// which would otherwise outlive the mount until this process exits.
    fn unmount(&mut self) {
        if let Some(session) = self.session.take() {
            drop(session);
            stop_servers(&std::mem::take(&mut self.servers), "fuse-t server");
        }
    }
}

impl<S> Drop for TestMount<S> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let diagnosis = teardown_mount(&self.path);
            eprintln!("mount_round_trip: cleaned up after a failure: {diagnosis}");
        }
        self.unmount();
    }
}

/// Read `rel` through an attached mount (see [`wait_until_mounted`]), once,
/// under the kernel deadline.
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

#[test]
fn a_mounted_projection_serves_its_leaves_and_stats_its_files() {
    let src = create_fixture();
    let graph = projection_graph(src.path());

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

    let mounted = TestMount::new(graph);
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
/// the kernel deadline. The fsync is what makes the mount publish.
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
    let mut mounted = TestMount::new(graph);
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
        "the daemon's own readers see the mount's write"
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

/// The falsifier for the kernel deadline (bead ley-line-open-667b3b), run by
/// hand on a fuse-t machine: `LEYLINE_MOUNT_WEDGE=1 cargo test -p
/// leyline-cli-lib --features mount --test mount_round_trip -- --ignored
/// --nocapture`. It mounts, stops the fuse-t server with SIGSTOP so every
/// kernel call blocks, and asserts the read fails within the deadline with
/// the diagnosis — and that afterwards nothing is mounted and no server is
/// left. Without the deadline this test, like the gate, never finishes.
#[test]
#[ignore = "falsifier; needs fuse-t and LEYLINE_MOUNT_WEDGE=1"]
fn a_wedged_mount_fails_within_the_deadline_and_cleans_up() {
    if std::env::var_os("LEYLINE_MOUNT_WEDGE").is_none() {
        panic!("set LEYLINE_MOUNT_WEDGE=1 to run the wedge falsifier");
    }
    let src = create_fixture();
    let graph = projection_graph(src.path());
    // Attached before it is wedged.
    let mounted = TestMount::new(graph);
    let mount = mounted.path().to_path_buf();
    let _ = read_through_mount(&mount, LEAF);
    let pids = fuse_server_pids(&mount);
    assert!(
        !pids.is_empty(),
        "fuse-t server for {} not found",
        mount.display()
    );
    for pid in &pids {
        assert!(
            Command::new("kill")
                .args(["-STOP", &pid.to_string()])
                .status()
                .expect("kill -STOP")
                .success()
        );
    }

    let started = Instant::now();
    let outcome = std::panic::catch_unwind(|| read_through_mount(&mount, LEAF));
    let elapsed = started.elapsed();
    let msg = match outcome {
        Err(payload) => payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "<non-string panic>".to_string()),
        Ok(bytes) => panic!("a wedged mount served {bytes:?}; the server was not stopped"),
    };
    assert!(
        msg.contains("did not return within") && msg.contains("killed"),
        "the failure must carry the diagnosis; got: {msg}"
    );
    assert!(
        elapsed < KERNEL_OP_DEADLINE + Duration::from_secs(5),
        "failed after {elapsed:?}, past the deadline"
    );
    assert!(
        fuse_server_pids(&mount).is_empty(),
        "the wedged server must be gone after teardown"
    );
    // The session's Drop must return now that the mount is gone.
    drop(mounted);
}
