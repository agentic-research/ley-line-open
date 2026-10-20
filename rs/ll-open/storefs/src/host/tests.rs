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
    for refused in [
        "26.6.2", "26.0", "15.7.9", "15.0", "14.7.1", "16.0", "", "next",
    ] {
        let refusal = unpatched_nfs_client(refused)
            .unwrap_or_else(|| panic!("macOS {refused:?} must be refused"));
        assert!(refusal.contains("CVE-2026-43686"), "{refusal}");
        assert!(refusal.contains("CVE-2026-84549"), "{refusal}");
        assert!(
            refusal.contains("26.7") && refusal.contains("15.8"),
            "{refusal}"
        );
        assert!(refusal.contains(refused.trim()), "{refusal}");
    }
    for patched in [
        "26.7", "26.7.1", "26.10", "15.8", "15.8.1", "27.0.1", "28.0", " 27.0 ",
    ] {
        assert_eq!(unpatched_nfs_client(patched), None, "macOS {patched:?}");
    }
}

/// This Mac's product version reads as `major.minor[.patch]`.
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
    let server = serve_nfs(Arc::clone(&h.fs), &nfs_listen_addr(0))
        .await
        .unwrap();
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
    assert!(
        at.is_dir(),
        "the mountpoint is made before the backend is chosen"
    );
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
    assert!(
        matches!(refused, Err(nfsstat3::NFS3ERR_EXIST)),
        "{refused:?}"
    );
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
    assert!(
        matches!(refused, Err(nfsstat3::NFS3ERR_NOTEMPTY)),
        "{refused:?}"
    );
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
    assert_eq!(
        h.read_all(id).await,
        A_MERGED.as_bytes(),
        "a READ serves the merge"
    );
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
    assert!(
        matches!(refused, Err(nfsstat3::NFS3ERR_STALE)),
        "{refused:?}"
    );
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_OTHER));
    // The save's truncate went with its WRITE: nothing is left to land.
    assert!(
        !h.nfs
            .commit_if_idle(Instant::now() + COMMIT_IDLE, COMMIT_IDLE)
    );
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_OTHER));
    // The client has not read A_OTHER: a retried WRITE is checked against
    // what it read (A), so an overlapping retry is refused rather than
    // landing as an edit of bytes the client never saw.
    let retry = A.replace("return 1", "return 9");
    let retried = h.nfs.write(id, 0, retry.as_bytes()).await;
    assert!(
        matches!(retried, Err(nfsstat3::NFS3ERR_STALE)),
        "{retried:?}"
    );
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_OTHER));
    // Once it READs the store's bytes, its edit of them lands.
    assert_eq!(h.read_all(id).await, A_OTHER.as_bytes());
    let edit = A_OTHER.replace("return 3", "return 4");
    h.save(id, &edit).await.unwrap();
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(edit.as_str()));
}

/// A COMMIT refused for a change landed after the client's last WRITE drops
/// the staged edit: the client's next READ serves the store's bytes.
#[tokio::test]
async fn a_refused_commit_drops_the_edit_and_the_next_read_serves_the_store() {
    let h = host();
    let id = h.id("a.go").await;
    h.read_all(id).await;
    h.save(id, &A.replace("return 1", "return 2"))
        .await
        .unwrap();
    h.other_writer("a.go", A_OTHER);
    let refused = h.nfs.commit(id, 0, 0).await;
    assert!(
        matches!(refused, Err(nfsstat3::NFS3ERR_STALE)),
        "{refused:?}"
    );
    // A retried overlapping WRITE before any READ is refused: the base stays
    // what the client read.
    let retry = A.replace("return 1", "return 9");
    let retried = h.nfs.write(id, 0, retry.as_bytes()).await;
    assert!(
        matches!(retried, Err(nfsstat3::NFS3ERR_STALE)),
        "{retried:?}"
    );
    assert_eq!(h.read_all(id).await, A_OTHER.as_bytes());
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_OTHER));
}

/// DISCARD drops a handle's staged bytes and serves its base again, and a
/// created file's handle discards to empty.
#[test]
fn discard_drops_the_staged_bytes_and_keeps_the_base() {
    let h = host();
    let id = ino_of(&h, "a.go");
    let handle = h.fs.open(id, crate::open_flags::O_RDWR).unwrap().handle;
    h.fs.write(handle, 0, b"zzzz").unwrap();
    h.fs.discard(handle).unwrap();
    assert_eq!(h.fs.read(handle, 0, u32::MAX).unwrap(), A.as_bytes());
    assert_eq!(h.fs.flush(handle), Ok(()), "nothing staged to propose");
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A));

    let (_, created) =
        h.fs.create(ROOT_INODE, b"new.txt", crate::open_flags::O_RDWR)
            .unwrap();
    h.fs.write(created, 0, b"x").unwrap();
    h.fs.discard(created).unwrap();
    assert_eq!(h.fs.read(created, 0, u32::MAX).unwrap(), b"");
}

