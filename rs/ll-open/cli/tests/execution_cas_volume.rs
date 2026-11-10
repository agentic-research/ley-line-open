//! `leyline execution-daemon --cas-volume STATE_DIR`, end to end over the
//! control socket (bead `ley-line-open-ba86d3`): start creates nothing,
//! `llo_execution_provision` makes a case-sensitive CAS root exist and
//! reports it as `casRoot`, and a restart re-attaches the volume.
//!
//! On macOS this creates and attaches a real sparsebundle with
//! `/usr/bin/hdiutil`, so it is opt-in: set `LEYLINE_CAS_VOLUME_TESTS=1`.
//! On Linux no volume is made and it always runs.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use leyline_runtime::cas_volume::{
    CasVolume, DEFAULT_SIZE_CAP_BYTES, is_mount_point, probe_sensitivity,
};
use serde_json::{Value, json};

fn call(sock: &Path, request: Value) -> Value {
    let mut stream = UnixStream::connect(sock).expect("connect daemon socket");
    stream
        .write_all(format!("{request}\n").as_bytes())
        .expect("send request");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("end request");
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("read reply");
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("reply {line:?}: {error}"))
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The daemon's files, and the volume it provisions, detached on drop
/// before the directory (and the sparsebundle in it) is removed.
struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["runs", "home", "state"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("mkdir");
        }
        std::fs::write(dir.path().join("worker"), "#!/bin/sh\nexit 1\n").expect("worker");
        std::fs::write(dir.path().join("catalog.json"), r#"{"entries":[]}"#).expect("catalog");
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn volume(&self) -> CasVolume {
        CasVolume::new(self.path("state"), DEFAULT_SIZE_CAP_BYTES).expect("volume")
    }

    fn daemon(&self) -> Daemon {
        let sock = self.path("daemon.sock");
        let _ = std::fs::remove_file(&sock);
        let log = std::fs::File::create(self.path("daemon.log")).expect("log");
        let child = Command::new(env!("CARGO_BIN_EXE_leyline"))
            .arg("execution-daemon")
            .arg("--backend")
            .arg("native")
            .arg("--arena")
            .arg(self.path("daemon.arena"))
            .arg("--control")
            .arg(self.path("daemon.ctrl"))
            .arg("--cas-volume")
            .arg(self.path("state"))
            .arg("--cas-volume-size-cap-gib")
            .arg("2")
            .arg("--run-root")
            .arg(self.path("runs"))
            .arg("--worker")
            .arg(self.path("worker"))
            .arg("--catalog")
            .arg(self.path("catalog.json"))
            .env("HOME", self.path("home"))
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("log"))
            .stderr(log)
            .spawn()
            .expect("spawn leyline execution-daemon");
        let mut daemon = Daemon { child };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if sock.exists() && UnixStream::connect(&sock).is_ok() {
                return daemon;
            }
            if let Some(status) = daemon.child.try_wait().expect("poll daemon") {
                panic!(
                    "the daemon exited ({status}); log:\n{}",
                    std::fs::read_to_string(self.path("daemon.log")).unwrap_or_default()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the daemon socket never answered"
            );
            std::thread::yield_now();
        }
    }

    fn provision(&self) -> Value {
        let reply = call(
            &self.path("daemon.sock"),
            json!({
                "op": "llo_execution_provision",
                "backendClass": "native",
                "idempotencyKey": "provision-cas-volume",
            }),
        );
        assert_ne!(reply["ok"], false, "provision: {reply}");
        reply
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if cfg!(target_os = "macos") {
            let volume = self.volume();
            // Each attempt is an hdiutil run; retry them until a deadline.
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if volume.detach().is_ok() {
                    return;
                }
            }
            eprintln!(
                "the test volume beneath {} is still attached: detach it with hdiutil",
                self.path("state").display()
            );
        }
    }
}

/// The `casRoot` a provision reply carries, wherever the transport puts the
/// operation's output.
fn cas_root_of(reply: &Value) -> PathBuf {
    let cas_root = reply
        .get("casRoot")
        .or_else(|| reply.get("result").and_then(|result| result.get("casRoot")))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no casRoot in {reply}"));
    PathBuf::from(cas_root)
}

#[test]
fn provision_over_the_socket_reports_a_case_sensitive_cas_root_and_restart_reattaches_it() {
    if cfg!(target_os = "macos")
        && std::env::var_os("LEYLINE_CAS_VOLUME_TESTS").is_none_or(|value| value != "1")
    {
        eprintln!(
            "SKIPPED (NOT a pass): LEYLINE_CAS_VOLUME_TESTS=1 is unset; this test creates and \
             attaches a real sparsebundle with /usr/bin/hdiutil"
        );
        return;
    }
    let host = Host::new();
    let expected = host.volume().cas_root();

    let daemon = host.daemon();
    assert!(
        !host.path("state/cas-volume").exists() && !host.path("state/cas").exists(),
        "start creates no volume and no CAS root"
    );
    let reply = host.provision();
    let cas_root = cas_root_of(&reply);
    assert_eq!(cas_root, expected, "{reply}");
    assert!(cas_root.is_dir());
    assert!(probe_sensitivity(&cas_root).unwrap().case_sensitive);
    assert_eq!(cas_root_of(&host.provision()), expected, "idempotent");
    drop(daemon);

    if cfg!(target_os = "macos") {
        // As after a reboot: the volume is detached when the daemon starts.
        let mountpoint = host.path("state/cas-volume/mnt");
        assert!(host.volume().detach().expect("detach"));
        assert!(!is_mount_point(&mountpoint).unwrap());
        let _daemon = host.daemon();
        assert!(
            is_mount_point(&mountpoint).unwrap(),
            "daemon start re-attached the volume; log:\n{}",
            std::fs::read_to_string(host.path("daemon.log")).unwrap_or_default()
        );
        assert!(expected.is_dir());
    }
}
