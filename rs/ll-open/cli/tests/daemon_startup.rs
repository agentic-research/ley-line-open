//! `leyline daemon` startup, run as the real binary under a temporary HOME.
//!
//! Two defects reported by mache, both on the path its auto-start takes:
//!
//! - `ley-line-open-e7459a`: a daemon on an explicit `--arena` repointed
//!   `~/.mache/default.sock` at its own socket and left it dangling when it
//!   exited. Only the default instance (`~/.mache/default.arena` with
//!   `~/.mache/default.ctrl`) may touch that name, and on exit it removes it
//!   only while the name still holds its own socket.
//! - `ley-line-open-feabf4`: a control block written by an older VERSION was
//!   refused even with `--reset-arena`. With the flag, and no other process
//!   holding the control block, the daemon reinitialises it at the current
//!   VERSION; a held control block keeps the refusal.
//!
//! Every daemon here runs with `HOME` set to a temporary directory, so the
//! real `~/.mache` is never read or written. Readiness is the daemon's own
//! `daemon ready` line on stderr, never a sleep.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// Bounds a wait that would otherwise hang the suite if the daemon wedges.
/// Every wait ends on an event (a line, an exit); this only caps it.
const GUARD: Duration = Duration::from_secs(120);

const MAGIC: u32 = 0x4C45_5943;

/// The stable prefix of a `--reset-arena` refusal caused by another holder,
/// spelled out rather than imported: consumers (mache) match this text.
const HELD_REFUSAL: &str = "--reset-arena refused: another process holds";
const CONTROL_SIZE: usize = 4096;

