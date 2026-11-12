//! `leyline-conn-broker`: one run's credential-free connection broker
//! (ADR-0046 Slice 1, S5; bead `ley-line-open-52b5b3`).
//!
//! Spawned by the supervisor with `--lifeline-fd <fd>` (and, on the microVM
//! tier, `--peer-pid <worker pid>`). It reads the authorized canonical
//! connections document from stdin, binds every endpoint, confines itself,
//! reports ready with the digest of what it bound, relays, reports each
//! connection's record, and exits on drain, on stdin end-of-file, or the
//! moment the lifeline reaches end-of-file. Everything but this entry point is
//! `leyline_runtime::broker::control`; the profile is macOS only, and elsewhere
//! the broker refuses by name before reporting ready (ADR-0046 S6).

use std::io::Write;
use std::process::ExitCode;

use leyline_runtime::broker::control::{Event, line, parse_args, serve, watch_lifeline};
use leyline_runtime::broker::{profile, relay};

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(detail) => {
            let _ = std::io::stdout().write_all(line(&Event::Error { detail }).as_bytes());
            return ExitCode::from(2);
        }
    };
    // The lifeline first: from here on, a vanished supervisor ends us.
    watch_lifeline(args.lifeline_fd);
    match serve(
        std::io::stdin().lock(),
        std::io::stdout(),
        args.peer_pid,
        relay::Limits::default(),
        profile::apply,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}
