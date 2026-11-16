//! `leyline daemon --lifeline-fd`: a daemon whose launcher is killed with
//! `SIGKILL` shuts down cleanly (bead `ley-line-open-f6b525`).
//!
//! mache runs its daemon in a process group of its own, so neither a
//! `SIGKILL` of `mache serve` nor a terminal's Ctrl-C reaches it. With the
//! flag, the launcher keeps the only write end of a pipe and hands the daemon
//! the read end; the launcher's death closes the write end, and the daemon
//! runs the same graceful shutdown SIGTERM runs.
//!
//! The launcher in the first test is this test binary run again as
//! [`launcher`]: it starts the real `leyline` binary as its own child, as
//! mache does, and waits to be killed. In the second, the launcher holds the
//! only reader of the daemon's standard error, as mache does, so the daemon
//! writes into a pipe with no reader once it is killed. Both are judged from
//! state (exit, socket, the published arena), not from log lines: once its
//! lifeline ends the daemon points any piped output at `/dev/null`. Every
//! wait ends on an event (a line, an exit); [`GUARD`] only bounds a failure.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;

const GUARD: Duration = Duration::from_secs(120);

/// Set in the launcher: the fixture's HOME and its working directory.
const LAUNCHER_HOME: &str = "LEYLINE_TEST_LIFELINE_LAUNCHER_HOME";
const LAUNCHER_WORK: &str = "LEYLINE_TEST_LIFELINE_LAUNCHER_WORK";

/// The launcher's announcement of the daemon it started, on its stdout.
const DAEMON_PID: &str = "DAEMON-PID ";

/// `leyline daemon` on an explicit arena under `work`, with HOME at `home`,
/// so the real `~/.mache` is never touched.
fn daemon(home: &Path, work: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_leyline"));
    command
        .arg("daemon")
        .arg("--arena")
        .arg(work.join("lifeline.arena"))
        .env("HOME", home)
        .env("LLO_EMBEDDER", "zero")
        .env_remove("LEYLINE_SOCKET")
        .env_remove("LEYLINE_EXECUTION_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    command
}

/// The launching process, standing in for `mache serve`: starts the daemon
/// with the read end of a lifeline it holds the write end of, announces the
/// daemon's pid, and waits to be killed. Not a test on its own.
#[test]
#[ignore = "the launcher of a_sigkilled_launcher_makes_its_daemon_shut_down_cleanly"]
fn launcher() {
    let (Some(home), Some(work)) = (
        std::env::var_os(LAUNCHER_HOME).map(PathBuf::from),
        std::env::var_os(LAUNCHER_WORK).map(PathBuf::from),
    ) else {
        return;
    };
    // Opening a named pipe for writing waits for the test to open it for
    // reading.
    let stderr = std::fs::OpenOptions::new()
        .write(true)
        .open(work.join("daemon.stderr"))
        .expect("the daemon's stderr pipe");
    let (lifeline, read_end) = leyline_runtime::lifeline::pair().expect("a lifeline");
    let mut command = daemon(&home, &work);
    command
        .arg("--source")
        .arg(work.join("src"))
        .arg("--lifeline-fd")
        .arg(read_end.raw_fd().to_string())
        .stderr(stderr);
    read_end.pass_at(&mut command, read_end.raw_fd());
    // Never waited on here: this process is killed with the daemon running,
    // which is the point, and the orphan is reaped by pid 1.
    #[allow(clippy::zombie_processes)]
    let child = command.spawn().expect("spawn the daemon");
    drop(read_end);
    println!("{DAEMON_PID}{}", child.id());
    std::io::stdout().flush().unwrap();
    // Held until this process is killed.
    let _lifeline = lifeline;
    loop {
        std::thread::park();
    }
}

/// Each line of the daemon's stderr, read from the named pipe at `path`.
fn lines_of(path: PathBuf) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let pipe = std::fs::File::open(path).expect("open the daemon's stderr pipe");
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

#[test]
fn a_sigkilled_launcher_makes_its_daemon_shut_down_cleanly() {
    let _serial = common::serial();
    let home = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".mache")).unwrap();
    let work = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(work.path().join("src")).unwrap();
    std::fs::write(work.path().join("src/a.go"), "package a\n").unwrap();
    let stderr_pipe = work.path().join("daemon.stderr");
    let status = Command::new("/usr/bin/mkfifo")
        .arg(&stderr_pipe)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo");
    let log = lines_of(stderr_pipe);

    let mut launcher = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "launcher",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(LAUNCHER_HOME, home.path())
        .env(LAUNCHER_WORK, work.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the launcher");
    let launcher_pid = launcher.id() as libc::pid_t;
    let announced = {
        let stdout = launcher.stdout.take().expect("launcher stdout");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // libtest prints `test launcher ... ` before it on the line.
                if let Some(at) = line.find(DAEMON_PID) {
                    let pid = line[at + DAEMON_PID.len()..].trim();
                    let _ = tx.send(pid.parse::<libc::pid_t>().expect("a pid"));
                    return;
                }
            }
        });
        rx.recv_timeout(GUARD)
    };
    let Ok(daemon_pid) = announced else {
        // SAFETY: a signal to the child this test spawned.
        unsafe { libc::kill(launcher_pid, libc::SIGKILL) };
        panic!("TIMEOUT: the launcher did not start a daemon within {GUARD:?}");
    };

    // EVENT 1: the daemon is up.
    let mut seen = Vec::new();
    loop {
        match log.recv_timeout(GUARD) {
            Ok(line) => {
                let ready = line.contains("daemon ready");
                seen.push(line);
                if ready {
                    break;
                }
            }
            Err(_) => {
                // SAFETY: signals to processes this test started.
                unsafe {
                    libc::kill(daemon_pid, libc::SIGKILL);
                    libc::kill(launcher_pid, libc::SIGKILL);
                }
                panic!(
                    "TIMEOUT: the daemon never became ready:\n{}",
                    seen.join("\n")
                );
            }
        }
    }
    let socket = work.path().join("lifeline.sock");
    assert!(socket.exists(), "the ready daemon's socket");
    if let Err(reply) = propose_b(&socket) {
        // SAFETY: signals to processes this test started.
        unsafe {
            libc::kill(daemon_pid, libc::SIGKILL);
            libc::kill(launcher_pid, libc::SIGKILL);
        }
        panic!("the write was refused: {reply}");
    }

    // SAFETY: a signal to the child this test spawned.
    assert_eq!(unsafe { libc::kill(launcher_pid, libc::SIGKILL) }, 0);
    launcher.wait().expect("reap the launcher");

    // EVENT 2: the daemon's stderr ends (it points its pipes at /dev/null
    // once the lifeline ends, since a launcher's pipe may have no reader
    // left), and the daemon exits.
    while log.recv_timeout(GUARD).is_ok() {}
    if let Err(error) = common::await_exit(daemon_pid, GUARD) {
        // SAFETY: a signal to the leftover daemon, so a failure leaves none.
        unsafe { libc::kill(daemon_pid, libc::SIGKILL) };
        panic!("the daemon outlived its launcher: {error}");
    }
    // The graceful path, from state: the final publish holds the write, and
    // the socket is removed.
    assert!(!socket.exists(), "the daemon left its socket behind");
    assert!(
        published_tree(work.path()).contains(&"b.go".to_owned()),
        "the arena does not hold the write made before the kill: no final publish"
    );
}

