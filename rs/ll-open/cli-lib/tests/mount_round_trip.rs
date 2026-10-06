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
    Arc::new(SqliteGraphAdapter::new_writable(image.as_ref()).expect("graph from projection image"))
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
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(KERNEL_OP_DEADLINE) {
        Ok(v) => v,
        Err(_) => {
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

/// Tears the mount down if the test is unwinding (a panic anywhere after
/// the mount), so a failed assertion cannot strand a mount and its server.
/// On the normal path the session's own `Drop` unmounts and this does
/// nothing.
struct MountGuard {
    mount: PathBuf,
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let diagnosis = teardown_mount(&self.mount);
            eprintln!("mount_round_trip: cleaned up after a failure: {diagnosis}");
        }
    }
}

/// Read `rel` through the mount, polling the real condition until the kernel
/// serves it. `mount_fuse` returns before the mount is attached on fuse-t
/// (the attach is asynchronous), so the first lookups can miss; the same
/// `wait_for_uds` idiom applies — poll what we are waiting for, yield between
/// attempts, never sleep (`sleep_in_tests`). `ATTACH_DEADLINE` turns a mount
/// that never serves into a failure; the kernel deadline around the whole
/// poll turns a mount that never RETURNS into one.
fn read_through_mount(mount: &Path, rel: &str) -> Vec<u8> {
    let path = mount.join(rel);
    with_kernel_deadline("read", mount, move || {
        let deadline = Instant::now() + ATTACH_DEADLINE;
        loop {
            match fs::read(&path) {
                Ok(bytes) => return bytes,
                Err(_) if Instant::now() < deadline => std::thread::yield_now(),
                Err(e) => panic!(
                    "{} never became readable through the mount: {e}",
                    path.display()
                ),
            }
        }
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

    let mount_dir = TempDir::new().expect("create mountpoint");
    // Declared after `mount_dir` so it drops (unmounts) before the directory
    // is removed.
    let _session = leyline_fs::fuse::mount_fuse(graph, mount_dir.path())
        .expect("mount the projection over FUSE");
    // Declared after the session so it drops FIRST on a panic: force-unmount
    // and kill the server before the session's own Drop would wait on them.
    let _guard = MountGuard {
        mount: mount_dir.path().to_path_buf(),
    };

    // 1. Bytes through the kernel.
    let via_mount = read_through_mount(mount_dir.path(), LEAF);
    assert_eq!(
        via_mount, LEAF_TOKEN,
        "bytes read through the mount must be the source token"
    );
    assert_eq!(
        via_mount, via_graph,
        "the mount and read_content must agree"
    );
    let leaf_meta = stat_through_mount(mount_dir.path(), LEAF);
    assert!(leaf_meta.is_file(), "a token leaf is a regular file");
    assert_eq!(
        leaf_meta.len(),
        LEAF_TOKEN.len() as u64,
        "a leaf's st_size is its token length"
    );

    // 2. The source file's entry: a directory of syntax nodes whose mtime is
    //    the file's own, not the parse time (ca51fa).
    let file_meta = stat_through_mount(mount_dir.path(), "util.go");
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
    let mount_dir = TempDir::new().expect("create mountpoint");
    let mount = mount_dir.path().to_path_buf();
    let session = leyline_fs::fuse::mount_fuse(graph, &mount).expect("mount");
    // Let the attach complete before wedging it.
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
    drop(session);
}
