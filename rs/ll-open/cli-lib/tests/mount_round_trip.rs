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
use std::path::Path;
use std::sync::Arc;
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

/// Read `rel` through the mount, polling the real condition until the kernel
/// serves it. `mount_fuse` returns before the mount is attached on fuse-t
/// (the attach is asynchronous), so the first lookups can miss; the same
/// `wait_for_uds` idiom applies — poll what we are waiting for, yield between
/// attempts, never sleep (`sleep_in_tests`). The deadline turns a mount that
/// never serves into a failure, not a hang.
fn read_through_mount(mount: &Path, rel: &str) -> Vec<u8> {
    let path = mount.join(rel);
    let deadline = Instant::now() + Duration::from_secs(10);
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
    let leaf_meta = fs::metadata(mount_dir.path().join(LEAF)).expect("stat the leaf");
    assert!(leaf_meta.is_file(), "a token leaf is a regular file");
    assert_eq!(
        leaf_meta.len(),
        LEAF_TOKEN.len() as u64,
        "a leaf's st_size is its token length"
    );

    // 2. The source file's entry: a directory of syntax nodes whose mtime is
    //    the file's own, not the parse time (ca51fa).
    let file_meta = fs::metadata(mount_dir.path().join("util.go")).expect("stat util.go");
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