/// A temporary HOME and a separate temporary working directory.
struct Fixture {
    home: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".mache")).unwrap();
        Self {
            home,
            work: tempfile::TempDir::new().unwrap(),
        }
    }

    fn mache(&self, name: &str) -> PathBuf {
        self.home.path().join(".mache").join(name)
    }

    fn work(&self, name: &str) -> PathBuf {
        self.work.path().join(name)
    }

    /// `leyline daemon <args>` under this fixture's HOME.
    fn daemon(&self, args: &[&std::ffi::OsStr]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_leyline"));
        command
            .arg("daemon")
            .args(args)
            .env("HOME", self.home.path())
            .env("LLO_EMBEDDER", "zero")
            .env_remove("LEYLINE_SOCKET")
            .env_remove("LEYLINE_EXECUTION_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }
}

/// A daemon that reached `daemon ready`.
struct Running {
    child: Option<Child>,
    log: Receiver<String>,
    seen: Vec<String>,
}

impl Running {
    /// Send SIGTERM (what launchd, systemd and mache send) and wait for the
    /// exit, returning the status and all of stderr.
    fn stop(mut self) -> (ExitStatus, String) {
        let child = self.child.take().expect("running");
        // SAFETY: signalling a child this test spawned and has not reaped.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let status = wait_with_guard(child);
        // stderr closes at exit, which ends the reader.
        while let Ok(line) = self.log.recv_timeout(GUARD) {
            self.seen.push(line);
        }
        (status, self.seen.join("\n"))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Wait for `child` to exit, killing it if it outlives [`GUARD`].
fn wait_with_guard(mut child: Child) -> ExitStatus {
    let pid = child.id();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    match rx.recv_timeout(GUARD) {
        Ok(status) => status.expect("wait for the daemon"),
        Err(_) => {
            // SAFETY: the waiter thread has not reaped it (no status sent).
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            panic!("the daemon did not exit");
        }
    }
}

/// Spawn a stderr reader that forwards each line.
fn lines_of(child: &mut Child) -> Receiver<String> {
    let stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

/// Start a daemon and wait for its `daemon ready` line.
fn start(command: Command) -> Running {
    start_until(command, "daemon ready")
}

/// Start a daemon and wait for the first stderr line containing `marker`.
fn start_until(mut command: Command, marker: &str) -> Running {
    let mut child = command.spawn().expect("spawn leyline daemon");
    let log = lines_of(&mut child);
    let mut seen = Vec::new();
    loop {
        match log.recv_timeout(GUARD) {
            Ok(line) => {
                let ready = line.contains(marker);
                seen.push(line);
                if ready {
                    return Running {
                        child: Some(child),
                        log,
                        seen,
                    };
                }
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "the daemon never became ready; stderr:\n{}",
                    seen.join("\n")
                );
            }
        }
    }
}

/// Run a daemon that must refuse to start; return its stderr.
fn refused(mut command: Command) -> String {
    let mut child = command.spawn().expect("spawn leyline daemon");
    let log = lines_of(&mut child);
    let mut seen = Vec::new();
    // The stderr reader ends when the process closes stderr, i.e. exits.
    loop {
        match log.recv_timeout(GUARD) {
            Ok(line) => {
                if line.contains("daemon ready") {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "the daemon started; it must refuse. stderr:\n{}",
                        seen.join("\n")
                    );
                }
                seen.push(line);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                panic!("the daemon neither refused nor started");
            }
        }
    }
    let status = wait_with_guard(child);
    let stderr = seen.join("\n");
    assert!(
        !status.success(),
        "a refused start exits non-zero:\n{stderr}"
    );
    stderr
}

fn os(path: &Path) -> &std::ffi::OsStr {
    path.as_os_str()
}

/// A control block as a VERSION-`version` binary leaves it, naming `arena`.
fn write_control_block(path: &Path, version: u32, arena: &Path) {
    let mut block = vec![0u8; CONTROL_SIZE];
    block[0..4].copy_from_slice(&MAGIC.to_ne_bytes());
    block[4..8].copy_from_slice(&version.to_ne_bytes());
    let name = arena.as_os_str().as_encoded_bytes();
    block[16..16 + name.len()].copy_from_slice(name);
    std::fs::write(path, block).unwrap();
}

fn control_version(path: &Path) -> u32 {
    let bytes = std::fs::read(path).unwrap();
    u32::from_ne_bytes(bytes[4..8].try_into().unwrap())
}

/// Hold `flock(how)` on `path` through an open file description of the
/// test's own, which `flock` treats exactly as another process's.
fn hold_flock(path: &Path, how: libc::c_int) -> std::fs::File {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    // SAFETY: flock on a descriptor this function owns.
    let rc = unsafe { libc::flock(file.as_raw_fd(), how | libc::LOCK_NB) };
    assert_eq!(rc, 0, "the test takes its lock on {}", path.display());
    file
}

// --------------------------------------------------------------------------
// ley-line-open-e7459a: the default discovery link
// --------------------------------------------------------------------------

/// The reported repro: a daemon on an explicit arena under a temp dir must
/// leave `HOME/.mache/default.sock` exactly as it found it, while running and
/// after it exits.
#[test]
fn an_explicit_arena_daemon_leaves_the_default_socket_untouched() {
    let fx = Fixture::new();
    let default_sock = fx.mache("default.sock");
    let elsewhere = PathBuf::from("/nonexistent/another-daemon.sock");
    std::os::unix::fs::symlink(&elsewhere, &default_sock).unwrap();

    let daemon = start(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("x.arena")),
        "--control".as_ref(),
        os(&fx.work("x.ctrl")),
        "--reset-arena".as_ref(),
    ]));
    assert_eq!(
        std::fs::read_link(&default_sock).unwrap(),
        elsewhere,
        "a running explicit-arena daemon must not repoint default.sock"
    );
    assert!(
        fx.work("x.sock").exists(),
        "it serves on its own <ctrl>.sock"
    );

    let (status, log) = daemon.stop();
    assert!(status.success(), "{status}:\n{log}");
    assert_eq!(
        std::fs::read_link(&default_sock).unwrap(),
        elsewhere,
        "an exiting explicit-arena daemon must not remove or repoint default.sock"
    );
    assert!(!fx.work("x.sock").exists(), "it removes its own socket");
}

/// An explicit arena paired with the default instance's control block would
/// bind `default.sock` itself, which an explicit instance never does.
#[test]
fn an_explicit_arena_on_the_default_control_block_is_refused() {
    let fx = Fixture::new();
    let default_sock = fx.mache("default.sock");
    let elsewhere = PathBuf::from("/nonexistent/another-daemon.sock");
    std::os::unix::fs::symlink(&elsewhere, &default_sock).unwrap();

    let stderr = refused(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("x.arena")),
        "--control".as_ref(),
        os(&fx.mache("default.ctrl")),
    ]));
    assert!(stderr.contains("default.ctrl"), "{stderr}");
    assert!(stderr.contains("default instance"), "{stderr}");
    assert_eq!(std::fs::read_link(&default_sock).unwrap(), elsewhere);
}