/// Create `b.go` through the daemon at `socket`: `Err` holds the reply of a
/// refused write.
fn propose_b(socket: &Path) -> Result<(), String> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
    stream
        .write_all(
            b"{\"op\":\"propose\",\"writes\":[{\"path\":\"b.go\",\"base\":\"absent\",\
              \"kind\":\"create\",\"content\":\"package b\\n\"}]}\n",
        )
        .unwrap();
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).unwrap();
    if reply.contains("\"ok\":true") {
        Ok(())
    } else {
        Err(reply)
    }
}

/// A lifeline that is not a readable pipe or socket refuses startup by
/// name, before the daemon announces anything; a malformed one is refused
/// by the argument parser.
#[test]
fn an_unusable_or_malformed_lifeline_refuses_startup() {
    let _serial = common::serial();
    let home = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".mache")).unwrap();
    let work = tempfile::TempDir::new().unwrap();
    // 0 is /dev/null here: a character device, which would read end-of-file
    // at once. 1048576 is not open.
    for fd in ["0", "1048576", "-1", "three"] {
        let output = daemon(home.path(), work.path())
            .args(["--lifeline-fd", fd])
            .stderr(Stdio::piped())
            .output()
            .expect("run the daemon");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{fd}: {stderr}");
        assert!(stderr.contains("--lifeline-fd"), "{fd}: {stderr}");
        assert!(!stderr.contains("daemon ready"), "{fd}: {stderr}");
        assert!(
            !work.path().join("lifeline.sock").exists(),
            "{fd}: a refused daemon bound its socket"
        );
    }
}

