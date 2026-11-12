//! `leyline-conn-broker`: one run's credential-free connection broker
//! (ADR-0046 Slice 1, S5; bead `ley-line-open-52b5b3`).
//!
//! Spawned by the supervisor with `--lifeline-fd <fd>` and, on the microVM
//! tier, `--await-peer` (the worker's pid follows on the control pipe). It
//! reads the authorized canonical connections document from stdin, binds every
//! endpoint, confines itself, reports ready with the digest of what it bound,
//! relays, reports each connection's record, and exits on drain, on stdin
//! end-of-file, or the moment the lifeline reaches end-of-file. Everything but
//! this entry point is `leyline_runtime::broker::control`; the profile is macOS
//! only, and elsewhere the broker refuses by name before reporting ready
//! (ADR-0046 S6).

use std::io::Write;
use std::process::ExitCode;

use leyline_runtime::broker::control::{
    Event, FILE_LIMIT, line, parse_args, raise_fd_limit, serve, watch_lifeline,
};
use leyline_runtime::broker::{profile, relay};

fn refuse(detail: String) -> ExitCode {
    let _ = std::io::stdout().write_all(line(&Event::Error { detail }).as_bytes());
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(detail) => return refuse(detail),
    };
    // The lifeline first: from here on, a vanished supervisor ends us.
    watch_lifeline(args.lifeline_fd);
    // Enough descriptors for the connection cap, or refuse to start rather
    // than refuse connections by EMFILE later.
    let limit = raise_fd_limit(FILE_LIMIT);
    if limit < FILE_LIMIT {
        return refuse(format!(
            "descriptor limit {limit} is below the {FILE_LIMIT} the connection cap needs"
        ));
    }
    match serve(
        std::io::stdin().lock(),
        std::io::stdout(),
        args.peer,
        relay::Limits::default(),
        profile::apply,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}
