//! `leyline daemon --lifeline-fd`: a daemon whose launcher is killed with
//! `SIGKILL` shuts down cleanly (bead `ley-line-open-f6b525`).
//!
//! mache runs its daemon in a process group of its own, so neither a
//! `SIGKILL` of `mache serve` nor a terminal's Ctrl-C reaches it. With the
//! flag, the launcher keeps the only write end of a pipe and hands the daemon
//! the read end; the launcher's death closes the write end, and the daemon
//! runs the same graceful shutdown SIGTERM runs.
//!
//! The launcher here is this test binary run again as [`launcher`]: it
//! starts the real `leyline` binary as its own child, as mache does, and
//! waits to be killed. The daemon's standard error is a named pipe this test
//! reads, so the daemon outlives its launcher without writing into a closed
//! pipe, and end-of-file on it is the daemon closing its last descriptor.
//! Every wait ends on an event (a line, an exit); [`GUARD`] only bounds a
//! failure.

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

    // SAFETY: a signal to the child this test spawned.
    assert_eq!(unsafe { libc::kill(launcher_pid, libc::SIGKILL) }, 0);
    launcher.wait().expect("reap the launcher");

    // EVENT 2: the daemon's stderr ends, and the daemon exits.
    while let Ok(line) = log.recv_timeout(GUARD) {
        seen.push(line);
    }
    if let Err(error) = common::await_exit(daemon_pid, GUARD) {
        // SAFETY: a signal to the leftover daemon, so a failure leaves none.
        unsafe { libc::kill(daemon_pid, libc::SIGKILL) };
        panic!("the daemon outlived its launcher: {error}");
    }
    let log = seen.join("\n");
    assert!(
        log.contains("the launcher's lifeline ended, shutting down"),
        "{log}"
    );
    // The graceful path: the final publish, then the socket removed.
    assert!(log.contains("final snapshot saved to arena"), "{log}");
    assert!(!log.contains("panicked"), "{log}");
    assert!(!socket.exists(), "the daemon left its socket behind");
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
