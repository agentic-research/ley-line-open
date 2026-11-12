//! `leyline-conn-broker`: one run's credential-free connection broker
//! (ADR-0046 Slice 1, S5; bead `ley-line-open-52b5b3`).
//!
//! Spawned by the supervisor with `--lifeline-fd <fd>` (and, on the microVM
//! tier, `--peer-pid <worker pid>`). It reads the authorized canonical
//! connections document from stdin, binds every endpoint, confines itself,
//! reports ready with the digest of what it bound, relays, reports each
//! connection's record, and exits on drain, on stdin end-of-file, or the
//! moment the lifeline reaches end-of-file. The protocol is
//! `leyline_runtime::broker::control`.

use std::io::{BufRead, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "macos")]
    {
        macos::run()
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "leyline-conn-broker: the connection broker is delivered on macOS only \
             (ADR-0046 S6); refusing to run"
        );
        let _ = (std::io::stdin().lock().lines(), std::io::stdout().flush());
        ExitCode::from(2)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    use leyline_runtime::broker::control::{Event, Request, line};
    use leyline_runtime::broker::{check_delivered, profile, relay};
    use leyline_schema_spec::connections;

    struct Args {
        lifeline_fd: i32,
        peer_pid: Option<i32>,
    }

    fn args() -> Result<Args, String> {
        let mut lifeline_fd = None;
        let mut peer_pid = None;
        let mut iter = std::env::args().skip(1);
        while let Some(flag) = iter.next() {
            let value = iter.next().ok_or(format!("{flag} needs a value"))?;
            let number: i32 = value.parse().map_err(|_| format!("{flag} {value:?} is not a number"))?;
            match flag.as_str() {
                "--lifeline-fd" => lifeline_fd = Some(number),
                "--peer-pid" => peer_pid = Some(number),
                _ => return Err(format!("unknown argument {flag}")),
            }
        }
        Ok(Args {
            lifeline_fd: lifeline_fd.ok_or("--lifeline-fd is required")?,
            peer_pid,
        })
    }

    fn emit(event: &Event) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(line(event).as_bytes());
        let _ = stdout.flush();
    }

    fn fail(detail: String) -> ExitCode {
        emit(&Event::Error { detail });
        ExitCode::from(1)
    }

    pub fn run() -> ExitCode {
        let args = match args() {
            Ok(args) => args,
            Err(detail) => return fail(detail),
        };
        // The lifeline first: from here on, a vanished supervisor ends us.
        let lifeline = args.lifeline_fd;
        thread::spawn(move || {
            let mut byte = 0u8;
            loop {
                // SAFETY: a one-byte read into a valid buffer.
                let n = unsafe { libc::read(lifeline, (&mut byte as *mut u8).cast(), 1) };
                if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if n <= 0 {
                    std::process::exit(0);
                }
            }
        });

        let stdin = std::io::stdin();
        let mut requests = stdin.lock().lines();
        let document = match requests.next() {
            Some(Ok(text)) => match serde_json::from_str::<Request>(&text) {
                Ok(Request::Start { document }) => document,
                Ok(other) => return fail(format!("expected start, got {other:?}")),
                Err(error) => return fail(format!("unreadable start request: {error}")),
            },
            _ => return fail("control pipe closed before start".into()),
        };
        let parsed = match connections::parse(document.as_bytes()) {
            Ok(parsed) => parsed,
            Err(error) => return fail(error.to_string()),
        };
        if parsed.canonical_bytes() != document.as_bytes() {
            return fail("connections document is not canonical".into());
        }
        if let Err(error) = check_delivered(&parsed) {
            return fail(error.detail);
        }
        // Bind, then confine, then report ready.
        let endpoints = match relay::bind(&parsed) {
            Ok(endpoints) => endpoints,
            Err(error) => return fail(format!("bind endpoint: {error}")),
        };
        if let Err(error) = profile::apply(&parsed) {
            return fail(error.detail);
        }
        // SAFETY: geteuid cannot fail.
        let uid = unsafe { libc::geteuid() };
        let (sender, records) = mpsc::channel();
        let handle = match relay::start(
            endpoints,
            relay::PeerPolicy {
                uid,
                pid: args.peer_pid,
            },
            relay::Limits::default(),
            sender,
        ) {
            Ok(handle) => handle,
            Err(error) => return fail(format!("start relay: {error}")),
        };
        emit(&Event::Ready {
            connections_digest: format!("blake3-256:{}", parsed.digest_hex()),
        });

        thread::spawn(move || {
            for record in records {
                emit(&Event::from(&record));
            }
        });
        // Wait for drain; end-of-file on the control pipe also ends the run.
        loop {
            match requests.next() {
                Some(Ok(text)) => match serde_json::from_str::<Request>(&text) {
                    Ok(Request::Drain) => break,
                    _ => continue,
                },
                _ => return ExitCode::SUCCESS,
            }
        }
        for record in handle.drain() {
            emit(&Event::from(&record));
        }
        emit(&Event::Drained);
        ExitCode::SUCCESS
    }
}
