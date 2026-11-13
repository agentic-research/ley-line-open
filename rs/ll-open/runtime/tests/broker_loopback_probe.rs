//! The loopback probe (ADR-0046 S11a; bead `ley-line-open-52deb4`): can the
//! broker's OS profile scope egress per loopback address, and is each address
//! there at all? It is the evidence a widening of the delivered set
//! (`broker::DELIVERED_TARGETS`, today `127.0.0.1` only) must cite: an address
//! joins only when its row is `scoped-allowed` with the port control passing on
//! every platform that delivers slice 1.
//!
//! For each candidate address A, on this platform:
//! 1. availability: bind a TCP listener on `A:P`; `EADDRNOTAVAIL` records the
//!    row `unavailable`, never a pass;
//! 2. allowed: a child confined with port P allowed connects to `A:P`;
//!    success is `scoped-allowed`, `EPERM` is `profile-blocks`;
//! 3. port control: a child with only another port Q allowed connects to
//!    `A:P`; it must get `EPERM`, and success (`port-unscoped`) fails the probe;
//! 4. address control: neither platform's filter has an address dimension
//!    (Seatbelt `remote tcp "localhost:N"`, Landlock `ConnectTcp`), so a row
//!    whose allowed step succeeds is `port-scoped-only` by construction: the
//!    broker's code alone holds the address (S2).
//!
//! The test prints the observed table and asserts the pinned one for this
//! platform. The confined half re-runs this binary as a child, the
//! `confinement_local_connect_probe` pattern; `LEYLINE_REQUIRE_SANDBOX=1` turns
//! an unapplicable profile into a failure instead of a named skip.
//!
//! Linux records its Landlock ABI. Below ABI 4 there is no `ConnectTcp`, so the
//! port control is unexpressible and every available row reads `abi-below-4`.
//! `LEYLINE_PROBE_NO_IPV6=1` declares a network namespace without IPv6
//! loopback (the CI container run), where `::1` must read `unavailable`.

use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::process::Command;

const PROBE_ENV: &str = "LLO_LOOPBACK_PROBE";
const CANDIDATES: &[&str] = &["127.0.0.1", "127.0.0.2", "127.255.255.254", "::1"];

fn sandbox_unavailable(reason: &str) -> ! {
    assert!(
        std::env::var_os("LEYLINE_REQUIRE_SANDBOX").is_none(),
        "LEYLINE_REQUIRE_SANDBOX is set, but the loopback probe could not apply the \
         broker profile: {reason}"
    );
    eprintln!(
        "SKIPPED (NOT a pass): the loopback probe could not apply the broker profile on \
         this host ({reason}). Nothing about loopback scoping was measured. Set \
         LEYLINE_REQUIRE_SANDBOX=1 to make this a failure."
    );
    std::process::exit(0)
}

#[cfg(target_os = "macos")]
fn capabilities(port: u16) -> nono::CapabilitySet {
    let document = leyline_schema_spec::connections::ConnectionsDocument::new(vec![
        leyline_schema_spec::connections::Connection {
            name: "probe".into(),
            endpoint_unix: "/private/tmp/llo-probe.sock".into(),
            host: "127.0.0.1".parse().unwrap(),
            port,
        },
    ])
    .unwrap();
    leyline_runtime::broker::profile::capabilities(&document).expect("broker profile")
}

#[cfg(target_os = "linux")]
fn capabilities(port: u16) -> nono::CapabilitySet {
    // The Linux tier is refused in slice 1; this is the egress rule a Linux
    // broker profile would use (confinement's connectLocal list compile).
    nono::CapabilitySet::new()
        .block_network()
        .allow_tcp_connect(port)
}

#[cfg(target_os = "linux")]
fn landlock_abi() -> i64 {
    // SAFETY: LANDLOCK_CREATE_RULESET_VERSION with a null attribute pointer.
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32,
        )
    }
}

/// The confined half: allow one port, connect to another address and port,
/// report the result.
#[test]
#[ignore = "the confined child of the_loopback_table_matches_this_platform"]
fn loopback_probe_child() {
    let spec = std::env::var(PROBE_ENV).expect("probe spec");
    let parts: Vec<&str> = spec.split('|').collect();
    let (address, port, allowed): (IpAddr, u16, u16) = (
        parts[0].parse().unwrap(),
        parts[1].parse().unwrap(),
        parts[2].parse().unwrap(),
    );
    if let Err(error) = nono::Sandbox::apply_auto(&capabilities(allowed)) {
        println!("PROBE:apply-refused {error}");
        return;
    }
    let result = match TcpStream::connect(SocketAddr::new(address, port)) {
        Ok(_) => "ok".to_owned(),
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => "eperm".to_owned(),
        Err(error) if error.raw_os_error() == Some(libc::EACCES) => "eperm".to_owned(),
        Err(error) => format!("errno{}", error.raw_os_error().unwrap_or(-1)),
    };
    println!("PROBE:{result}");
}

