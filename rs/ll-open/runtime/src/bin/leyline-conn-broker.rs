//! `leyline-conn-broker`: one run's credential-free connection broker
//! (ADR-0046 Slice 1, S5; bead `ley-line-open-52b5b3`).
//!
//! Spawned by the supervisor with `--lifeline-fd <fd>` and, on the microVM
//! tier, `--await-peer` (the worker's pid follows on the control pipe). It
//! reads the authorized canonical connections document from stdin, binds every
//! endpoint, confines itself with `broker::profile::apply`, reports ready with
//! the digest of what it bound, relays, reports each connection's record, and
//! exits on drain, on stdin end-of-file, or the moment the lifeline reaches
//! end-of-file. The process is `leyline_runtime::broker::control::run_broker`;
//! the profile is macOS only, and elsewhere the broker refuses by name before
//! reporting ready (ADR-0046 S6).

use std::process::ExitCode;

use leyline_runtime::broker::{control, profile};

fn main() -> ExitCode {
    ExitCode::from(control::run_broker(std::env::args().skip(1), profile::apply))
}
