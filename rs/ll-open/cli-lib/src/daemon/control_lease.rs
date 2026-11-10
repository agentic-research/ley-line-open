//! Who may reset a control block, and the lock every daemon holds on it.
//!
//! Bead `ley-line-open-feabf4`. A control block written by another VERSION
//! is refused on a normal start: each VERSION gives the counter at `[8..16]`
//! a different meaning, and a v2 reader cannot see a v3 publish in flight.
//! `--reset-arena` reinitialises such a block at the current VERSION, but
//! only when no other process holds it. Two locks say whether one does:
//!
//! - **`<ctrl>.lock`, the lease.** Every running daemon holds it `flock`
//!   SHARED for its lifetime; a daemon started with `--reset-arena` takes it
//!   EXCLUSIVE, non-blocking, and keeps it exclusive for its lifetime. So a
//!   reset is refused while any daemon (or any reader that takes the lease)
//!   is live, and a normal start is refused while a reset holds it. The
//!   lease lives in a sibling file, not on the `.ctrl`, because the `.ctrl`
//!   lock is the seqlock's publish lock (`ley-line-open-0a8c14`): a daemon
//!   holding the `.ctrl` shared for its lifetime would block its own
//!   publishes, and make every reader probing for a dead publisher see a
//!   live one.
//! - **`<ctrl>.daemon.lock`, the daemon lock.** Every daemon holds it
//!   EXCLUSIVE, non-blocking, for its lifetime, so a control block has one
//!   daemon whatever `--arena` each names (bead `ley-line-open-1d4003`).
//!   The lease alone admitted two: both took it shared, the second
//!   published into the arena the block names while holding only its own
//!   arena's lock, and its socket bind replaced the first's `<ctrl>.sock`.
//!   It is a file of its own so readers, which share the lease, never
//!   conflict with it.
//! - **The `.ctrl` itself.** A reset also takes the `.ctrl` exclusive,
//!   non-blocking, for the length of the reset. That refuses while a
//!   publisher is mid-publish, and while any process that locks the block
//!   holds it.
//!
//! Daemons released before the lease hold neither lock at rest. A reset
//! therefore also probes the arena lock (`<arena>.lock`, held by every
//! daemon since `ley-line-open-0cba88`) of the arena the control block names:
//! a held lock means a daemon still serves through this block.
//!
//! What no lock can show is an older process that maps the block and takes
//! no lock at all. [`leyline_core::control::reinitialize`] covers it: the
//! fresh block is a new inode renamed into place, so such a process keeps
//! reading the old bytes at the old version and never misreads the new
//! version's seqlock; when it reopens the path, its own VERSION check
//! refuses the new block.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The prefix of every `--reset-arena` refusal caused by another holder of
/// the control block: its lease, a lock on the `.ctrl` itself, or the arena it
/// names. Stable: consumers key a retry on it (mache treats it as transient,
/// since its own brief publish and probe locks on the `.ctrl` can refuse a
/// concurrent reset). A refusal that retrying cannot fix (a symlinked control
/// path, an unopenable one) does not carry it.
pub const HELD_REFUSAL: &str = "--reset-arena refused: another process holds";

/// The locks a daemon holds on its control block for its lifetime: the
/// lease, and the block's daemon lock. Dropping it releases both; the files
/// stay, since unlinking a file another process may be locking would split
/// the lock in two.
#[derive(Debug)]
pub struct ControlLease {
    _file: File,
    _daemon: File,
    exclusive: bool,
}

impl ControlLease {
    /// Whether this daemon reset the control block, and so holds the lease
    /// exclusively.
    pub fn is_exclusive(&self) -> bool {
        self.exclusive
    }