fn child(address: &str, port: u16, allowed: u16) -> String {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "loopback_probe_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE_ENV, format!("{address}|{port}|{allowed}"))
        .output()
        .expect("run the confined child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find_map(|line| line.split_once("PROBE:").map(|(_, r)| r.to_owned()))
        .unwrap_or_else(|| {
            panic!(
                "the child reported nothing\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    if let Some(reason) = line.strip_prefix("apply-refused ") {
        sandbox_unavailable(reason);
    }
    line
}

/// One row: `<address> <availability> allowed=… port=… address=…`.
fn row(candidate: &str) -> String {
    let address: IpAddr = candidate.parse().unwrap();
    let listener = match TcpListener::bind(SocketAddr::new(address, 0)) {
        Ok(listener) => listener,
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EADDRNOTAVAIL) | Some(libc::EAFNOSUPPORT)
            ) =>
        {
            return format!("{candidate} unavailable");
        }
        Err(error) => panic!("bind {candidate}: {error}"),
    };
    #[cfg(target_os = "linux")]
    if landlock_abi() < 4 {
        return format!("{candidate} available abi-below-4");
    }
    let port = listener.local_addr().unwrap().port();
    let other = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let allowed = match child(candidate, port, port).as_str() {
        "ok" => "scoped-allowed".to_owned(),
        "eperm" => "profile-blocks".to_owned(),
        other => other.to_owned(),
    };
    let port_control = match child(candidate, port, other).as_str() {
        "eperm" => "eperm".to_owned(),
        "ok" => "port-unscoped".to_owned(),
        other => other.to_owned(),
    };
    let address_control = if allowed == "scoped-allowed" {
        "port-scoped-only"
    } else {
        "n/a"
    };
    drop(listener);
    format!("{candidate} available allowed={allowed} port={port_control} address={address_control}")
}

/// The pinned table for this platform (observed, not assumed: changing it is
/// a change to what the delivered set may claim).
fn expected() -> Vec<String> {
    let scoped = |a: &str| {
        format!("{a} available allowed=scoped-allowed port=eperm address=port-scoped-only")
    };
    #[cfg(target_os = "macos")]
    {
        vec![
            scoped("127.0.0.1"),
            // lo0 carries only 127.0.0.1 unless an alias is configured.
            "127.0.0.2 unavailable".to_owned(),
            "127.255.255.254 unavailable".to_owned(),
            // Measured 2026-11-13 (macOS 27.2): Seatbelt's `localhost:N`
            // matches ::1 too, and the port control holds.
            scoped("::1"),
        ]
    }
    #[cfg(target_os = "linux")]
    {
        let no_ipv6 = std::env::var_os("LEYLINE_PROBE_NO_IPV6").is_some();
        vec![
            scoped("127.0.0.1"),
            scoped("127.0.0.2"),
            scoped("127.255.255.254"),
            if no_ipv6 {
                "::1 unavailable".to_owned()
            } else {
                scoped("::1")
            },
        ]
    }
}

#[test]
fn the_loopback_table_matches_this_platform() {
    let observed: Vec<String> = CANDIDATES.iter().map(|c| row(c)).collect();
    println!("loopback probe ({}):", std::env::consts::OS);
    #[cfg(target_os = "linux")]
    println!("  landlock ABI {}", landlock_abi());
    for line in &observed {
        println!("  {line}");
    }
    assert!(
        !observed.iter().any(|row| row.contains("port-unscoped")),
        "a profile that lets a port it never allowed through is a wildcard: {observed:#?}"
    );
    assert_eq!(observed, expected(), "the pinned loopback table");
    // The delivered set may hold only addresses whose row is scoped-allowed
    // with the port control passing.
    for target in leyline_runtime::broker::DELIVERED_TARGETS {
        let row = observed
            .iter()
            .find(|row| row.starts_with(&format!("{target} ")))
            .expect("every delivered address is a candidate");
        assert!(
            row.contains("allowed=scoped-allowed port=eperm"),
            "{target} is delivered but its row is {row}"
        );
    }
}