fn ino_of(h: &Host, path: &str) -> u64 {
    path.split('/').fold(ROOT_INODE, |parent, name| {
        h.fs.lookup(parent, name.as_bytes()).unwrap().attr.inode
    })
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
    let diagnosed: i64 =
        h.fs.store()
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
    assert!(
        !h.nfs.commit_if_idle(now, COMMIT_IDLE * 1000),
        "not yet idle"
    );
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
    assert_eq!(
        h.nfs.lookup(sub, &name("..")).await.unwrap(),
        h.id("pkg").await
    );
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
    assert!(
        matches!(refused, Err(nfsstat3::NFS3ERR_PERM)),
        "{refused:?}"
    );
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
    use super::fuse_abi::host_errno;
    use crate::Errno;
    assert_eq!(host_errno(Errno::ESTALE), libc::ESTALE);
    assert_eq!(host_errno(Errno::ENOTEMPTY), libc::ENOTEMPTY);
    assert_eq!(host_errno(Errno::ENOSYS), libc::ENOSYS);
    for same in [
        Errno::EPERM,
        Errno::ENOENT,
        Errno::EACCES,
        Errno::EEXIST,
        Errno::EISDIR,
    ] {
        assert_eq!(host_errno(same), same.0);
    }
}

/// The host's `open(2)` access mode and `O_TRUNC` reach the filesystem in
/// Linux's values; other flags are not carried.
#[test]
fn host_open_flags_reach_the_filesystem_in_linux_values() {
    use super::fuse_abi::linux_open_flags;
    use crate::open_flags;
    assert_eq!(linux_open_flags(libc::O_RDONLY), open_flags::O_RDONLY);
    assert_eq!(linux_open_flags(libc::O_WRONLY), open_flags::O_WRONLY);
    assert_eq!(linux_open_flags(libc::O_RDWR), open_flags::O_RDWR);
    assert_eq!(
        linux_open_flags(libc::O_RDWR | libc::O_TRUNC),
        open_flags::O_RDWR | open_flags::O_TRUNC
    );
    assert_eq!(
        linux_open_flags(libc::O_WRONLY | libc::O_APPEND),
        open_flags::O_WRONLY
    );
}

/// macOS's `renamex_np(2)` flags reach the filesystem as Linux's
/// `renameat2(2)` flags; an unknown bit passes through to be refused.
#[cfg(target_os = "macos")]
#[test]
fn host_rename_flags_reach_the_filesystem_in_linux_values() {
    use super::fuse_abi::{UNKNOWN_RENAME_FLAG, linux_rename_flags};
    use crate::rename_flags;
    assert_eq!(linux_rename_flags(0), 0);
    assert_eq!(linux_rename_flags(0x4), rename_flags::RENAME_NOREPLACE);
    assert_eq!(linux_rename_flags(0x2), rename_flags::RENAME_EXCHANGE);
    assert_eq!(
        linux_rename_flags(0x6),
        rename_flags::RENAME_NOREPLACE | rename_flags::RENAME_EXCHANGE
    );
    // RENAME_SECLUDE (0x1) is no RENAME_NOREPLACE, though Linux spells
    // NOREPLACE with that bit.
    assert_eq!(linux_rename_flags(0x1), UNKNOWN_RENAME_FLAG);
    assert_eq!(linux_rename_flags(0x10), UNKNOWN_RENAME_FLAG);
    assert_eq!(
        linux_rename_flags(0x5),
        UNKNOWN_RENAME_FLAG | rename_flags::RENAME_NOREPLACE
    );
}

/// A rename whose host flag has no Linux spelling is refused, not misread.
#[cfg(target_os = "macos")]
#[test]
fn a_rename_with_an_unknown_host_flag_is_refused() {
    use super::fuse_abi::linux_rename_flags;
    let f = fixture(crate::tests::whole());
    let refused = f.fs.rename(
        ROOT_INODE,
        b"a.go",
        ROOT_INODE,
        b"b.go",
        linux_rename_flags(0x1),
    );
    assert_eq!(refused, Err(crate::Errno::EINVAL));
    assert!(f.fs.lookup(ROOT_INODE, b"a.go").is_ok());
}

