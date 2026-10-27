//! A kernel probe for §3 `connectLocal` as a list: the compiled policy, applied
//! for real, lets the workload dial every declared port, and no other port,
//! and bind nothing.
//!
//! ## Why (bead `ley-line-open-e2c33f`)
//!
//! `confinement_dimension_coverage` checks the `CapabilitySet` the compiler
//! builds. That is a claim about nono's input, not about the kernel. The
//! two-or-more-port path on macOS adds each further port as a Seatbelt
//! platform rule, and whether that rule survives the profile's own
//! `(deny network*)` is a property of Seatbelt's rule ordering — the kind of
//! fact ADR-0037 records as believed until measured. So it is measured here.
//!
//! Applying a sandbox is irreversible for the process, so the parent test
//! re-runs this test binary as a child (selected by `LLO_CONNECT_PROBE`),
//! which confines itself and reports what it could reach.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;

use leyline_runtime::backends::libkrun::confinement::{Tier, capabilities_from_manifest};
use leyline_runtime::confinement::ConfinementManifest;

const PROBE_ENV: &str = "LLO_CONNECT_PROBE";

fn reach(port: u16) -> &'static str {
    match TcpStream::connect(("127.0.0.1", port)) {
        Ok(_) => "ok",
        Err(_) => "denied",
    }
}

/// The confined half. A no-op unless the parent selected it.
#[test]
fn connect_probe_child() {
    let Ok(spec) = std::env::var(PROBE_ENV) else {
        return;
    };
    let ports: Vec<u16> = spec
        .split(',')
        .map(|port| port.parse().expect("probe port"))
        .collect();
    let (granted, ungranted) = ports.split_at(ports.len() - 1);
    let mut manifest = ConfinementManifest::new();
    for port in granted {
        manifest = manifest.with_connect_local(*port).expect("connectLocal");
    }
    let capabilities = capabilities_from_manifest(&manifest, Path::new("/"), Tier::Native)
        .expect("connectLocal compiles on the native tier");
    if let Err(error) = nono::Sandbox::apply_auto(&capabilities) {
        println!("PROBE:apply-refused {error}");
        return;
    }
    let mut report = Vec::new();
    for port in granted {
        report.push(format!("granted:{port}={}", reach(*port)));
    }
    report.push(format!(
        "ungranted:{}={}",
        ungranted[0],
        reach(ungranted[0])
    ));
    let bind = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(_) => "ok",
        Err(_) => "denied",
    };
    report.push(format!("bind={bind}"));
    println!("PROBE:{}", report.join(" "));
}

#[test]
fn every_declared_local_port_is_reachable_and_nothing_else_is() {
    if std::env::var(PROBE_ENV).is_ok() {
        return;
    }
    let listeners: Vec<TcpListener> = (0..3)
        .map(|_| TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener"))
        .collect();
    let ports: Vec<u16> = listeners
        .iter()
        .map(|listener| listener.local_addr().expect("addr").port())
        .collect();
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "connect_probe_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            PROBE_ENV,
            ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
        .output()
        .expect("run the confined child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        // libtest prints `test connect_probe_child ... ` on the same line.
        .find_map(|line| line.split_once("PROBE:").map(|(_, report)| report))
        .unwrap_or_else(|| {
            panic!(
                "the child reported nothing\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });

    // A kernel without per-port network rules (Landlock ABI < 4) must refuse
    // the whole set — never apply part of it.
    if let Some(reason) = line.strip_prefix("apply-refused ") {
        assert!(
            cfg!(target_os = "linux"),
            "only a Landlock kernel without network rules may refuse: {reason}"
        );
        eprintln!("kernel refused per-port network rules, fail-closed: {reason}");
        return;
    }
    let expected = format!(
        "granted:{}=ok granted:{}=ok ungranted:{}=denied bind=denied",
        ports[0], ports[1], ports[2]
    );
    assert_eq!(
        line, expected,
        "a connectLocal list must reach every declared port, no other port, and \
         grant no bind"
    );
}
