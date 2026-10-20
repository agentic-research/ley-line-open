//! The host mount below the kernel: the NFS adapter's requests answered by
//! [`StoreFs`] over a real store, the macOS NFS-client gate, and the mount
//! plumbing that needs no mount.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use leyline_cli_lib::daemon::db_pool::LiveDb;
use leyline_cli_lib::source_write::{self, Base, Grant, Proposed};
use leyline_cli_lib::tree::{self as store_tree, Entry as StoreEntry, Mode};
use nfsserve::nfs::{
    filename3, ftype3, nfspath3, nfsstat3, nfsstring, sattr3, set_mode3, set_size3,
};
use nfsserve::vfs::NFSFileSystem;
use tempfile::TempDir;

use super::nfs::{COMMIT_IDLE, HostNfs};
use super::*;
use crate::tests::{A, A_BROKEN, A_OTHER, Fixture, fixture};
use crate::{ROOT_INODE, StoreFs};

/// A tail line under `A`'s function: an edit to other lines than `A_OTHER`'s.
const A_TAILED: &str = "package a\n\nfunc A() int { return 1 }\n// tail\n";
/// `A_OTHER`'s change and `A_TAILED`'s, merged.
const A_MERGED: &str = "package a\n\nfunc A() int { return 3 }\n// tail\n";

/// The NFS adapter over a cold-parsed store holding `a.go`, `pkg/b.go`,
/// `pkg/sub/c.go` and `secret/d.go`.
struct Host {
    _db_dir: TempDir,
    _src: TempDir,
    fs: Arc<StoreFs<Arc<LiveDb>>>,
    nfs: HostNfs<Arc<LiveDb>>,
}

fn host() -> Host {
    let Fixture {
        _db_dir, src, fs, ..
    } = fixture(crate::tests::whole());
    let fs = Arc::new(fs);
    Host {
        _db_dir,
        _src: src,
        nfs: HostNfs::new(Arc::clone(&fs)),
        fs,
    }
}

fn name(text: &str) -> filename3 {
    nfsstring(text.as_bytes().to_vec())
}

impl Host {
    fn entry(&self, path: &str) -> Option<StoreEntry> {
        self.fs
            .store()
            .with_read(|conn| store_tree::entry(conn, path))
            .unwrap()
    }

    fn stored(&self, path: &str) -> Option<Vec<u8>> {
        let Some(StoreEntry::File { blob, .. }) = self.entry(path) else {
            return None;
        };
        self.fs
            .store()
            .with_read(|conn| store_tree::blob_bytes(conn, blob))
            .unwrap()
    }

    fn stored_text(&self, path: &str) -> Option<String> {
        self.stored(path).map(|b| String::from_utf8(b).unwrap())
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

    async fn id(&self, path: &str) -> u64 {
        let mut id = ROOT_INODE;
        for part in path.split('/') {
            id = self.nfs.lookup(id, &name(part)).await.unwrap();
        }
        id
    }

    /// READ the whole file, as a client does before it edits.
    async fn read_all(&self, id: u64) -> Vec<u8> {
        self.nfs.read(id, 0, u32::MAX).await.unwrap().0
    }

    /// Replace the file's bytes as a client's save does: SETATTR of the size
    /// to 0, an UNSTABLE WRITE of the new bytes, then (separately) COMMIT.
    async fn save(&self, id: u64, text: &str) -> Result<(), nfsstat3> {
        self.nfs
            .setattr(
                id,
                sattr3 {
                    size: set_size3::size(0),
                    ..sattr3::default()
                },
            )
            .await?;
        self.nfs.write(id, 0, text.as_bytes()).await.map(|_| ())
    }
}

// ── the macOS NFS-client gate ────────────────────────────────────────────

/// Tahoe before 26.7 and Sequoia before 15.8 are refused, naming the CVEs
/// and the fixed releases; those releases and every later major mount.
#[test]
fn an_unpatched_macos_nfs_client_is_refused_and_a_patched_one_mounts() {
    for refused in ["26.6.2", "26.0", "15.7.9", "15.0", "14.7.1", "16.0", "", "next"] {
        let refusal = unpatched_nfs_client(refused)
            .unwrap_or_else(|| panic!("macOS {refused:?} must be refused"));
        assert!(refusal.contains("CVE-2026-43686"), "{refusal}");
        assert!(refusal.contains("CVE-2026-84549"), "{refusal}");
        assert!(refusal.contains("26.7") && refusal.contains("15.8"), "{refusal}");
        assert!(refusal.contains(refused.trim()), "{refusal}");
    }
    for patched in ["26.7", "26.7.1", "26.10", "15.8", "15.8.1", "27.0.1", "28.0", " 27.0 "] {
        assert_eq!(unpatched_nfs_client(patched), None, "macOS {patched:?}");
    }
}

/// This Mac's product version reads as `major.minor[.patch]`.
#[cfg(target_os = "macos")]
#[test]
fn this_macs_product_version_reads_as_a_release_number() {
    let version = macos_product_version().unwrap();
    let major: u32 = version.split('.').next().unwrap().parse().unwrap();
    assert!(major >= 11, "{version}");
}

// ── mount plumbing ───────────────────────────────────────────────────────

/// The NFS export is unauthenticated and writable, and the mount reaches it
/// at 127.0.0.1, so it listens on loopback only.
#[test]
fn the_nfs_export_listens_on_loopback_only() {
    let addr: std::net::SocketAddr = nfs_listen_addr(2049).parse().unwrap();
    assert!(addr.ip().is_loopback(), "{addr}");
    assert_eq!(addr.port(), 2049);
}

/// Unmounting a directory nothing is mounted on fails, naming it, after
/// both `umount` and `umount -f` refuse.
#[test]
fn unmounting_what_is_not_mounted_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = unmount_nfs(dir.path()).unwrap_err();
    assert!(
        format!("{err:#}").contains(&dir.path().display().to_string()),
        "{err:#}"
    );
}

