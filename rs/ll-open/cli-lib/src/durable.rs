//! What each flush a proposal issues promises (bead `ley-line-open-98d0c3`).
//!
//! An accepted proposal is durable when its rows are durable in `live.db`:
//! the writer's WAL commit at `synchronous = FULL` with `fullfsync`, one
//! `F_FULLFSYNC` on macOS ([`crate::cmd_daemon`]'s WAL configuration). That
//! commit is the acknowledgement. Everything else a proposal writes is a copy
//! of those rows that a later pass rebuilds, so it needs order, not its own
//! flush:
//!
//! - a checkout file's bytes reach the disk before the rename that installs
//!   it, so a power loss leaves the old file or the whole new one, never a
//!   file whose rename landed ahead of its data;
//! - the rename reaches the disk before the `_checkout_index` row that
//!   records it, so the index never claims a file the directory lost (which
//!   [`crate::checkout::reconcile`] would read as an edit and ingest, undoing
//!   the proposal);
//! - the Σ head's bytes reach the disk before its rename (never a torn head).
//!
//! [`order`] gives that: `F_BARRIERFSYNC` on macOS, which writes the file's
//! data and orders it before later I/O without flushing the drive's cache.
//! Elsewhere it is `fsync(2)`, the only ordering POSIX offers.
//!
//! The rows those copies record become durable at the next `F_FULLFSYNC`:
//! the next proposal's commit or a checkpoint. A power loss before then can
//! roll a copy back to its predecessor, which the next reconcile or parse
//! brings forward from the durable rows.

use std::fs::File;
use std::io;
use std::path::Path;

/// Write `file`'s data and order it before any later write, without
/// flushing the drive's cache: `F_BARRIERFSYNC` on macOS, `fsync(2)`
/// elsewhere.
pub fn order(file: &File, path: &Path) -> io::Result<()> {
    #[cfg(test)]
    record(Event::Order(path.to_path_buf()));
    #[cfg(not(test))]
    let _ = path;
    barrier(file)
}

/// [`order`] on the directory `dir`, so the entries renamed into it or
/// removed from it are ordered before any later write.
pub fn order_dir(dir: &Path) -> io::Result<()> {
    order(&File::open(dir)?, dir)
}

/// Rename `from` over `to`.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    record(Event::Rename(to.to_path_buf()));
    std::fs::rename(from, to)
}

/// One function with the platform split inside, so the body a mutation
/// test replaces is the one this platform runs (a `cfg`-gated second
/// function would be mutated without being compiled).
fn barrier(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `fcntl` with `F_BARRIERFSYNC` takes no pointer argument and
        // reads only the descriptor, which `file` keeps open for the call.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_all()
    }
}

/// Run `body` in one transaction whose commit does not sync the WAL
/// (`synchronous = NORMAL`), restoring the connection's level after. For
/// rows that only record what a durable commit already holds: a later
/// `F_FULLFSYNC` makes them durable, and losing them before then loses
/// nothing a later pass cannot rebuild. Inside an open transaction the
/// enclosing commit decides, so the level is left alone.
pub fn commit_unsynced<T>(
    conn: &rusqlite::Connection,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if !conn.is_autocommit() {
        return body();
    }
    let level: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    let committed = (|| {
        let tx = conn.unchecked_transaction()?;
        let out = body()?;
        #[cfg(test)]
        record(Event::Commit);
        tx.commit()?;
        Ok(out)
    })();
    conn.pragma_update(None, "synchronous", level)?;
    committed
}

/// One durability-relevant step, in the order a test thread issued them.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Order(std::path::PathBuf),
    Rename(std::path::PathBuf),
    Commit,
}

#[cfg(test)]
thread_local! {
    static EVENTS: std::cell::RefCell<Vec<Event>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record(event: Event) {
    EVENTS.with(|e| e.borrow_mut().push(event));
}

/// The steps this thread issued since the last call, oldest first.
#[cfg(test)]
pub fn take_events() -> Vec<Event> {
    EVENTS.with(|e| std::mem::take(&mut *e.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn order_writes_the_data_and_records_the_step() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"bytes").unwrap();
        take_events();
        order(&File::open(&path).unwrap(), &path).unwrap();
        order_dir(dir.path()).unwrap();
        assert_eq!(
            take_events(),
            [
                Event::Order(path.clone()),
                Event::Order(dir.path().to_path_buf())
            ]
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"bytes");
    }

    #[test]
    fn order_reports_a_descriptor_it_cannot_sync() {
        // A pipe has no data on a disk to order: the OS refuses the sync
        // (EBADF for F_BARRIERFSYNC on macOS, EINVAL for fsync on Linux),
        // and `order` must surface that error, not swallow it.
        let (_reader, writer) = std::io::pipe().unwrap();
        let pipe = File::from(std::os::fd::OwnedFd::from(writer));
        let err = order(&pipe, Path::new("pipe")).unwrap_err();
        #[cfg(target_vendor = "apple")]
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
        #[cfg(not(target_vendor = "apple"))]
        assert!(err.raw_os_error().is_some(), "{err}");
    }

    #[test]
    fn an_unsynced_commit_lands_its_rows_and_restores_the_level() {
        let dir = TempDir::new().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("db")).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "synchronous", "FULL").unwrap();
        conn.execute("CREATE TABLE t (v INTEGER)", []).unwrap();
        take_events();
        let seen = commit_unsynced(&conn, || {
            let level: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            conn.execute("INSERT INTO t VALUES (1)", [])?;
            Ok(level)
        })
        .unwrap();
        assert_eq!(seen, 1, "the body commits at NORMAL");
        let after: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, 2, "FULL is restored");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(take_events(), [Event::Commit]);
    }

    #[test]
    fn a_failed_unsynced_commit_rolls_back_and_restores_the_level() {
        let dir = TempDir::new().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("db")).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "synchronous", "FULL").unwrap();
        conn.execute("CREATE TABLE t (v INTEGER)", []).unwrap();
        let failed = commit_unsynced(&conn, || -> anyhow::Result<()> {
            conn.execute("INSERT INTO t VALUES (1)", [])?;
            anyhow::bail!("refused")
        });
        assert!(failed.is_err());
        let level: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(level, 2);
        let n: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "nothing of the failed body landed");
    }

    #[test]
    fn inside_an_open_transaction_the_level_is_left_alone() {
        let dir = TempDir::new().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("db")).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "synchronous", "FULL").unwrap();
        conn.execute("CREATE TABLE t (v INTEGER)", []).unwrap();
        let outer = conn.unchecked_transaction().unwrap();
        let seen = commit_unsynced(&conn, || {
            Ok(conn.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))?)
        })
        .unwrap();
        outer.commit().unwrap();
        assert_eq!(seen, 2);
    }
}
