//! A live guest receives exactly the argv and environment its request
//! names (beads `ley-line-open-7410ec`, `ley-line-open-d4b07f`,
//! `ley-line-open-f7f862`).
//!
//! libkrun used to hand both to the guest on the kernel command line, which
//! the kernel and init re-parse: an argument `--` cut the argv there
//! (`tini -- docker-entrypoint.sh` received no program), a double quote was
//! consumed or ended a value, and a space-separated word inside a value
//! could be read as a boot or init token. Both now travel in the exec
//! config file the worker writes into the run's root and the guest init
//! reads (LLO libkrun patch 0013).
//!
//! The guest prints each argument and each named variable as hex, one line
//! each, so the comparison is byte for byte and survives any byte the
//! console would otherwise mangle. Synchronization is the run's completion;
//! the durations only bound a failure.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::{Backend, BackendRunStatus, DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
#[path = "../../runtime/tests/conformance/mod.rs"]
#[allow(dead_code)]
mod conformance;
mod hypervisor;

/// Prints `argv <hex>` for each argument after argv[0], `env <name> <hex>`
/// (or `env <name> -`) for each variable named in its own first argument's
/// list, `dummy0 up|down|absent` for whether init's TSI setup brought up
/// the dummy interface, `cmdline <hex>` for each kernel command-line token,
/// `uid <n>` for its own real uid, and `config yes|no` for whether the exec
/// config is still in its root.
const ARGV_PROBE: &str = r#"use std::os::unix::ffi::OsStrExt;
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn main() {
    let mut args = std::env::args_os().skip(1);
    let names = args.next().expect("the variable names");
    for arg in args {
        println!("argv {}", hex(arg.as_bytes()));
    }
    for name in names.as_bytes().split(|b| *b == b',') {
        let name = std::ffi::OsStr::from_bytes(name);
        match std::env::var_os(name) {
            Some(value) => println!("env {} {}", name.to_string_lossy(), hex(value.as_bytes())),
            None => println!("env {} -", name.to_string_lossy()),
        }
    }
    // The kernel's dummy driver creates dummy0 by itself; what init's TSI
    // setup does is bring it up (IFF_UP, bit 0 of its flags).
    let dummy = match std::fs::read_to_string("/sys/class/net/dummy0/flags") {
        Ok(flags) => {
            let flags = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16)
                .expect("dummy0's flags");
            if flags & 1 == 1 { "up" } else { "down" }
        }
        Err(_) => "absent",
    };
    println!("dummy0 {dummy}");
    let cmdline = std::fs::read_to_string("/proc/cmdline").expect("the kernel command line");
    for token in cmdline.split_whitespace() {
        println!("cmdline {}", hex(token.as_bytes()));
    }
    let status = std::fs::read_to_string("/proc/self/status").expect("its status");
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().next())
        .expect("its uid");
    println!("uid {uid}");
    let config = std::path::Path::new("/.leyline-exec.json").exists();
    println!("config {}", if config { "yes" } else { "no" });
}
"#;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every byte 0x01-0x7F, multi-byte UTF-8, and the shapes that broke on the
/// kernel command line.
fn hostile_arguments() -> Vec<String> {
    let every_ascii: String = (1u8..=0x7f).map(char::from).collect();
    vec![
        "a".into(),
        "--".into(),
        "b".into(),
        "--x".into(),
        "\"".into(),
        "\\".into(),
        "'".into(),
        "line1\nline2".into(),
        "a, b".into(),
        "x=\"ab\"; exit 93".into(),
        "caf\u{e9} \u{1F600} \u{4e2d}".into(),
        "x tsi_hijack y".into(),
        "".into(),
        every_ascii,
        // The keys libkrun's general config reader looks for, as data. That
        // reader matches them, without regard to case, against every token
        // of the file, so these set init's uid and gid and prepended to the
        // argv; the exec config reader takes only the object's own keys.
        "uid".into(),
        "1000".into(),
        "GID".into(),
        "65534".into(),
        "Entrypoint".into(),
        "/bin/false".into(),
        "WorkingDir".into(),
        "/etc".into(),
        "Cmd".into(),
        "Env".into(),
    ]
}

fn hostile_environment() -> BTreeMap<String, String> {
    // Every byte, `=` included: a value may hold it; only the key may not.
    let mut every_ascii: String = (1u8..=0x7f).map(char::from).collect();
    every_ascii.push_str(" caf\u{e9} \u{1F600} \u{4e2d}");
    BTreeMap::from([
        ("A".to_owned(), "x tsi_hijack y".to_owned()),
        ("QUOTED".to_owned(), "v\"q\" -- \\n".to_owned()),
        ("MULTI".to_owned(), "l1\nl2\tl3".to_owned()),
        ("lower.case".to_owned(), "caf\u{e9}".to_owned()),
        ("EVERY".to_owned(), every_ascii),
    ])
}

