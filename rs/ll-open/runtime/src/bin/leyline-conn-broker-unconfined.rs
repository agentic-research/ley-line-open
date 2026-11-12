//! TEST ONLY: the connection broker with no OS profile.
//!
//! Built only with the `test-support` feature, which no shipping configuration
//! enables (tools/feature-ledger.txt; this crate's tests enable it through
//! their dev-dependency on the crate). It exists so the supervisor's side of
//! the protocol (`BrokerProcess`: ready, bound sockets, peer naming, drain,
//! teardown) is exercised as a real child process on every platform, including
//! Linux, where the shipped broker refuses at its profile. Nothing confines
//! it: it must never ship. Bead `ley-line-open-52b5b3`.

use std::process::ExitCode;

use leyline_runtime::broker::control;

fn main() -> ExitCode {
    ExitCode::from(control::run_broker(std::env::args().skip(1), |_| Ok(())))
}