/// The default instance (mache's own spawn: `--arena ~/.mache/default.arena
/// --control ~/.mache/default.ctrl`) serves at `default.sock`, and removes it
/// on a clean exit.
#[test]
fn the_default_instance_removes_its_own_socket_on_exit() {
    let fx = Fixture::new();
    let daemon = start(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.mache("default.arena")),
        "--control".as_ref(),
        os(&fx.mache("default.ctrl")),
    ]));
    let default_sock = fx.mache("default.sock");
    assert!(
        std::fs::symlink_metadata(&default_sock)
            .unwrap()
            .file_type()
            .is_socket(),
        "the default instance binds default.sock itself"
    );
    UnixStream::connect(&default_sock).expect("and serves on it");

    let (status, log) = daemon.stop();
    assert!(status.success(), "{status}:\n{log}");
    assert!(
        std::fs::symlink_metadata(&default_sock).is_err(),
        "a clean exit removes the default instance's own socket"
    );
}

/// Compare-then-remove: when another daemon has taken `default.sock` by the
/// time the default instance exits, the exit leaves the newer one alone.
#[test]
fn the_default_instance_exit_keeps_a_socket_another_daemon_took() {
    let fx = Fixture::new();
    // No --arena/--control: the CLI's own defaults under HOME.
    let daemon = start(fx.daemon(&[]));
    let default_sock = fx.mache("default.sock");
    UnixStream::connect(&default_sock).expect("the default instance serves");

    // A newer daemon replaces the name with its own socket.
    std::fs::remove_file(&default_sock).unwrap();
    let newer = UnixListener::bind(&default_sock).unwrap();

    let (status, log) = daemon.stop();
    assert!(status.success(), "{status}:\n{log}");
    assert!(
        std::fs::symlink_metadata(&default_sock).is_ok(),
        "the exiting daemon must not remove a socket it no longer owns"
    );
    UnixStream::connect(&default_sock).expect("the newer daemon still answers");
    drop(newer);
}

/// The well-known-name rule is scoped to `HOME/.mache`: a daemon whose
/// control block is `default.ctrl` in some other directory is an ordinary
/// explicit instance. It starts, serves at its own `default.sock`, and leaves
/// `HOME/.mache/default.sock` alone.
#[test]
fn a_default_named_control_block_outside_home_mache_is_admitted() {
    let fx = Fixture::new();
    let default_sock = fx.mache("default.sock");
    let elsewhere = PathBuf::from("/nonexistent/another-daemon.sock");
    std::os::unix::fs::symlink(&elsewhere, &default_sock).unwrap();

    let daemon = start(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("default.arena")),
        "--control".as_ref(),
        os(&fx.work("default.ctrl")),
    ]));
    UnixStream::connect(fx.work("default.sock")).expect("it serves at its own socket");
    let (status, log) = daemon.stop();
    assert!(status.success(), "{status}:\n{log}");
    assert_eq!(std::fs::read_link(&default_sock).unwrap(), elsewhere);
}

/// A SIGTERM sent the moment the daemon announces itself (its socket line,
/// before `daemon ready`) reaches the handler: the daemon shuts down cleanly
/// and removes its socket. It used to install the handlers only after
/// announcing ready, so a supervisor signalling on the announcement hit the
/// default action and killed it with nothing cleaned up (seen on Linux CI).
#[test]
fn a_sigterm_on_the_socket_announcement_shuts_down_cleanly() {
    let fx = Fixture::new();
    let daemon = start_until(
        fx.daemon(&[
            "--arena".as_ref(),
            os(&fx.work("s.arena")),
            "--control".as_ref(),
            os(&fx.work("s.ctrl")),
        ]),
        "daemon socket at",
    );
    let (status, log) = daemon.stop();
    assert!(status.success(), "{status}:\n{log}");
    assert!(log.contains("received SIGTERM"), "{log}");
    assert!(
        std::fs::symlink_metadata(fx.work("s.sock")).is_err(),
        "a clean shutdown removes the socket"
    );
}

