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
//! re-runs this test binary as a child (the `#[ignore]`d
//! `connect_probe_child`, given its ports in `LLO_CONNECT_PROBE`), which
//! confines itself and reports what it could reach.
//!
//! Fail closed: when the host cannot apply the sandbox (an outer sandbox
//! already confines the test, or the kernel lacks Landlock network rules),
//! the test prints a named `SKIPPED (NOT a pass)`. `LEYLINE_REQUIRE_SANDBOX=1`
//! turns that into a failure, the same contract `native_confinement.rs`
//! gives (bead `ley-line-open-704853`). Every other failure is a failure.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;

use leyline_runtime::backends::libkrun::confinement::{Tier, capabilities_from_manifest};
use leyline_runtime::confinement::ConfinementManifest;

const PROBE_ENV: &str = "LLO_CONNECT_PROBE";

/// The fail-closed gate. Returns normally only to report a named skip.
fn sandbox_unavailable(reason: &str) {
    assert!(
        std::env::var_os("LEYLINE_REQUIRE_SANDBOX").is_none(),
        "LEYLINE_REQUIRE_SANDBOX is set, so this host claims it can apply the native \
         sandbox, but the connectLocal probe could not: {reason}. Refusing to report a \
         pass for a confinement test that confined nothing."
    );
    eprintln!(
        "SKIPPED (NOT a pass): the connectLocal probe could not apply nono confinement \
         on this host ({reason}). Most often the test is already inside an outer sandbox \
         (Seatbelt refuses to nest) or the kernel lacks Landlock network rules. Nothing \
         about connectLocal enforcement was verified by this run. Set \
         LEYLINE_REQUIRE_SANDBOX=1 to make this a failure."
    );
}

fn reach(port: u16) -> &'static str {
    match TcpStream::connect(("127.0.0.1", port)) {
        Ok(_) => "ok",
        Err(_) => "denied",
    }
}

/// The confined half, run only by the parent below. Ignored rather than a
/// silent no-op, so an ordinary run never reports it as a pass.
#[test]
#[ignore = "the confined child of every_declared_local_port_is_reachable_and_nothing_else_is"]
fn connect_probe_child() {
    let spec = std::env::var(PROBE_ENV).unwrap_or_else(|_| {
        panic!(
            "connect_probe_child is the confined half of a probe and needs its \
             ports in {PROBE_ENV}; run every_declared_local_port_is_reachable_and_nothing_else_is"
        )
    });
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
            "--ignored",
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

    // A host that cannot apply the policy refuses the whole set (a kernel
    // without per-port network rules never applies part of it). That is
    // fail-closed, but it verifies nothing, so it is a named skip, never a
    // pass, and a failure where the host claims it can sandbox.
    if let Some(reason) = line.strip_prefix("apply-refused ") {
        sandbox_unavailable(reason);
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