/// Dropping an NFS mount unmounts its own path, then stops the server and
/// lands what it staged: the daemon relies on the drop alone to leave no
/// mount and no unlanded write behind.
#[tokio::test]
async fn dropping_an_nfs_mount_unmounts_its_path_and_lands_what_is_staged() {
    use std::sync::Mutex;
    static UNMOUNTED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    fn record(path: &Path) -> anyhow::Result<()> {
        UNMOUNTED.lock().unwrap().push(path.to_path_buf());
        Ok(())
    }
    let h = host();
    let server = serve_nfs(Arc::clone(&h.fs), &nfs_listen_addr(0)).await.unwrap();
    assert_ne!(server.port(), 0, "port 0 picks one");
    let id = h.id("a.go").await;
    server.fs.read(id, 0, u32::MAX).await.unwrap();
    server.fs.write(id, 0, A_OTHER.as_bytes()).await.unwrap();
    let path = PathBuf::from("/nonexistent/leyline-test-mount");
    drop(NfsMount {
        path: path.clone(),
        unmount: record,
        _server: server,
    });
    assert_eq!(*UNMOUNTED.lock().unwrap(), vec![path]);
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_OTHER));
}

/// An unknown backend is refused by name, after the mountpoint is made.
#[tokio::test]
async fn an_unknown_backend_is_refused_by_name() {
    let h = host();
    let dir = tempfile::tempdir().unwrap();
    let at = dir.path().join("mnt");
    let err = match mount(Arc::clone(&h.fs), &at, "carrier-pigeon", 0).await {
        Ok(_) => panic!("an unknown backend must be refused"),
        Err(err) => err,
    };
    assert!(
        format!("{err:#}").contains("unknown backend: carrier-pigeon"),
        "{err:#}"
    );
    assert!(at.is_dir(), "the mountpoint is made before the backend is chosen");
}

// ── the NFS adapter: what storefs lands, a host client lands ─────────────

/// CREATE lands the new file at once, empty; a WRITE and COMMIT land its
/// bytes. A file of no language is a file like any other.
#[tokio::test]
async fn a_new_file_lands_at_create_and_its_bytes_at_commit() {
    let h = host();
    let (id, attr) = h
        .nfs
        .create(ROOT_INODE, &name("README.md"), sattr3::default())
        .await
        .unwrap();
    assert!(matches!(attr.ftype, ftype3::NF3REG));
    assert_eq!(h.stored("README.md"), Some(Vec::new()), "`touch` lands");
    h.nfs.write(id, 0, b"# notes\n").await.unwrap();
    assert_eq!(h.stored("README.md"), Some(Vec::new()), "a WRITE stages");
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("README.md").as_deref(), Some("# notes\n"));
}

/// CREATE of a name the store holds is `NFS3ERR_EXIST`.
#[tokio::test]
async fn creating_a_held_name_is_exist() {
    let h = host();
    let refused = h
        .nfs
        .create(ROOT_INODE, &name("a.go"), sattr3::default())
        .await;
    assert!(matches!(refused, Err(nfsstat3::NFS3ERR_EXIST)), "{refused:?}");
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A));
}