    /// Take the lease on the control block at `ctrl` for a daemon serving
    /// `arena`, the arena whose admission lock this process already holds.
    ///
    /// Without `reset`: the lease shared. With `reset`: the lease exclusive,
    /// then, if the block exists, the `.ctrl` exclusive and the arena the
    /// block names probed; a block of another VERSION is then reinitialised
    /// at the current one, and the version it had is logged. Every lock is
    /// non-blocking: a held one refuses the start, naming what holds it.
    pub fn acquire(ctrl: &Path, arena: &Path, reset: bool) -> Result<Self> {
        let lease_path = lease_path_for(ctrl);
        if let Some(parent) = lease_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create control block dir {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lease_path)
            .with_context(|| format!("open control block lease {}", lease_path.display()))?;

        if !reset {
            if !try_flock(&file, libc::LOCK_SH)? {
                bail!(
                    "control block {} is being reset: another daemon holds its lease {} \
                     exclusively. Stop that daemon, or wait for it to exit, then retry.",
                    ctrl.display(),
                    lease_path.display()
                );
            }
            let daemon = take_daemon_lock(ctrl, false)?;
            return Ok(Self {
                _file: file,
                _daemon: daemon,
                exclusive: false,
            });
        }

        if !try_flock(&file, libc::LOCK_EX)? {
            bail!(
                "{HELD_REFUSAL} the control block {} \
                 (a running daemon or reader holds its lease {}). Stop every process \
                 using it, then retry.",
                ctrl.display(),
                lease_path.display()
            );
        }
        let daemon = take_daemon_lock(ctrl, true)?;
        // With the lease exclusive no other reset runs: a staging file left
        // beside the block is a crashed reset's.
        leyline_core::control::remove_stale_reinit_staging(ctrl).with_context(|| {
            format!("--reset-arena: remove stale staging of {}", ctrl.display())
        })?;
        reset_if_unheld(ctrl, arena)?;
        Ok(Self {
            _file: file,
            _daemon: daemon,
            exclusive: true,
        })
    }
}

/// The daemon lock for the control block at `ctrl`: `<ctrl>.daemon.lock`.
/// Every daemon on the block holds it exclusively for its lifetime, so the
/// block has one daemon (bead `ley-line-open-1d4003`). It is a file of its
/// own because the lease is shared by readers (mache's mapper holds
/// `<ctrl>.lock` shared), which a daemon taking the lease exclusively would
/// shut out.
pub fn daemon_lock_path_for(ctrl: &Path) -> PathBuf {
    let mut s = ctrl.as_os_str().to_owned();
    s.push(".daemon.lock");
    PathBuf::from(s)
}

/// Take the block's daemon lock exclusively, non-blocking. A second daemon
/// on the block, whatever arena it names, is refused: it would publish into
/// the arena the block names while holding only its own arena's lock, and
/// its socket bind would replace the first daemon's `<ctrl>.sock`.
fn take_daemon_lock(ctrl: &Path, reset: bool) -> Result<File> {
    let path = daemon_lock_path_for(ctrl);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open control block daemon lock {}", path.display()))?;
    if !try_flock(&file, libc::LOCK_EX)? {
        // Under a reset the lease is already exclusive, so a holder here
        // took this lock without the lease: still another holder, refused
        // with the stable prefix a retry keys on.
        if reset {
            bail!(
                "{HELD_REFUSAL} the control block {}'s daemon lock {}. Stop every process \
                 using it, then retry.",
                ctrl.display(),
                path.display()
            );
        }
        bail!(
            "another daemon serves the control block {} (it holds {}). A control block has \
             one daemon: stop that one, or give this daemon its own --control.",
            ctrl.display(),
            path.display()
        );
    }
    Ok(file)
}

/// The lease file for the control block at `ctrl`: `<ctrl>.lock`, the full
/// control path with `.lock` appended (`default.ctrl` -> `default.ctrl.lock`).
/// A cross-repo contract: a long-lived reader that maps the control block
/// (mache's `mount --control`) holds it `LOCK_SH` for its lifetime, so a
/// `--reset-arena` cannot replace the block under it.
pub fn lease_path_for(ctrl: &Path) -> PathBuf {
    let mut s = ctrl.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

/// With the lease held exclusively: refuse while anything else holds the
/// block, then reinitialise a block of another VERSION.
fn reset_if_unheld(ctrl: &Path, arena: &Path) -> Result<()> {
    let block = match OpenOptions::new().read(true).write(true).open(ctrl) {
        Ok(file) => file,
        // No block yet: `setup_arena` creates a fresh one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("open control block {}", ctrl.display()));
        }
    };
    // Held, on this descriptor, until this function returns: through the
    // reinitialise below. Released before the daemon opens its controller,
    // whose publish lock would otherwise conflict with it.
    if !try_flock(&block, libc::LOCK_EX)? {
        bail!(
            "{HELD_REFUSAL} a lock on the control block {} \
             (a publish in progress, or a reader that locks it). Stop every process using \
             it, then retry.",
            ctrl.display()
        );
    }

    let Some(header) = leyline_core::control::read_header(ctrl)? else {
        return Ok(());
    };
    if header.magic != leyline_core::control::MAGIC {
        // Not a control block, or a corrupt one: nothing here says it is
        // safe to overwrite. `open_or_create` names the magic it found.
        return Ok(());
    }
    refuse_if_named_arena_is_served(ctrl, &header.arena_path, arena)?;

    if header.version != leyline_core::control::VERSION {
        // The lock is on the descriptor; the rename works on the path. Tie
        // them: the path must still name, without a symlink, the inode
        // locked above, or the rename would replace something unchecked.
        use std::os::unix::fs::MetadataExt;
        let locked = block.metadata().context("stat the locked control block")?;
        let named = std::fs::symlink_metadata(ctrl)
            .with_context(|| format!("stat control block {}", ctrl.display()))?;
        if (named.dev(), named.ino()) != (locked.dev(), locked.ino()) {
            bail!(
                "--reset-arena refused: {} is not the control block file that was locked \
                 (a symlink, or replaced meanwhile). Pass the control block's own path.",
                ctrl.display()
            );
        }
        leyline_core::control::reinitialize(ctrl)
            .with_context(|| format!("--reset-arena: reinitialise {}", ctrl.display()))?;
        eprintln!(
            "--reset-arena: control block {} was v{}; reinitialised at v{} \
             (no other process held it)",
            ctrl.display(),
            header.version,
            leyline_core::control::VERSION
        );
    }
    Ok(())
}

