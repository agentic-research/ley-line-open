//! The conformance matrix's workload (bead `ley-line-open-f8ebcf`): one
//! program, written once, that both backends run.
//!
//! It depends on `std` alone, so it is delivered two ways from this one file:
//!
//! - **native nono**: compiled into the runtime's `conformance_matrix` test
//!   binary as a module. The binary is copied into the rootfs and re-entered
//!   at its ignored `conformance_guest` test, which calls [`run`]. This is the
//!   delivery `native_confinement.rs` uses, and for the same reason: on Linux
//!   the dynamically linked guest's loader and libraries are exactly the ones
//!   mapped into the test process.
//! - **embedded libkrun**: compiled by `rustc` as a standalone static Linux
//!   program (its `main` below), as every `libkrun_guest_*` test compiles its
//!   guest.
//!
//! What it does is chosen by its public environment, the one input both
//! backends carry into the workload unchanged:
//!
//! - `CONFORMANCE_MODE=exit:<code>` exits with `<code>` and does nothing else.
//! - `CONFORMANCE_MODE=block` dials the host over its granted channel, writes
//!   `blocked pid=<pid> ppid=<ppid>`, and then never returns on its own.
//! - `CONFORMANCE_MODE=probe` dials the host, runs every probe named in
//!   `CONFORMANCE_PROBES`, writes one `probe` line per probe and then `done`,
//!   and exits 0.
//!
//! `CONFORMANCE_DIAL` names the channel: `tcp:<host>:<port>` (the native
//! tier's §3 `connectLocal`) or `vsock:<port>` (the microVM tier's §6
//! `unixSocket.allow`, which the VMM delivers as a vsock port muxed to the
//! host socket). Either way the host's `accept` returning is the event the
//! matrix waits on; nothing here sleeps.
//!
//! A probe is `name=kind:argument`, probes separated by `;`:
//!
//! - `read:<path>` reads a file;
//! - `write:<path>:<hex bytes>` writes a file and reads it back;
//! - `list:<path>` lists a directory, names sorted and joined by `,`;
//! - `connect:<host>:<port>` opens a TCP connection.
//!
//! A report line is `probe <name> ok <detail>` or
//! `probe <name> err <errno> <detail>`, with `errno` `-` when the failure
//! carried none. Details have line breaks replaced, so one probe is one line.

use std::io::Write;

const MODE: &str = "CONFORMANCE_MODE";
const DIAL: &str = "CONFORMANCE_DIAL";
const PROBES: &str = "CONFORMANCE_PROBES";

/// Exit status when the guest could not do what its mode asked, as opposed to
/// a probe being denied (which is a result, reported over the channel).
const GUEST_BROKEN: i32 = 70;

#[allow(dead_code)]
fn main() {
    run()
}

/// Do what `CONFORMANCE_MODE` asks. Never returns.
pub fn run() -> ! {
    let mode = std::env::var(MODE).unwrap_or_default();
    if let Some(code) = mode.strip_prefix("exit:") {
        std::process::exit(code.parse().unwrap_or(GUEST_BROKEN));
    }
    match mode.as_str() {
        "block" => {
            let mut channel = dial();
            let line = format!(
                "blocked pid={} ppid={}\n",
                std::process::id(),
                std::os::unix::process::parent_id()
            );
            if channel.write_all(line.as_bytes()).is_err() || channel.flush().is_err() {
                std::process::exit(GUEST_BROKEN);
            }
            // Blocked until the host ends the run. Nothing here returns.
            loop {
                std::thread::park();
            }
        }
        "probe" => {
            let mut channel = dial();
            let probes = std::env::var(PROBES).unwrap_or_default();
            for probe in probes.split(';').filter(|probe| !probe.is_empty()) {
                let line = format!("{}\n", run_probe(probe));
                if channel.write_all(line.as_bytes()).is_err() {
                    std::process::exit(GUEST_BROKEN);
                }
            }
            if channel.write_all(b"done\n").is_err() || channel.flush().is_err() {
                std::process::exit(GUEST_BROKEN);
            }
            std::process::exit(0);
        }
        other => {
            eprintln!("conformance guest: unknown {MODE} {other:?}");
            std::process::exit(GUEST_BROKEN);
        }
    }
}