/// MKDIR lands an empty directory entry; RMDIR (a REMOVE of a directory)
/// removes it; a directory with entries under it is `NFS3ERR_NOTEMPTY`.
#[tokio::test]
async fn mkdir_and_rmdir_are_directory_entries() {
    let h = host();
    let (_, attr) = h.nfs.mkdir(ROOT_INODE, &name("empty")).await.unwrap();
    assert!(matches!(attr.ftype, ftype3::NF3DIR));
    assert_eq!(h.entry("empty"), Some(StoreEntry::Dir));
    h.nfs.remove(ROOT_INODE, &name("empty")).await.unwrap();
    assert_eq!(h.entry("empty"), None);
    let refused = h.nfs.remove(ROOT_INODE, &name("pkg")).await;
    assert!(matches!(refused, Err(nfsstat3::NFS3ERR_NOTEMPTY)), "{refused:?}");
}

/// `chmod +x` lands the executable mode; the bytes stay.
#[tokio::test]
async fn a_mode_change_lands_the_executable_mode() {
    let h = host();
    let id = h.id("a.go").await;
    let attr = h
        .nfs
        .setattr(
            id,
            sattr3 {
                mode: set_mode3::mode(0o755),
                ..sattr3::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(attr.mode & 0o111, 0o111, "{:o}", attr.mode);
    assert!(matches!(
        h.entry("a.go"),
        Some(StoreEntry::File {
            mode: Mode::Executable,
            ..
        })
    ));
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A));
}

/// SYMLINK lands a symlink whose blob is its target; READLINK answers it.
#[tokio::test]
async fn a_symlink_lands_with_its_target() {
    let h = host();
    let (id, attr) = h
        .nfs
        .symlink(
            ROOT_INODE,
            &name("main.link"),
            &nfspath3::from(b"a.go".to_vec()),
            &sattr3::default(),
        )
        .await
        .unwrap();
    assert!(matches!(attr.ftype, ftype3::NF3LNK));
    assert!(matches!(
        h.entry("main.link"),
        Some(StoreEntry::File {
            mode: Mode::Symlink,
            ..
        })
    ));
    assert_eq!(h.stored("main.link"), Some(b"a.go".to_vec()));
    assert_eq!(h.nfs.readlink(id).await.unwrap().0, b"a.go".to_vec());
}

/// A save against bytes another writer changed on other lines merges three
/// ways at its COMMIT.
#[tokio::test]
async fn a_save_over_a_disjoint_landed_change_merges_at_commit() {
    let h = host();
    let id = h.id("a.go").await;
    assert_eq!(h.read_all(id).await, A.as_bytes());
    h.other_writer("a.go", A_OTHER);
    h.save(id, A_TAILED).await.unwrap();
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_MERGED));
    assert_eq!(h.read_all(id).await, A_MERGED.as_bytes(), "a READ serves the merge");
}

/// A save that overlaps a landed change is `NFS3ERR_STALE` at the WRITE,
/// which the macOS client reports at `fsync` and `close`; the store keeps
/// the other writer's bytes.
#[tokio::test]
async fn a_save_over_an_overlapping_landed_change_is_stale_at_the_write() {
    let h = host();
    let id = h.id("a.go").await;
    h.read_all(id).await;
    h.other_writer("a.go", A_OTHER);
    let refused = h.save(id, &A.replace("return 1", "return 2")).await;
    assert!(matches!(refused, Err(nfsstat3::NFS3ERR_STALE)), "{refused:?}");
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_OTHER));
    // The save's truncate went with its WRITE: nothing is left to land.
    assert!(!h.nfs.commit_if_idle(Instant::now() + COMMIT_IDLE, COMMIT_IDLE));
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_OTHER));
}

/// A COMMIT refused for a change landed after the client's last WRITE drops
/// the staged edit: the client's next READ serves the store's bytes.
#[tokio::test]
async fn a_refused_commit_drops_the_edit_and_the_next_read_serves_the_store() {
    let h = host();
    let id = h.id("a.go").await;
    h.read_all(id).await;
    h.save(id, &A.replace("return 1", "return 2")).await.unwrap();
    h.other_writer("a.go", A_OTHER);
    let refused = h.nfs.commit(id, 0, 0).await;
    assert!(matches!(refused, Err(nfsstat3::NFS3ERR_STALE)), "{refused:?}");
    assert_eq!(h.read_all(id).await, A_OTHER.as_bytes());
}

