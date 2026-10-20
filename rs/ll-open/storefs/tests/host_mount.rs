//! The host mount through the kernel (bead `ley-line-open-3e73dc`): the
//! store mounted the way `daemon --mount` mounts it, on the platform's
//! default backend (our NFS server under `mount_nfs` on macOS, FUSE on
//! Linux), and driven with plain file syscalls. What a guest's file
//! operations land through its virtio-fs share, a host process's land here:
//! a save, a merge, a refusal of overlapping lines, a new file of any kind,
//! an empty directory, a mode, a symlink and a rename.
//!
//! The adapters' request-by-request behavior is tested below the kernel in
//! `src/host/tests.rs`; this file proves the kernel's clients reach it.
//!
//! One skip path, named: a macOS whose NFS client corrupts kernel memory is
//! refused by the mount itself (`host::nfs_client_refusal`), and the test
//! prints that refusal and returns. Any other runner that cannot mount
//! fails. No functional workload here loads the NFS client beyond a handful
//! of operations.

#![cfg(feature = "mount")]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use leyline_cli_lib::daemon::db_pool::LiveDb;
use leyline_cli_lib::source_write::{self, Base, Grant, Proposed};
use leyline_cli_lib::tree::{self as store_tree, Entry, Mode};
use leyline_storefs::host::{self, Mounted};
use leyline_storefs::{Options, Scope, Store, StoreFs};
use tempfile::TempDir;

const A: &str = "package a\n\nfunc A() int { return 1 }\n";
/// `A` with a tail line: an edit to other lines than `A_OTHER`'s.
const A_TAILED: &str = "package a\n\nfunc A() int { return 1 }\n// tail\n";
/// Another writer's change to `A`'s function line.
const A_OTHER: &str = "package a\n\nfunc A() int { return 3 }\n";
/// `A_OTHER` and `A_TAILED`, merged.
const A_MERGED: &str = "package a\n\nfunc A() int { return 3 }\n// tail\n";

/// How long the mount may take to appear in the mount table.
const ATTACH_DEADLINE: Duration = Duration::from_secs(10);

/// How long ONE kernel-facing call may block before the mount is declared
/// wedged. A call on a mount whose server stopped answering never returns,
/// so the deadline is enforced from outside the call.
const KERNEL_OP_DEADLINE: Duration = Duration::from_secs(30);

/// Run a kernel-facing call on its own thread and wait for it with a
/// deadline; on timeout force-unmount and fail naming the mount.
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