fn run_probe(probe: &str) -> String {
    let Some((name, action)) = probe.split_once('=') else {
        return format!("probe {probe} err - malformed probe");
    };
    let result = match action.split_once(':') {
        Some(("read", path)) => std::fs::read_to_string(path),
        Some(("write", rest)) => match rest.rsplit_once(':') {
            Some((path, hex)) => match decode_hex(hex) {
                Some(bytes) => {
                    std::fs::write(path, &bytes).and_then(|()| std::fs::read_to_string(path))
                }
                None => Err(std::io::Error::other("malformed hex")),
            },
            None => Err(std::io::Error::other("write needs <path>:<hex>")),
        },
        Some(("list", path)) => std::fs::read_dir(path).and_then(|entries| {
            let mut names = entries
                .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<std::io::Result<Vec<_>>>()?;
            names.sort();
            Ok(names.join(","))
        }),
        Some(("connect", address)) => {
            std::net::TcpStream::connect(address).map(|_| "connected".to_owned())
        }
        _ => Err(std::io::Error::other(format!("unknown probe {action:?}"))),
    };
    match result {
        Ok(detail) => format!("probe {name} ok {}", one_line(&detail)),
        Err(error) => format!(
            "probe {name} err {} {}",
            error
                .raw_os_error()
                .map_or_else(|| "-".to_owned(), |errno| errno.to_string()),
            one_line(&error.to_string())
        ),
    }
}

fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], "\\n")
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect()
}

/// The host end of the run's granted channel. A guest that cannot reach it
/// exits [`GUEST_BROKEN`], and the host's bounded `accept` reports it.
fn dial() -> Box<dyn Write> {
    let spec = std::env::var(DIAL).unwrap_or_default();
    if let Some(address) = spec.strip_prefix("tcp:") {
        match std::net::TcpStream::connect(address) {
            Ok(stream) => return Box::new(stream),
            Err(error) => {
                eprintln!("conformance guest: dial {address}: {error}");
                std::process::exit(GUEST_BROKEN);
            }
        }
    }
    if let Some(port) = spec.strip_prefix("vsock:") {
        return vsock(port.parse().unwrap_or(0));
    }
    eprintln!("conformance guest: unknown {DIAL} {spec:?}");
    std::process::exit(GUEST_BROKEN);
}

/// `AF_VSOCK` to the host (CID 2) on `port`. Linux only: the microVM guest is
/// always Linux, and the native tier dials TCP.
#[cfg(target_os = "linux")]
fn vsock(port: u32) -> Box<dyn Write> {
    use std::ffi::c_int;
    use std::os::fd::FromRawFd;

    #[repr(C)]
    struct SockaddrVm {
        svm_family: u16,
        svm_reserved1: u16,
        svm_port: u32,
        svm_cid: u32,
        svm_zero: [u8; 4],
    }
    unsafe extern "C" {
        fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
        fn connect(fd: c_int, address: *const SockaddrVm, length: u32) -> c_int;
    }
    const AF_VSOCK: c_int = 40;
    const SOCK_STREAM: c_int = 1;
    const VMADDR_CID_HOST: u32 = 2;

    // SAFETY: plain syscalls on a descriptor this process owns, with a live
    // address of the size passed.
    let fd = unsafe { socket(AF_VSOCK, SOCK_STREAM, 0) };
    if fd < 0 {
        eprintln!(
            "conformance guest: vsock socket: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(GUEST_BROKEN);
    }
    let address = SockaddrVm {
        svm_family: AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: VMADDR_CID_HOST,
        svm_zero: [0; 4],
    };
    let rc = unsafe { connect(fd, &address, std::mem::size_of::<SockaddrVm>() as u32) };
    if rc != 0 {
        eprintln!(
            "conformance guest: vsock connect {port}: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(GUEST_BROKEN);
    }
    // SAFETY: `fd` is a connected socket this process owns and hands over.
    Box::new(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn vsock(_port: u32) -> Box<dyn Write> {
    eprintln!("conformance guest: vsock exists only in a Linux guest");
    std::process::exit(GUEST_BROKEN);
}