/// A daemon released before the lease holds no lock on the control block at
/// rest, but holds its arena's admission lock. If the block names an arena
/// other than `ours` and that arena's lock is held, a daemon still serves
/// through this block.
fn refuse_if_named_arena_is_served(ctrl: &Path, named: &str, ours: &Path) -> Result<()> {
    if named.is_empty() {
        return Ok(());
    }
    let named = Path::new(named);
    // This process already holds its own arena's lock, which a second
    // descriptor would find held.
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    if canonical(named) == canonical(ours) {
        return Ok(());
    }
    let mut lock = named.as_os_str().to_owned();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    // A live daemon keeps its lock file; none means none holds it.
    let file = match OpenOptions::new().read(true).open(&lock) {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };
    if !try_flock(&file, libc::LOCK_EX)? {
        bail!(
            "{HELD_REFUSAL} the control block {}: the daemon \
             serving its arena {} is still running (it holds {}). Stop it, then retry.",
            ctrl.display(),
            named.display(),
            lock.display()
        );
    }
    Ok(())
}

/// `flock(how | LOCK_NB)`: `Ok(false)` when another descriptor holds a
/// conflicting lock, `Err` for any other failure.
fn try_flock(file: &File, how: libc::c_int) -> Result<bool> {
    // SAFETY: flock on a descriptor `file` owns for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), how | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(err).context("flock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(path: &Path, how: libc::c_int) -> File {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        assert!(try_flock(&file, how).unwrap(), "the test takes its lock");
        file
    }

    fn write_block(path: &Path, version: u32, arena: &str) {
        let mut block = vec![0u8; leyline_core::control::CONTROL_SIZE];
        block[0..4].copy_from_slice(&leyline_core::control::MAGIC.to_ne_bytes());
        block[4..8].copy_from_slice(&version.to_ne_bytes());
        block[16..16 + arena.len()].copy_from_slice(arena.as_bytes());
        std::fs::write(path, block).unwrap();
    }

    fn version(path: &Path) -> u32 {
        leyline_core::control::read_header(path)
            .unwrap()
            .unwrap()
            .version
    }

    /// A running daemon shares the lease with readers; a reset needs it
    /// alone, and holds it alone, refusing normal starts meanwhile.
    #[test]
    fn a_reset_excludes_running_daemons_and_is_excluded_by_them() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        let arena = dir.path().join("a.arena");

        let reader = hold(&lease_path_for(&ctrl), libc::LOCK_SH);
        let daemon = ControlLease::acquire(&ctrl, &arena, false).unwrap();
        assert!(!daemon.is_exclusive());
        let err = ControlLease::acquire(&ctrl, &arena, true).unwrap_err();
        assert!(
            format!("{err:#}").contains("another process holds"),
            "{err:#}"
        );

        drop((reader, daemon));
        let reset = ControlLease::acquire(&ctrl, &arena, true).unwrap();
        assert!(reset.is_exclusive());
        let err = ControlLease::acquire(&ctrl, &arena, false).unwrap_err();
        assert!(format!("{err:#}").contains("is being reset"), "{err:#}");
    }

    /// One daemon per control block (bead `ley-line-open-1d4003`): a second
    /// normal start on the same block is refused whatever arena it names,
    /// while a reader holding the lease shared is not, and the block's
    /// daemon lock frees when the first daemon exits.
    #[test]
    fn a_second_daemon_on_one_block_is_refused_and_readers_still_share() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");

        let first = ControlLease::acquire(&ctrl, &dir.path().join("a.arena"), false).unwrap();
        let reader = hold(&lease_path_for(&ctrl), libc::LOCK_SH);
        for arena in ["a.arena", "b.arena"] {
            let err = ControlLease::acquire(&ctrl, &dir.path().join(arena), false).unwrap_err();
            assert!(
                format!("{err:#}").contains("another daemon serves the control block"),
                "{arena}: {err:#}"
            );
        }
        drop(first);
        let _second = ControlLease::acquire(&ctrl, &dir.path().join("b.arena"), false).unwrap();
        drop(reader);
    }

    /// A reset that crashed between staging its fresh block and renaming it
    /// leaves `.<name>.reinit.<pid>`; the next reset removes it (bead
    /// `ley-line-open-40594e`), whatever the block's version, and leaves
    /// other siblings alone.
    #[test]
    fn a_reset_removes_a_crashed_resets_staging_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        let stale = dir.path().join(".c.ctrl.reinit.4242");
        let other = dir.path().join(".d.ctrl.reinit.4242");
        let lookalike = dir.path().join(".c.ctrl.reinit.x");
        for version in [2, leyline_core::control::VERSION] {
            write_block(&ctrl, version, "");
            for path in [&stale, &other, &lookalike] {
                std::fs::write(path, b"staged").unwrap();
            }
            let lease = ControlLease::acquire(&ctrl, &dir.path().join("a.arena"), true).unwrap();
            assert!(
                !stale.exists(),
                "v{version}: the stale staging file is gone"
            );
            assert!(other.exists(), "another block's staging file is not ours");
            assert!(lookalike.exists(), "not a pid suffix");
            drop(lease);
        }
    }

    /// The probe of the arena a block names is not held past the reset's
    /// check (bead `ley-line-open-40594e`): a daemon starting on that arena
    /// afterwards takes its admission lock.
    #[test]
    fn the_named_arena_probe_is_released_after_the_reset_checks_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        let theirs = dir.path().join("theirs.arena");
        write_block(&ctrl, 2, theirs.to_str().unwrap());
        // A lock file left by a daemon that has exited.
        std::fs::write(dir.path().join("theirs.arena.lock"), b"").unwrap();
        let _reset = ControlLease::acquire(&ctrl, &dir.path().join("ours.arena"), true).unwrap();
        crate::daemon::arena_lock::ArenaLock::try_acquire(&theirs)
            .expect("the probe does not hold the named arena's lock");
    }

    /// An older block with no holder is reinitialised; one whose `.ctrl` is
    /// locked is not.
    #[test]
    fn a_reset_reinitialises_an_older_block_only_when_unheld() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        let arena = dir.path().join("a.arena");
        write_block(&ctrl, 2, "");

        let held = hold(&ctrl, libc::LOCK_SH);
        let err = ControlLease::acquire(&ctrl, &arena, true).unwrap_err();
        assert!(format!("{err:#}").contains("holds a lock on the control block"));
        assert_eq!(version(&ctrl), 2);
        drop(held);

        let _lease = ControlLease::acquire(&ctrl, &arena, true).unwrap();
        assert_eq!(version(&ctrl), leyline_core::control::VERSION);
    }

    /// A daemon released before the lease holds only its arena's lock. A
    /// reset of a block naming that arena is refused while the lock is held,
    /// whatever the block's version.
    #[test]
    fn a_reset_is_refused_while_the_named_arena_is_served() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        let theirs = dir.path().join("theirs.arena");
        let ours = dir.path().join("ours.arena");
        for v in [2, leyline_core::control::VERSION] {
            write_block(&ctrl, v, theirs.to_str().unwrap());
            let served = crate::daemon::arena_lock::ArenaLock::try_acquire(&theirs).unwrap();
            let err = ControlLease::acquire(&ctrl, &ours, true).unwrap_err();
            assert!(
                format!("{err:#}").contains("is still running"),
                "v{v}: {err:#}"
            );
            assert_eq!(version(&ctrl), v, "untouched");
            drop(served);
        }
        // No daemon left on it: the reset proceeds.
        write_block(&ctrl, 2, theirs.to_str().unwrap());
        let _lease = ControlLease::acquire(&ctrl, &ours, true).unwrap();
        assert_eq!(version(&ctrl), leyline_core::control::VERSION);
    }

    /// The block naming this daemon's own arena is not a sign of another
    /// daemon: this process holds that arena's lock itself.
    #[test]
    fn a_reset_does_not_refuse_its_own_arena() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        let ours = dir.path().join("ours.arena");
        std::fs::write(&ours, b"").unwrap();
        write_block(&ctrl, 2, ours.to_str().unwrap());
        let _own = crate::daemon::arena_lock::ArenaLock::try_acquire(&ours).unwrap();
        let _lease = ControlLease::acquire(&ctrl, &ours, true).unwrap();
        assert_eq!(version(&ctrl), leyline_core::control::VERSION);
    }

    /// A control path that is a symlink is refused rather than replaced: the
    /// rename would swap the link, not the block it points at.
    #[test]
    fn a_reset_refuses_to_replace_a_symlinked_control_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("real.ctrl");
        write_block(&target, 2, "");
        let ctrl = dir.path().join("c.ctrl");
        std::os::unix::fs::symlink(&target, &ctrl).unwrap();
        let err = ControlLease::acquire(&ctrl, &dir.path().join("a.arena"), true).unwrap_err();
        assert!(
            format!("{err:#}").contains("not the control block file"),
            "{err:#}"
        );
        assert!(
            std::fs::symlink_metadata(&ctrl)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(version(&target), 2);
    }

    /// A file that is not a control block is never overwritten by a reset.
    #[test]
    fn a_reset_leaves_a_file_with_another_magic_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        let bytes = vec![7u8; leyline_core::control::CONTROL_SIZE];
        std::fs::write(&ctrl, &bytes).unwrap();
        let _lease = ControlLease::acquire(&ctrl, &dir.path().join("a.arena"), true).unwrap();
        assert_eq!(std::fs::read(&ctrl).unwrap(), bytes);
    }

    /// The lease's name is a cross-repo contract (mache's long-lived control
    /// block mapper holds it shared): the full control path plus `.lock`.
    #[test]
    fn the_lease_is_the_control_path_with_lock_appended() {
        assert_eq!(
            lease_path_for(Path::new("/home/u/.mache/default.ctrl")),
            PathBuf::from("/home/u/.mache/default.ctrl.lock")
        );
        assert_eq!(
            lease_path_for(Path::new("rel/d.ctrl")),
            PathBuf::from("rel/d.ctrl.lock")
        );
        // A reader holding it shared co-exists with a running daemon, and
        // refuses a reset.
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        let arena = dir.path().join("a.arena");
        let reader = hold(&dir.path().join("c.ctrl.lock"), libc::LOCK_SH);
        let _daemon = ControlLease::acquire(&ctrl, &arena, false).unwrap();
        assert!(ControlLease::acquire(&ctrl, &arena, true).is_err());
        drop(reader);
    }

    /// Every refusal caused by another holder starts with [`HELD_REFUSAL`],
    /// whose text is stable: mache keys a retry on it.
    #[test]
    fn every_held_refusal_starts_with_the_stable_prefix() {
        assert_eq!(HELD_REFUSAL, "--reset-arena refused: another process holds");
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        let ours = dir.path().join("ours.arena");
        let theirs = dir.path().join("theirs.arena");
        write_block(&ctrl, 2, theirs.to_str().unwrap());
        let mut refusals = Vec::new();

        // The lease, held by a running daemon.
        let daemon = ControlLease::acquire(&ctrl, &ours, false).unwrap();
        refusals.push(ControlLease::acquire(&ctrl, &ours, true).unwrap_err());
        drop(daemon);
        // A lock on the `.ctrl` itself.
        let publisher = hold(&ctrl, libc::LOCK_EX);
        refusals.push(ControlLease::acquire(&ctrl, &ours, true).unwrap_err());
        drop(publisher);
        // A pre-lease daemon serving the arena the block names.
        let served = crate::daemon::arena_lock::ArenaLock::try_acquire(&theirs).unwrap();
        refusals.push(ControlLease::acquire(&ctrl, &ours, true).unwrap_err());
        drop(served);

        for err in refusals {
            assert!(err.to_string().starts_with(HELD_REFUSAL), "{err:#}");
        }
        assert_eq!(version(&ctrl), 2, "every refusal left the block alone");
    }

    /// The lease lives beside the control block, whose directory a fresh
    /// `--control` may not have created yet.
    #[test]
    fn the_lease_creates_the_control_block_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("not/yet/c.ctrl");
        let _lease = ControlLease::acquire(&ctrl, &dir.path().join("a.arena"), false).unwrap();
        assert!(lease_path_for(&ctrl).exists());
    }

    /// A control path that exists but cannot be opened as a file is an
    /// error, not "no block yet".
    #[test]
    fn a_reset_refuses_a_control_path_it_cannot_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctrl = dir.path().join("c.ctrl");
        std::fs::create_dir(&ctrl).unwrap();
        let err = ControlLease::acquire(&ctrl, &dir.path().join("a.arena"), true).unwrap_err();
        assert!(format!("{err:#}").contains("open control block"), "{err:#}");
    }
}