/// Force-unmount the mountpoint; a diagnosis line for the failure message.
fn force_unmount(mount: &Path) -> String {
    let unmount = if cfg!(target_os = "macos") {
        Command::new("umount").arg("-f").arg(mount).output()
    } else {
        Command::new("fusermount").arg("-uz").arg(mount).output()
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

/// Whether the kernel's mount table names `mount` as a mountpoint. Asks the
/// table, never the filesystem, so polling cannot disturb an attach.
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

/// Wait until `mount` is in the mount table, yielding between polls and
/// never sleeping.
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

/// A cold-parsed store over a source tree holding `a.go` and `pkg/b.go`.
struct Fixture {
    _db_dir: TempDir,
    _src: TempDir,
    fs: Arc<StoreFs<Arc<LiveDb>>>,
}

fn fixture() -> Fixture {
    let src = tempfile::tempdir().unwrap();
    for (rel, text) in [("a.go", A), ("pkg/b.go", "package pkg\n")] {
        let p = src.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    let db_dir = tempfile::tempdir().unwrap();
    let live = Arc::new(LiveDb::open_fresh_for_test(&db_dir.path().join("live.db")));
    live.with_write(|conn| {
        leyline_cli_lib::cmd_parse::parse_into_conn(conn, src.path(), None, None)?;
        leyline_cli_lib::checkout::ensure_tables(conn)
    })
    .unwrap();
    let scope = Scope {
        see: Grant::Whole,
        propose: Grant::Whole,
    };
    Fixture {
        _db_dir: db_dir,
        _src: src,
        fs: Arc::new(StoreFs::new(live, scope, Options::default())),
    }
}

impl Fixture {
    fn entry(&self, path: &str) -> Option<Entry> {
        self.fs
            .store()
            .with_read(|conn| store_tree::entry(conn, path))
            .unwrap()
    }

    fn stored(&self, path: &str) -> Option<String> {
        let Some(Entry::File { blob, .. }) = self.entry(path) else {
            return None;
        };
        let bytes = self
            .fs
            .store()
            .with_read(|conn| store_tree::blob_bytes(conn, blob))
            .unwrap()?;
        Some(String::from_utf8(bytes).unwrap())
    }

    fn other_writer(&self, path: &str, text: &str) {
        self.fs
            .store()
            .with_write(|conn| {
                source_write::propose(
                    conn,
                    &Grant::Whole,
                    vec![Proposed::write(path, Base::Any, text.as_bytes().to_vec())],
                )
            })
            .unwrap();
    }
}

/// A mount made the way the daemon makes one, attached before it is
/// returned; on a panic the mountpoint is force-unmounted first.
struct TestMount {
    mounted: Option<Mounted<Arc<LiveDb>>>,
    path: PathBuf,
    _dir: TempDir,
}

impl TestMount {
    async fn new(fs: Arc<StoreFs<Arc<LiveDb>>>) -> TestMount {
        let dir = TempDir::new().expect("create mountpoint");
        let path = dir.path().canonicalize().expect("canonical mountpoint");
        let backend = leyline_cli_lib::lifecycle::default_backend();
        // Port 0: the NFS server takes a free port.
        let mounted = host::mount(fs, &path, &backend, 0)
            .await
            .unwrap_or_else(|e| panic!("mount on {backend}: {e:#}"));
        wait_until_mounted(&path);
        TestMount {
            mounted: Some(mounted),
            path,
            _dir: dir,
        }
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }

    /// Unmount through the guard: the kernel flushes, the server lands
    /// what it staged.
    fn unmount(&mut self) {
        drop(self.mounted.take());
    }
}

impl Drop for TestMount {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("host_mount: cleaned up after a failure: {}", force_unmount(&self.path));
        }
        self.unmount();
    }
}

/// Replace `path`'s bytes as an editor's save does — `open(O_TRUNC)`,
/// write, `fsync`, `close` — and answer the first refusal.
fn save(path: PathBuf, text: &'static str) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    Ok(())
}

/// Every edit a host process makes through the mount lands as the same
/// proposal storefs makes for a guest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_process_edits_the_store_through_the_mount() {
    if let Some(refusal) = host::nfs_client_refusal().unwrap()
        && leyline_cli_lib::lifecycle::default_backend() == "nfs"
    {
        eprintln!("host_mount: SKIPPED on this host: {refusal}");
        return;
    }
    let f = fixture();
    let mut m = TestMount::new(Arc::clone(&f.fs)).await;

    // Reads: the store's files, as files.
    let a = m.at("a.go");
    let read = with_kernel_deadline("read", &m.path, move || fs::read_to_string(a).unwrap());
    assert_eq!(read, A);

    // A save over a disjoint change another writer landed merges.
    f.other_writer("a.go", A_OTHER);
    let a = m.at("a.go");
    with_kernel_deadline("disjoint save", &m.path, move || save(a, A_TAILED)).unwrap();
    assert_eq!(f.stored("a.go").as_deref(), Some(A_MERGED));

    // A save over an overlapping change is refused, and the store keeps the
    // other writer's bytes.
    let b = m.at("pkg/b.go");
    let read = with_kernel_deadline("read", &m.path, move || fs::read_to_string(b).unwrap());
    assert_eq!(read, "package pkg\n");
    f.other_writer("pkg/b.go", "package other\n");
    let b = m.at("pkg/b.go");
    let refused = with_kernel_deadline("overlapping save", &m.path, move || {
        save(b, "package mine\n")
    });
    assert!(refused.is_err(), "an overlapping save must fail at fsync or close");
    assert_eq!(f.stored("pkg/b.go").as_deref(), Some("package other\n"));

    // New entries of every kind.
    let root = m.path.clone();
    with_kernel_deadline("create, mkdir, chmod, symlink, rename", &m.path, move || {
        let mut notes = fs::File::create(root.join("NOTES.txt")).unwrap();
        notes.write_all(b"notes\n").unwrap();
        notes.sync_all().unwrap();
        drop(notes);
        fs::create_dir(root.join("empty")).unwrap();
        let mut run = fs::File::create(root.join("run.sh")).unwrap();
        run.write_all(b"#!/bin/sh\n").unwrap();
        run.sync_all().unwrap();
        drop(run);
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a.go", root.join("main.link")).unwrap();
        fs::rename(root.join("NOTES.txt"), root.join("pkg/NOTES.txt")).unwrap();
    });
    m.unmount();

    assert_eq!(f.stored("pkg/NOTES.txt").as_deref(), Some("notes\n"));
    assert_eq!(f.entry("NOTES.txt"), None);
    assert_eq!(f.entry("empty"), Some(Entry::Dir));
    assert!(matches!(
        f.entry("run.sh"),
        Some(Entry::File {
            mode: Mode::Executable,
            ..
        })
    ));
    assert!(matches!(
        f.entry("main.link"),
        Some(Entry::File {
            mode: Mode::Symlink,
            ..
        })
    ));
    assert_eq!(f.stored("main.link").as_deref(), Some("a.go"));
}