/// The launcher held the only reader of the daemon's standard error, as mache
/// does (`cmd.Stderr` is a pipe it reads). Once the launcher is killed, a
/// write to that stderr fails with EPIPE, and Rust's `eprintln!` panics on a
/// failed write: the daemon must still run its whole graceful shutdown.
///
/// Asserted from state, not from log lines (the log has no reader): the
/// daemon exits successfully, its socket is gone, and the root its
/// controller publishes is the root of its live database as it stands after
/// a write made just before the kill, so the final publish ran.
///
/// The daemon is this test's child, so its exit status is observable; the
/// launcher's two roles (only writer of the lifeline, only reader of the
/// daemon's stderr) are held by a shell that this test kills.
#[test]
fn a_dead_launchers_closed_stderr_does_not_cost_the_final_publish() {
    let _serial = common::serial();
    let home = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".mache")).unwrap();
    let work = tempfile::TempDir::new().unwrap();
    let source = work.path().join("src");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("a.go"), "package a\n").unwrap();

    let (lifeline_read, lifeline_write) = std::io::pipe().unwrap();
    let (stderr_read, stderr_write) = std::io::pipe().unwrap();
    // The launcher: reads the daemon's stderr and says when it is ready; its
    // own stderr is the lifeline's only write end, which it never writes.
    let mut launcher = Command::new("/bin/sh")
        .arg("-c")
        .arg(
            "while IFS= read -r line; do case \"$line\" in \
             *'daemon ready'*) echo READY;; esac; done",
        )
        .stdin(Stdio::from(stderr_read))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(lifeline_write))
        .spawn()
        .expect("spawn the launcher");
    let launcher_pid = launcher.id() as libc::pid_t;
    let mut daemon = daemon(home.path(), work.path())
        .arg("--source")
        .arg(&source)
        .args(["--lifeline-fd", "0"])
        .stdin(Stdio::from(lifeline_read))
        .stderr(Stdio::from(stderr_write))
        .spawn()
        .expect("spawn the daemon");
    let daemon_pid = daemon.id() as libc::pid_t;
    let kill_both = || {
        // SAFETY: signals to the children this test spawned.
        unsafe {
            libc::kill(daemon_pid, libc::SIGKILL);
            libc::kill(launcher_pid, libc::SIGKILL);
        }
    };

    // EVENT 1: the daemon is up.
    let ready = {
        let stdout = launcher.stdout.take().expect("launcher stdout");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        rx.recv_timeout(GUARD)
    };
    if !matches!(&ready, Ok(line) if line.trim() == "READY") {
        kill_both();
        panic!("TIMEOUT: the daemon never became ready: {ready:?}");
    }
    let socket = work.path().join("lifeline.sock");

    // A write, made just before the launcher dies.
    if let Err(reply) = propose_b(&socket) {
        kill_both();
        panic!("the write was refused: {reply}");
    }

    // SAFETY: a signal to the child this test spawned.
    assert_eq!(unsafe { libc::kill(launcher_pid, libc::SIGKILL) }, 0);
    launcher.wait().expect("reap the launcher");

    // EVENT 2: the daemon exits, and exits well.
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(daemon.wait());
    });
    let status = match rx.recv_timeout(GUARD) {
        Ok(status) => status.expect("wait for the daemon"),
        Err(_) => {
            kill_both();
            panic!("TIMEOUT: the daemon outlived its launcher by {GUARD:?}");
        }
    };
    assert!(status.success(), "the daemon's shutdown failed: {status}");
    assert!(!socket.exists(), "the daemon left its socket behind");

    // The final publish: the image the controller names holds the write.
    assert!(
        published_tree(work.path()).contains(&"b.go".to_owned()),
        "the arena does not hold the write made before the kill: no final publish"
    );
}