/// A node's FUSE attributes carry its inode, size, blocks, kind,
/// permissions, link count and time, and the owner they are given.
#[test]
fn a_nodes_fuse_attributes_carry_what_storefs_answers() {
    use super::fuse_abi::{BLKSIZE, file_attr};
    use crate::{Attr, Kind};
    use std::time::{Duration, UNIX_EPOCH};
    let attr = Attr {
        inode: 7,
        kind: Kind::File,
        size: 1025,
        perm: 0o755,
        mtime: Duration::new(1_760_000_000, 42),
    };
    let fa = file_attr(&attr, (501, 20));
    assert_eq!(fa.ino, 7);
    assert_eq!(fa.size, 1025);
    assert_eq!(fa.blocks, 3);
    assert_eq!(fa.mtime, UNIX_EPOCH + Duration::new(1_760_000_000, 42));
    assert_eq!(fa.atime, fa.mtime);
    assert_eq!(fa.ctime, fa.mtime);
    assert_eq!(fa.crtime, fa.mtime);
    assert_eq!(fa.kind, fuser::FileType::RegularFile);
    assert_eq!(fa.perm, 0o755);
    assert_eq!(fa.nlink, attr.nlink());
    assert_eq!((fa.uid, fa.gid), (501, 20));
    assert_eq!(fa.blksize, BLKSIZE);
}

/// Each node kind has its FUSE file type.
#[test]
fn each_kind_has_its_fuse_file_type() {
    use super::fuse_abi::file_type;
    use crate::Kind;
    assert_eq!(file_type(Kind::File), fuser::FileType::RegularFile);
    assert_eq!(file_type(Kind::Directory), fuser::FileType::Directory);
    assert_eq!(file_type(Kind::Symlink), fuser::FileType::Symlink);
}

/// A SETATTR naming the owner every node is presented with changes
/// nothing; any other owner is passed on, to be refused.
#[test]
fn only_another_owner_is_an_owner_change() {
    use super::fuse_abi::owner_change;
    let owner = (501, 20);
    assert_eq!(owner_change(None, None, owner), (None, None));
    assert_eq!(owner_change(Some(501), Some(20), owner), (None, None));
    assert_eq!(owner_change(Some(0), None, owner), (Some(0), None));
    assert_eq!(owner_change(None, Some(0), owner), (None, Some(0)));
    assert_eq!(
        owner_change(Some(502), Some(21), owner),
        (Some(502), Some(21))
    );
}

/// A CREATE whose mode grants execute stages that mode; any other lands
/// regular.
#[test]
fn a_create_that_grants_execute_stages_its_mode() {
    use super::fuse_abi::create_mode;
    assert_eq!(create_mode(0o755), Some(0o755));
    assert_eq!(create_mode(0o100), Some(0o100));
    assert_eq!(create_mode(0o001), Some(0o001));
    assert_eq!(create_mode(0o644), None);
    assert_eq!(create_mode(0o600), None);
}

/// Nodes are presented as owned by this process's user.
#[test]
fn nodes_are_owned_by_this_process() {
    use super::fuse_abi::process_owner;
    // SAFETY: `getuid(2)` and `getgid(2)` cannot fail and touch no memory.
    let want = unsafe { (libc::getuid(), libc::getgid()) };
    assert_eq!(process_owner(), want);
}

// ── the NFS adapter's attributes and owners ─────────────────────────────

/// A node's NFS attributes carry its type, permissions, size, file id and
/// time, owned by this process's user.
#[test]
fn a_nodes_nfs_attributes_carry_what_storefs_answers() {
    use crate::{Attr, Kind};
    use std::time::Duration;
    let attr = Attr {
        inode: 9,
        kind: Kind::Directory,
        size: 0,
        perm: 0o755,
        mtime: Duration::new(1_760_000_123, 456),
    };
    let fa = super::nfs::fattr(&attr);
    assert!(matches!(fa.ftype, ftype3::NF3DIR));
    assert_eq!(fa.mode, 0o755);
    assert_eq!(fa.fileid, 9);
    assert_eq!(fa.nlink, attr.nlink());
    assert_eq!((fa.mtime.seconds, fa.mtime.nseconds), (1_760_000_123, 456));
    assert_eq!((fa.atime.seconds, fa.ctime.nseconds), (1_760_000_123, 456));
    // SAFETY: `getuid(2)` and `getgid(2)` cannot fail and touch no memory.
    let owner = unsafe { (libc::getuid(), libc::getgid()) };
    assert_eq!((fa.uid, fa.gid), owner);
    let link = Attr {
        kind: Kind::Symlink,
        size: 4,
        ..attr
    };
    let fa = super::nfs::fattr(&link);
    assert!(matches!(fa.ftype, ftype3::NF3LNK));
    assert_eq!((fa.size, fa.used), (4, 4));
}