// --------------------------------------------------------------------------
// ley-line-open-feabf4: control block VERSION mismatch and --reset-arena
// --------------------------------------------------------------------------

/// (1) A v2 control block with no other holder, plus `--reset-arena`, starts
/// cleanly at the current VERSION and says which version it replaced.
#[test]
fn reset_arena_reinitialises_an_unheld_older_control_block() {
    let fx = Fixture::new();
    let ctrl = fx.work("a.ctrl");
    write_control_block(&ctrl, 2, &fx.work("a.arena"));

    let daemon = start(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("a.arena")),
        "--control".as_ref(),
        os(&ctrl),
        "--reset-arena".as_ref(),
    ]));
    let ready_log = daemon.seen.join("\n");
    assert!(
        ready_log.contains("v2") && ready_log.contains("reinitialis"),
        "the reset logs the version it replaced:\n{ready_log}"
    );
    assert_eq!(control_version(&ctrl), 3, "the control block is now v3");
    UnixStream::connect(fx.work("a.sock")).expect("the daemon serves");
    let (status, log) = daemon.stop();
    assert!(status.success(), "{status}:\n{log}");
}

/// (2) A v2 control block another process holds keeps the refusal under
/// `--reset-arena`, and the refusal names that condition. The holder here
/// locks the control block itself, as a publisher does.
#[test]
fn reset_arena_refuses_an_older_control_block_another_process_locks() {
    let fx = Fixture::new();
    let ctrl = fx.work("a.ctrl");
    write_control_block(&ctrl, 2, &fx.work("a.arena"));
    let _holder = hold_flock(&ctrl, libc::LOCK_SH);

    let stderr = refused(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("a.arena")),
        "--control".as_ref(),
        os(&ctrl),
        "--reset-arena".as_ref(),
    ]));
    assert!(
        stderr.contains(HELD_REFUSAL),
        "the refusal names the holder condition:\n{stderr}"
    );
    assert_eq!(
        control_version(&ctrl),
        2,
        "a held control block is not touched"
    );
}

/// (2, lock-aware holder) A process holding the control block's liveness
/// lock (`<ctrl>.lock`, which every running daemon holds shared) keeps the
/// refusal too.
#[test]
fn reset_arena_refuses_an_older_control_block_a_live_holder_leases() {
    let fx = Fixture::new();
    let ctrl = fx.work("a.ctrl");
    write_control_block(&ctrl, 2, &fx.work("a.arena"));
    let _holder = hold_flock(&fx.work("a.ctrl.lock"), libc::LOCK_SH);

    let stderr = refused(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("a.arena")),
        "--control".as_ref(),
        os(&ctrl),
        "--reset-arena".as_ref(),
    ]));
    assert!(stderr.contains(HELD_REFUSAL), "{stderr}");
    assert_eq!(control_version(&ctrl), 2);
}

/// (3) Without `--reset-arena` the mismatch is refused, and the message names
/// the recovery.
#[test]
fn a_version_mismatch_without_reset_is_refused_naming_the_recovery() {
    let fx = Fixture::new();
    let ctrl = fx.work("a.ctrl");
    write_control_block(&ctrl, 2, &fx.work("a.arena"));

    let stderr = refused(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("a.arena")),
        "--control".as_ref(),
        os(&ctrl),
    ]));
    assert!(stderr.contains("VERSION mismatch"), "{stderr}");
    assert!(
        stderr.contains("--reset-arena"),
        "the refusal names the recovery:\n{stderr}"
    );
    assert_eq!(control_version(&ctrl), 2);
}