/// The `_tree` paths of the image the daemon last published: the arena the
/// controller names, its active buffer, `data_size` bytes of it, checked
/// against the controller's root, and read as a SQLite database.
fn published_tree(work: &Path) -> Vec<String> {
    let controller = leyline_core::Controller::open_or_create(&work.join("lifeline.ctrl"))
        .expect("the daemon's controller");
    let arena = std::fs::read(controller.arena_path()).expect("the published arena");
    let header = arena_header(&arena);
    let offset = header
        .active_buffer_offset(arena.len() as u64)
        .expect("a valid arena header") as usize;
    let image = &arena[offset..offset + header.data_size as usize];
    assert_eq!(
        leyline_cli_lib::cmd_daemon::image_root(image),
        controller.current_root(),
        "the controller's root names the arena's active image"
    );
    let mut db = rusqlite::Connection::open_in_memory().unwrap();
    db.deserialize_read_exact("main", std::io::Cursor::new(image), image.len(), true)
        .expect("the published image is a database");
    let mut statement = db.prepare("SELECT path FROM _tree ORDER BY path").unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The arena header at the start of `arena`, copied out of the bytes.
fn arena_header(arena: &[u8]) -> leyline_core::ArenaHeader {
    let size = std::mem::size_of::<leyline_core::ArenaHeader>();
    assert!(arena.len() >= size, "a truncated arena");
    // SAFETY: ArenaHeader is a plain-old-data repr(C) struct, the length is
    // checked above, and an unaligned read copies it out.
    unsafe { std::ptr::read_unaligned(arena.as_ptr() as *const leyline_core::ArenaHeader) }
}

/// No thread of the daemon can meet EPIPE on its standard error, at any
/// time: here the reader of that stderr dies first and the lifeline's
/// holder later, so a whole window exists in which the daemon is up, its
/// stderr pipe has no reader, and its lifeline has not ended. A query op
/// with `LEYLINE_PROFILE=1` writes a line to stderr from a worker thread;
/// it must still be answered (a write that failed with EPIPE would panic
/// that thread and drop the connection unanswered). Then the lifeline ends,
/// and the same state as the closing test above holds: exit 0, socket gone,
/// the write published.
#[test]
fn no_thread_meets_epipe_when_the_stderr_reader_dies_before_the_lifeline() {
    let _serial = common::serial();
    let home = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".mache")).unwrap();
    let work = tempfile::TempDir::new().unwrap();
    let source = work.path().join("src");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("a.go"), "package a\n").unwrap();

    let (lifeline_read, lifeline_write) = std::io::pipe().unwrap();
    let (stderr_read, stderr_write) = std::io::pipe().unwrap();
    // The lifeline's only holder.
    let mut holder = Command::new("/bin/sh")
        .arg("-c")
        .arg("exec /bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(lifeline_write))
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the lifeline holder");
    // The only reader of the daemon's stderr.
    let mut reader = Command::new("/bin/sh")
        .arg("-c")
        .arg(
            "while IFS= read -r line; do case \"$line\" in \
             *'daemon ready'*) echo READY;; esac; done",
        )
        .stdin(Stdio::from(stderr_read))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the stderr reader");
    let mut daemon = daemon(home.path(), work.path())
        .arg("--source")
        .arg(&source)
        .args(["--lifeline-fd", "0"])
        .env("LEYLINE_PROFILE", "1")
        .stdin(Stdio::from(lifeline_read))
        .stderr(Stdio::from(stderr_write))
        .spawn()
        .expect("spawn the daemon");
    let pids = [
        daemon.id() as libc::pid_t,
        holder.id() as libc::pid_t,
        reader.id() as libc::pid_t,
    ];
    let kill_all = || {
        for pid in pids {
            // SAFETY: signals to the children this test spawned.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    };

    let ready = {
        let stdout = reader.stdout.take().expect("reader stdout");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        rx.recv_timeout(GUARD)
    };
    if !matches!(&ready, Ok(line) if line.trim() == "READY") {
        kill_all();
        panic!("TIMEOUT: the daemon never became ready: {ready:?}");
    }
    let socket = work.path().join("lifeline.sock");

    // The stderr reader dies; the lifeline holds.
    // SAFETY: a signal to the child this test spawned.
    assert_eq!(unsafe { libc::kill(pids[2], libc::SIGKILL) }, 0);
    reader.wait().expect("reap the reader");

    // A worker thread writes to the reader-less stderr, and answers.
    let mut stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
    stream
        .write_all(b"{\"op\":\"query\",\"sql\":\"SELECT 1\"}\n")
        .unwrap();
    let mut reply = String::new();
    let _ = BufReader::new(&stream).read_line(&mut reply);
    drop(stream);
    if reply.trim().is_empty() {
        kill_all();
        panic!(
            "the query that wrote to a reader-less stderr went unanswered: \
             its thread met EPIPE"
        );
    }
    if let Err(reply) = propose_b(&socket) {
        kill_all();
        panic!("the write was refused: {reply}");
    }

    // The lifeline ends.
    drop(holder.stdin.take());
    holder.wait().expect("reap the holder");
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(daemon.wait());
    });
    let status = match rx.recv_timeout(GUARD) {
        Ok(status) => status.expect("wait for the daemon"),
        Err(_) => {
            kill_all();
            panic!("TIMEOUT: the daemon outlived its lifeline by {GUARD:?}");
        }
    };
    assert!(status.success(), "the daemon's shutdown failed: {status}");
    assert!(!socket.exists(), "the daemon left its socket behind");
    assert!(
        published_tree(work.path()).contains(&"b.go".to_owned()),
        "the arena does not hold the write: no final publish"
    );
}