/// An NFS SETATTR naming the presented owner changes nothing; another
/// owner, user or group, is passed on to be refused.
#[test]
fn only_another_owner_is_an_nfs_owner_change() {
    use super::nfs::owner_change;
    use nfsserve::nfs::{set_gid3, set_uid3};
    // SAFETY: `getuid(2)` and `getgid(2)` cannot fail and touch no memory.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let (other_uid, other_gid) = (uid.wrapping_add(1), gid.wrapping_add(1));
    assert_eq!(owner_change(&set_uid3::Void, &set_gid3::Void), (None, None));
    assert_eq!(
        owner_change(&set_uid3::uid(uid), &set_gid3::gid(gid)),
        (None, None)
    );
    assert_eq!(
        owner_change(&set_uid3::uid(other_uid), &set_gid3::Void),
        (Some(other_uid), None)
    );
    assert_eq!(
        owner_change(&set_uid3::Void, &set_gid3::gid(other_gid)),
        (None, Some(other_gid))
    );
}

// ── the NFS adapter's remaining requests ─────────────────────────────────

/// The export's root is the store's root inode.
#[test]
fn the_exports_root_is_the_root_inode() {
    let h = host();
    assert_eq!(h.nfs.root_dir(), ROOT_INODE);
}

/// GETATTR answers the file's type and size; with nothing staged it
/// opens no handle and a COMMIT has nothing to land.
#[tokio::test]
async fn getattr_answers_the_store_and_a_clean_commit_lands_nothing() {
    let h = host();
    let id = h.id("a.go").await;
    let attr = h.nfs.getattr(id).await.unwrap();
    assert!(matches!(attr.ftype, ftype3::NF3REG));
    assert_eq!(attr.size, A.len() as u64);
    assert_eq!(attr.fileid, id);
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A));
    let pkg = h.id("pkg").await;
    let attr = h.nfs.getattr(pkg).await.unwrap();
    assert!(matches!(attr.ftype, ftype3::NF3DIR));
}

/// A READ reports EOF exactly when it reaches the end of the file.
#[tokio::test]
async fn a_read_reports_eof_at_the_end_of_the_file() {
    let h = host();
    let id = h.id("a.go").await;
    let (data, eof) = h.nfs.read(id, 0, 4).await.unwrap();
    assert_eq!(data, b"pack");
    assert!(!eof, "a read of the first bytes is not the end");
    let (data, eof) = h.nfs.read(id, 4, u32::MAX).await.unwrap();
    assert_eq!(data, &A.as_bytes()[4..]);
    assert!(eof);
    let (_, eof) = h.nfs.read(id, 2, 1).await.unwrap();
    assert!(!eof, "offset 2 + 1 byte is not the end");
    let (data, eof) = h.nfs.read(id, 1, u32::MAX).await.unwrap();
    assert_eq!(data.len(), A.len() - 1);
    assert!(eof, "offset 1 + the rest is the end");
}

/// A file removed through the mount and made again by another writer is a
/// new file to the mount: a WRITE through its id is an edit of the new
/// file's bytes, not of the bytes the removed file's handle read.
#[tokio::test]
async fn a_removed_files_handle_does_not_outlive_it() {
    let h = host();
    let id = h.id("a.go").await;
    h.read_all(id).await;
    h.nfs.remove(ROOT_INODE, &name("a.go")).await.unwrap();
    h.fs.store()
        .with_write(|conn| {
            source_write::propose(
                conn,
                &Grant::Whole,
                vec![Proposed::write(
                    "a.go",
                    Base::Absent,
                    A_OTHER.as_bytes().to_vec(),
                )],
            )
        })
        .unwrap();
    let id = h.id("a.go").await;
    // The same line A_OTHER changed: against the removed file's bytes (A)
    // this overlaps; against the new file's it is a plain edit.
    let edit = A.replace("return 1", "return 9");
    h.nfs.write(id, 0, edit.as_bytes()).await.unwrap();
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(edit.as_str()));
}

