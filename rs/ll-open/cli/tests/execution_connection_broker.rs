//! `leyline execution-daemon --connection-broker PATH` (ADR-0046 Slice 1,
//! bead `ley-line-open-5308b7`): the daemon refuses to start when the path is
//! not an existing executable file, because with it the daemon would
//! advertise `cloister/execution/v1/connections` and serve nothing.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Run the daemon with `broker` named; it must exit, refused, on its own.
fn refused(fixture: &Path, broker: &Path) -> String {
    let worker = fixture.join("worker");
    std::fs::write(&worker, "#!/bin/sh\n").expect("worker");
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let output = Command::new(env!("CARGO_BIN_EXE_leyline"))
        .arg("execution-daemon")
        .arg("--arena")
        .arg(fixture.join("arena"))
        .arg("--control")
        .arg(fixture.join("control"))
        .arg("--cas-root")
        .arg(fixture.join("cas"))
        .arg("--run-root")
        .arg(fixture.join("runs"))
        .arg("--worker")
        .arg(&worker)
        .arg("--catalog")
        .arg(fixture.join("catalog.json"))
        .arg("--allow-unverified-evidence")
        .arg("--connection-broker")
        .arg(broker)
        .output()
        .expect("run leyline execution-daemon");
    assert!(
        !output.status.success(),
        "the daemon started with --connection-broker {}",
        broker.display()
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_connection_broker_that_is_not_an_executable_file_refuses_startup() {
    let fixture = tempfile::TempDir::new().expect("fixture");

    let plain = fixture.path().join("plain");
    std::fs::write(&plain, "#!/bin/sh\n").expect("file");
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("mode");
    let stderr = refused(fixture.path(), &plain);
    assert!(
        stderr.contains("--connection-broker is not an executable file")
            && stderr.contains("is not executable by this process (mode 644)"),
        "{stderr}"
    );

    let stderr = refused(fixture.path(), &fixture.path().join("missing"));
    assert!(
        stderr.contains("--connection-broker is not an executable file"),
        "{stderr}"
    );

    let stderr = refused(fixture.path(), fixture.path());
    assert!(stderr.contains("is not a regular file"), "{stderr}");

    // The control: executable, the same daemon gets past this check and is
    // refused by the next one it cannot pass (there is no CAS root).
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let stderr = refused(fixture.path(), &plain);
    assert!(
        !stderr.contains("--connection-broker"),
        "an executable broker is accepted: {stderr}"
    );
    assert!(
        stderr.contains("CAS root is not an existing directory"),
        "{stderr}"
    );
}