/// A file left unparseable lands, with its diagnostic row.
#[tokio::test]
async fn an_unparseable_save_lands_with_its_diagnostic() {
    let h = host();
    let id = h.id("a.go").await;
    h.read_all(id).await;
    h.save(id, A_BROKEN).await.unwrap();
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_BROKEN));
    let diagnosed: i64 = h
        .fs
        .store()
        .with_read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM _parse_diagnostics WHERE path = 'a.go'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(diagnosed, 1);
}

/// A truncate with no WRITE after it has no COMMIT coming (`: > f`): it
/// lands once it has stood for the idle window, and not before.
#[tokio::test]
async fn a_lone_truncate_lands_once_it_has_stood_idle() {
    let h = host();
    let id = h.id("a.go").await;
    h.read_all(id).await;
    h.nfs
        .setattr(
            id,
            sattr3 {
                size: set_size3::size(0),
                ..sattr3::default()
            },
        )
        .await
        .unwrap();
    let now = Instant::now();
    assert!(!h.nfs.commit_if_idle(now, COMMIT_IDLE * 1000), "not yet idle");
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A));
    assert!(h.nfs.commit_if_idle(now + COMMIT_IDLE, COMMIT_IDLE));
    assert_eq!(h.stored("a.go"), Some(Vec::new()));
}

/// REMOVE deletes the file; RENAME moves it, and a rename over a file
/// replaces it (an editor's write-temp-then-rename save).
#[tokio::test]
async fn remove_and_rename_are_proposals() {
    let h = host();
    h.nfs.remove(ROOT_INODE, &name("a.go")).await.unwrap();
    assert_eq!(h.entry("a.go"), None);

    let pkg = h.id("pkg").await;
    h.nfs
        .rename(pkg, &name("b.go"), ROOT_INODE, &name("b.go"))
        .await
        .unwrap();
    assert_eq!(h.entry("pkg/b.go"), None);
    assert!(h.stored_text("b.go").is_some());

    let (tmp, _) = h
        .nfs
        .create(ROOT_INODE, &name(".b.go.swp"), sattr3::default())
        .await
        .unwrap();
    h.nfs.write(tmp, 0, b"package pkg\n").await.unwrap();
    h.nfs.commit(tmp, 0, 0).await.unwrap();
    h.nfs
        .rename(ROOT_INODE, &name(".b.go.swp"), ROOT_INODE, &name("b.go"))
        .await
        .unwrap();
    assert_eq!(h.entry(".b.go.swp"), None);
    assert_eq!(h.stored_text("b.go").as_deref(), Some("package pkg\n"));
}

/// READDIR lists what the store holds, files of any kind included, and
/// pages from the entry after `start_after`.
#[tokio::test]
async fn readdir_lists_the_store_and_pages() {
    let h = host();
    h.nfs
        .create(ROOT_INODE, &name("notes.txt"), sattr3::default())
        .await
        .unwrap();
    let all = h.nfs.readdir(ROOT_INODE, 0, usize::MAX).await.unwrap();
    assert!(all.end);
    let names: Vec<String> = all
        .entries
        .iter()
        .map(|e| String::from_utf8(e.name.0.clone()).unwrap())
        .collect();
    for held in ["a.go", "notes.txt", "pkg", "secret"] {
        assert!(names.iter().any(|n| n == held), "{held} in {names:?}");
    }
    let first = h.nfs.readdir(ROOT_INODE, 0, 1).await.unwrap();
    assert_eq!(first.entries.len(), 1);
    assert!(!first.end);
    let rest = h
        .nfs
        .readdir(ROOT_INODE, first.entries[0].fileid, usize::MAX)
        .await
        .unwrap();
    assert_eq!(rest.entries.len(), all.entries.len() - 1);
    let stale_cookie = h.nfs.readdir(ROOT_INODE, 9_999_999, 1).await;
    assert!(matches!(stale_cookie, Err(nfsstat3::NFS3ERR_BAD_COOKIE)));
}

/// LOOKUP of `.` is the directory and of `..` its parent.
#[tokio::test]
async fn dot_and_dotdot_resolve() {
    let h = host();
    let sub = h.id("pkg/sub").await;
    assert_eq!(h.nfs.lookup(sub, &name(".")).await.unwrap(), sub);
    assert_eq!(h.nfs.lookup(sub, &name("..")).await.unwrap(), h.id("pkg").await);
}