#[test]
fn the_guest_receives_its_argv_and_environment_byte_for_byte() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical fixture");

    let source = root.join("argv-probe.rs");
    fs::write(&source, ARGV_PROBE).expect("guest source");
    let staged = root.join("argv-probe");
    hypervisor::compile_guest(&source, &staged);
    let bytes = fs::read(&staged).expect("guest bytes");
    let rootfs = conformance::publish_rootfs(&root.join("cas"), &[("bin/guest", &bytes, 0o755)]);
    let worker = hypervisor::worker_under_test(
        Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
        &root.join("argv-worker"),
    );
    let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
    // SAFETY: the tests here are serialized and nothing else reads the
    // environment concurrently.
    unsafe { std::env::set_var(library_var, library_dir) };
    let ephemeral_root = root.join("argv-runs");
    fs::create_dir_all(&ephemeral_root).expect("ephemeral root");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root: root.join("cas"),
        ephemeral_root,
        runtime_files: vec![libkrunfw.clone()],
        devices: Vec::new(),
        ready_timeout: Duration::from_secs(30),
        tsi_hijack_inet: false,
    });

    let environment = hostile_environment();
    // The two variables that point init at the exec config are init's to
    // read, never the workload's.
    let mut names: Vec<&str> = environment.keys().map(String::as_str).collect();
    names.extend(["KRUN_CONFIG", "KRUN_CONFIG_ARGV"]);
    let mut arguments = vec![names.join(",")];
    arguments.extend(hostile_arguments());
    let request = ExecutionRequest {
        run_id: "argv-probe".into(),
        replay_key: "argv-probe-replay".into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: rootfs,
        },
        executable: "bin/guest".into(),
        arguments,
        public_environment: environment.clone(),
        allowed_egress: Vec::new(),
        confinement_digest: String::new(),
        confinement_manifest: None,
        workspaces: Vec::new(),
        limits: ResourceLimits {
            wall_time_ms: 120_000,
            memory: leyline_runtime::MemoryLimits { limit: 256 << 20 },
            cpu: leyline_runtime::CpuLimits {
                quota: 100_000,
                period: 100_000,
            },
            ..ResourceLimits::default()
        },
    };
    backend
        .start(&request, leyline_runtime::ServedWorkspaces::none())
        .expect("the guest starts");
    let outcome = backend
        .wait_for_outcome("argv-probe")
        .expect("the guest completes");
    let output = outcome.output.expect("the run's output");
    assert!(
        matches!(outcome.status, BackendRunStatus::Succeeded),
        "the probe exits 0: {:?}\n{}",
        outcome.status,
        output.stdout_tail
    );

    let lines: Vec<&str> = output.stdout_tail.lines().collect();
    let received: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("argv "))
        .collect();
    let expected: Vec<String> = hostile_arguments()
        .iter()
        .map(|a| hex(a.as_bytes()))
        .collect();
    assert_eq!(
        received, expected,
        "every argument arrives exactly, in order, `--` included:\n{}",
        output.stdout_tail
    );
    for (name, value) in &environment {
        let want = format!("env {name} {}", hex(value.as_bytes()));
        assert!(
            lines.contains(&want.as_str()),
            "{name} arrives exactly ({want}):\n{}",
            output.stdout_tail
        );
    }
    // f7f862: a `tsi_hijack` word inside a value is the workload's data,
    // not an init token. Hijacking is off for this run, so init brings up
    // no dummy interface.
    for unset in ["env KRUN_CONFIG -", "env KRUN_CONFIG_ARGV -"] {
        assert!(
            lines.contains(&unset),
            "init unsets its own pointer to the exec config ({unset}):\n{}",
            output.stdout_tail
        );
    }
    assert!(
        lines.contains(&"uid 0"),
        "an argument naming uid or gid stays data:\n{}",
        output.stdout_tail
    );
    assert!(
        lines.contains(&"config no"),
        "init removes the exec config before the workload runs:\n{}",
        output.stdout_tail
    );
    // f7f862: the request's words never reach the kernel command line, so a
    // `tsi_hijack` inside a value cannot turn on init's TSI setup. Hijacking
    // is off for this run: no such token, and init leaves dummy0 down.
    let cmdline: Vec<String> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("cmdline "))
        .map(|token| {
            let bytes: Vec<u8> = (0..token.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&token[i..i + 2], 16).expect("hex"))
                .collect();
            // libkrun writes each variable quoted whole: "KEY=value".
            String::from_utf8_lossy(&bytes).trim_matches('"').to_owned()
        })
        .collect();
    assert!(
        cmdline.iter().any(|token| token == "KRUN_CONFIG_ARGV=1"),
        "the probe read the kernel command line: {cmdline:?}"
    );
    for token in &cmdline {
        assert!(
            !token.contains("tsi_hijack"),
            "no TSI token on the command line: {cmdline:?}"
        );
        for name in environment.keys() {
            assert!(
                !token.starts_with(&format!("{name}=")),
                "the request's environment stays off the command line: {cmdline:?}"
            );
        }
    }
    assert!(
        !lines.contains(&"dummy0 up"),
        "a value cannot turn on init's TSI setup:\n{}",
        output.stdout_tail
    );
}