/// A WRITE after a truncate waits with it for the client's COMMIT: the
/// idle window lands nothing, the COMMIT lands both.
#[tokio::test]
async fn a_truncate_then_a_write_waits_for_the_commit() {
    let h = host();
    let id = h.id("a.go").await;
    h.read_all(id).await;
    h.save(id, A_TAILED).await.unwrap();
    h.nfs
        .setattr(
            id,
            sattr3 {
                size: set_size3::size(A_TAILED.len() as u64),
                ..sattr3::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !h.nfs
            .commit_if_idle(Instant::now() + COMMIT_IDLE, COMMIT_IDLE),
        "a written handle waits for its COMMIT, whatever was truncated after"
    );
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A));
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_TAILED));
}

/// CREATE with a mode that grants execute lands an executable file.
#[tokio::test]
async fn a_create_with_execute_lands_executable() {
    let h = host();
    let (_, attr) = h
        .nfs
        .create(
            ROOT_INODE,
            &name("run.sh"),
            sattr3 {
                mode: set_mode3::mode(0o755),
                ..sattr3::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(attr.mode & 0o111, 0o111, "{:o}", attr.mode);
    assert!(matches!(
        h.entry("run.sh"),
        Some(StoreEntry::File {
            mode: Mode::Executable,
            ..
        })
    ));
}

/// CREATE_EXCLUSIVE lands a new file, and answers the existing one for a
/// name the store holds.
#[tokio::test]
async fn create_exclusive_lands_a_file_or_answers_the_one_held() {
    let h = host();
    let id = h
        .nfs
        .create_exclusive(ROOT_INODE, &name("lock"))
        .await
        .unwrap();
    assert_eq!(id, h.id("lock").await);
    assert_eq!(h.stored("lock"), Some(Vec::new()));
    let held = h
        .nfs
        .create_exclusive(ROOT_INODE, &name("a.go"))
        .await
        .unwrap();
    assert_eq!(held, h.id("a.go").await);
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A));
}

/// A file removed and made again under its name is a new file: the old
/// handle, and the base it read, go with the removal.
#[tokio::test]
async fn a_file_made_again_under_a_removed_name_is_a_new_file() {
    let h = host();
    let id = h.id("a.go").await;
    h.read_all(id).await;
    h.nfs.remove(ROOT_INODE, &name("a.go")).await.unwrap();
    let (id, _) = h
        .nfs
        .create(ROOT_INODE, &name("a.go"), sattr3::default())
        .await
        .unwrap();
    h.nfs.write(id, 0, A_OTHER.as_bytes()).await.unwrap();
    h.nfs.commit(id, 0, 0).await.unwrap();
    assert_eq!(h.stored_text("a.go").as_deref(), Some(A_OTHER));
    assert_eq!(h.read_all(id).await, A_OTHER.as_bytes());
}

// ── mount plumbing below the kernel ─────────────────────────────────────

/// The NFS server answers on the port it reports.
#[tokio::test]
async fn the_nfs_server_listens_on_the_port_it_reports() {
    let h = host();
    let server = serve_nfs(Arc::clone(&h.fs), &nfs_listen_addr(0))
        .await
        .unwrap();
    let port = server.port();
    assert!(port > 1024, "an ephemeral port: {port}");
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap_or_else(|e| panic!("connect to the reported port {port}: {e}"));
}

/// The NFS mount options: NFSv3 over TCP without locks, 128 KiB transfers,
/// the one-second attribute cache, and the server's port for both the
/// mount and the NFS service.
#[test]
fn the_nfs_mount_options_name_the_servers_port() {
    assert_eq!(
        nfs_mount_options(4049),
        "nolocks,vers=3,tcp,rsize=131072,wsize=131072,actimeo=1,port=4049,mountport=4049"
    );
}

/// A mount command that exits non-zero is an error naming its exit code; one
/// that exits zero mounted.
#[test]
fn a_failed_mount_command_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = run_mount("false", "ro", dir.path()).unwrap_err();
    assert!(format!("{err:#}").contains("exit code Some(1)"), "{err:#}");
    run_mount("true", "ro", dir.path()).unwrap();
    let err = run_mount("/nonexistent/mount_nfs", "ro", dir.path()).unwrap_err();
    assert!(
        format!("{err:#}").contains("spawn /nonexistent/mount_nfs"),
        "{err:#}"
    );
}

/// The product version the gate decides on is the one `sw_vers` reports.
#[cfg(target_os = "macos")]
#[test]
fn the_gate_reads_this_macs_product_version() {
    let out = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .unwrap();
    let want = String::from_utf8(out.stdout).unwrap();
    assert_eq!(macos_product_version().unwrap(), want.trim());
}
