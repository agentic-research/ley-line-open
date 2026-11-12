//! The credential-free connection broker (ADR-0046 Slice 1, S4–S5; bead
//! `ley-line-open-52b5b3`).
//!
//! One broker process per run whose grant carries a `cloister/connections/v1`
//! document. It listens on each connection's host UNIX endpoint and joins every
//! accepted connection to that connection's one loopback TCP target, copying
//! bytes and nothing else: no parser, no name resolution, no credential.
//!
//! - [`preflight`]: the per-endpoint host checks run before anything binds
//!   (path absent, parent private, no symlink, `sun_path` fits).
//! - [`registry`]: the daemon-wide endpoint registry that keeps one run from
//!   reaching another run's broker.
//! - [`relay`]: bind, accept, peer check, bounded dial hold, copy, records.
//! - [`profile`]: the broker's own OS profile, applied after binding.
//! - [`control`]: the control-pipe protocol, the supervisor's process handle,
//!   and receipt aggregation.
//!
//! The binary is `leyline-conn-broker`. `ExecutionService` integration (spawn,
//! start ordering, teardown) is `ley-line-open-5308b7`; until then the service
//! refuses grants with connections by name.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use leyline_schema_spec::connections::ConnectionsDocument;

use crate::ExecutionError;

pub mod control;
pub mod preflight;
pub mod profile;
pub mod registry;
pub mod relay;

// Platform queries, one file per platform so the mutation gate can mutate each
// where it compiles (tools/mutants_diff.sh excludes the other).
#[cfg(target_os = "macos")]
#[path = "sys_macos.rs"]
mod sys;
#[cfg(not(target_os = "macos"))]
#[path = "sys_other.rs"]
mod sys;

/// The target addresses this runtime delivers (ADR-0046 S2, S-Q3). The
/// connections type represents every loopback literal; delivery is this one
/// constant, widened only when the loopback probe passes for an address on
/// every delivering platform (`ley-line-open-52deb4`), in the same commit as
/// the probe's pinned table.
pub const DELIVERED_TARGETS: &[IpAddr] = &[IpAddr::V4(Ipv4Addr::LOCALHOST)];

/// Simultaneous connections per run (S-Q2). Further connections are closed
/// without a dial and counted as refusals.
pub const MAX_CONNECTIONS: usize = 256;

/// The fixed copy buffer per direction (S5).
pub const BUFFER_BYTES: usize = 64 * 1024;

/// How long an accepted connection is held while its upstream refuses
/// (`ECONNREFUSED`), the same bound as a serve channel's hold (S5).
pub const HOLD: Duration = Duration::from_secs(10);

/// The first and the largest wait between dial attempts during the hold.
pub const BACKOFF_START: Duration = Duration::from_millis(5);
pub const BACKOFF_MAX: Duration = Duration::from_millis(200);

/// Refuse, by connection name, any target this runtime does not deliver. Run
/// at start before anything is spawned, so a document the broker's profile
/// would block is never a declared connection that silently never dials.
pub fn check_delivered(document: &ConnectionsDocument) -> Result<(), ExecutionError> {
    for connection in document.connections() {
        if !DELIVERED_TARGETS.contains(&connection.host) {
            return Err(ExecutionError::unsupported(format!(
                "connection `{}`: target {}: loopback address not delivered by this runtime",
                connection.name,
                connection.target_text()
            )));
        }
    }
    Ok(())
}