/// (4) A live v3 daemon's control block is not reset out from under it by
/// another daemon's `--reset-arena`, on a different arena.
#[test]
fn reset_arena_refuses_a_control_block_a_live_daemon_serves() {
    let fx = Fixture::new();
    let ctrl = fx.work("c.ctrl");
    let live = start(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("a.arena")),
        "--control".as_ref(),
        os(&ctrl),
    ]));
    let before = std::fs::read(&ctrl).unwrap();
    // The live daemon holds its lease, exactly `<ctrl>.lock`, shared: a
    // reader (mache's `mount --control`) can hold it too, and no one can take
    // it exclusively.
    let lease = fx.work("c.ctrl.lock");
    let reader = hold_flock(&lease, libc::LOCK_SH);
    let probe = std::fs::File::open(&lease).unwrap();
    // SAFETY: flock on a descriptor this test owns.
    let rc = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_ne!(rc, 0, "a running daemon holds {}", lease.display());
    drop(reader);

    let stderr = refused(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("b.arena")),
        "--control".as_ref(),
        os(&ctrl),
        "--reset-arena".as_ref(),
    ]));
    assert!(stderr.contains(HELD_REFUSAL), "{stderr}");
    assert_eq!(
        std::fs::read(&ctrl).unwrap(),
        before,
        "the live daemon's control block is untouched"
    );
    UnixStream::connect(fx.work("c.sock")).expect("the live daemon still serves");
    let (status, log) = live.stop();
    assert!(status.success(), "{status}:\n{log}");
}

// --------------------------------------------------------------------------
// ley-line-open-1d4003: one daemon per control block
// --------------------------------------------------------------------------

/// A second daemon on the same `--control`, on another `--arena` and without
/// `--reset-arena`, is refused: it would publish into the arena the block
/// names while holding only its own arena's lock, and its socket bind would
/// unlink the first daemon's `<ctrl>.sock`. The first keeps serving on the
/// same socket, and a long-lived reader (mache's mapper) still shares the
/// lease with it.
#[test]
fn a_second_daemon_on_one_control_block_is_refused() {
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;

    let fx = Fixture::new();
    let ctrl = fx.work("c.ctrl");
    let first = start(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("a.arena")),
        "--control".as_ref(),
        os(&ctrl),
    ]));
    let sock = fx.work("c.sock");
    let inode = std::fs::metadata(&sock)
        .expect("the first daemon's socket")
        .ino();
    let block = std::fs::read(&ctrl).unwrap();
    // mache's mapper holds the lease shared beside the running daemon.
    let reader = hold_flock(&fx.work("c.ctrl.lock"), libc::LOCK_SH);

    let stderr = refused(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("b.arena")),
        "--control".as_ref(),
        os(&ctrl),
    ]));
    assert!(
        stderr.contains("another daemon serves the control block"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::metadata(&sock)
            .expect("the socket is still there")
            .ino(),
        inode,
        "the first daemon's socket was not replaced"
    );
    assert_eq!(
        std::fs::read(&ctrl).unwrap(),
        block,
        "the block is untouched"
    );
    let mut stream = UnixStream::connect(&sock).expect("the first daemon still serves");
    stream.write_all(b"{\"op\":\"leyline_version\"}\n").unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    assert!(reply.contains("\"ok\":true"), "{reply}");

    drop(reader);
    let (status, log) = first.stop();
    assert!(status.success(), "{status}:\n{log}");
}

/// Two control paths that differ only in extension derive one socket and
/// one living db (`with_extension`), so the second daemon is refused too
/// (bead `ley-line-open-b0800d`): it would otherwise unlink the first's
/// `c.live.db` on its cold start and replace its `c.sock`.
#[test]
fn a_daemon_on_a_sibling_control_path_is_refused() {
    use std::os::unix::fs::MetadataExt;

    let fx = Fixture::new();
    let first = start(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("a.arena")),
        "--control".as_ref(),
        os(&fx.work("c.ctrl")),
    ]));
    let ino = |name: &str| std::fs::metadata(fx.work(name)).expect(name).ino();
    let (sock, live) = (ino("c.sock"), ino("c.live.db"));

    let stderr = refused(fx.daemon(&[
        "--arena".as_ref(),
        os(&fx.work("b.arena")),
        "--control".as_ref(),
        os(&fx.work("c.other")),
    ]));
    assert!(
        stderr.contains("another daemon serves the control block"),
        "{stderr}"
    );
    assert_eq!(ino("c.sock"), sock, "the first daemon's socket stays");
    assert_eq!(ino("c.live.db"), live, "the first daemon's live db stays");
    UnixStream::connect(fx.work("c.sock")).expect("the first daemon still serves");
    let (status, log) = first.stop();
    assert!(status.success(), "{status}:\n{log}");
}