/// An owner other than the one presented is `NFS3ERR_PERM`.
#[tokio::test]
async fn an_owner_change_is_perm() {
    let h = host();
    let id = h.id("a.go").await;
    // SAFETY: `getuid(2)` cannot fail and touches no memory.
    let other = unsafe { libc::getuid() }.wrapping_add(1);
    let refused = h
        .nfs
        .setattr(
            id,
            sattr3 {
                uid: nfsserve::nfs::set_uid3::uid(other),
                ..sattr3::default()
            },
        )
        .await;
    assert!(matches!(refused, Err(nfsstat3::NFS3ERR_PERM)), "{refused:?}");
}

/// Each errno the filesystem answers reaches the client as its NFS status.
#[test]
fn each_errno_reaches_the_client_as_its_nfs_status() {
    use crate::Errno;
    for (errno, status) in [
        (Errno::EPERM, nfsstat3::NFS3ERR_PERM),
        (Errno::ENOENT, nfsstat3::NFS3ERR_NOENT),
        (Errno::EACCES, nfsstat3::NFS3ERR_ACCES),
        (Errno::EEXIST, nfsstat3::NFS3ERR_EXIST),
        (Errno::EXDEV, nfsstat3::NFS3ERR_XDEV),
        (Errno::ENOTDIR, nfsstat3::NFS3ERR_NOTDIR),
        (Errno::EISDIR, nfsstat3::NFS3ERR_ISDIR),
        (Errno::EINVAL, nfsstat3::NFS3ERR_INVAL),
        (Errno::ENOTEMPTY, nfsstat3::NFS3ERR_NOTEMPTY),
        (Errno::ESTALE, nfsstat3::NFS3ERR_STALE),
        (Errno::ENOSYS, nfsstat3::NFS3ERR_NOTSUPP),
        (Errno::EIO, nfsstat3::NFS3ERR_IO),
        (Errno::EBADF, nfsstat3::NFS3ERR_IO),
    ] {
        assert_eq!(
            super::nfs::nfs_status(errno) as u32,
            status as u32,
            "{errno:?}"
        );
    }
}

// ── the FUSE adapter's translations ──────────────────────────────────────

/// The Linux errno values that differ from the host's are translated; the
/// rest pass through.
#[test]
fn linux_errnos_reach_the_host_in_its_values() {
    use super::fuse::host_errno;
    use crate::Errno;
    assert_eq!(host_errno(Errno::ESTALE), libc::ESTALE);
    assert_eq!(host_errno(Errno::ENOTEMPTY), libc::ENOTEMPTY);
    assert_eq!(host_errno(Errno::ENOSYS), libc::ENOSYS);
    for same in [Errno::EPERM, Errno::ENOENT, Errno::EACCES, Errno::EEXIST, Errno::EISDIR] {
        assert_eq!(host_errno(same), same.0);
    }
}

/// The host's `open(2)` access mode and `O_TRUNC` reach the filesystem in
/// Linux's values; other flags are not carried.
#[test]
fn host_open_flags_reach_the_filesystem_in_linux_values() {
    use super::fuse::linux_open_flags;
    use crate::open_flags;
    assert_eq!(linux_open_flags(libc::O_RDONLY), open_flags::O_RDONLY);
    assert_eq!(linux_open_flags(libc::O_WRONLY), open_flags::O_WRONLY);
    assert_eq!(linux_open_flags(libc::O_RDWR), open_flags::O_RDWR);
    assert_eq!(
        linux_open_flags(libc::O_RDWR | libc::O_TRUNC),
        open_flags::O_RDWR | open_flags::O_TRUNC
    );
    assert_eq!(linux_open_flags(libc::O_WRONLY | libc::O_APPEND), open_flags::O_WRONLY);
}

/// macOS's `renamex_np(2)` flags reach the filesystem as Linux's
/// `renameat2(2)` flags; an unknown bit passes through to be refused.
#[cfg(target_os = "macos")]
#[test]
fn host_rename_flags_reach_the_filesystem_in_linux_values() {
    use super::fuse::linux_rename_flags;
    use crate::rename_flags;
    assert_eq!(linux_rename_flags(0), 0);
    assert_eq!(linux_rename_flags(0x4), rename_flags::RENAME_NOREPLACE);
    assert_eq!(linux_rename_flags(0x2), rename_flags::RENAME_EXCHANGE);
    assert_eq!(linux_rename_flags(0x10), 0x10);
}
