//! The broker's own OS profile (ADR-0046 S5, "Bind, then confine").
//!
//! The broker binds every endpoint, then confines itself with this profile,
//! then reports ready. Two layers hold the allow-list: the broker's code dials
//! one address per listener, and this profile refuses every other port.
//!
//! nono's macOS base profile always allows `process-exec*`, `process-fork`,
//! `mach-lookup`, the mDNSResponder socket, `sysctl-read`, `ipc-posix-shm*` and
//! `file-read*` on `/`. So the profile is that base plus explicit
//! `platform_rule` denies, which nono emits after its allows so the deny wins.
//! Outbound TCP is allowed to `localhost:<port>` per declared port and nothing
//! else; there is no inbound TCP and no listen. Accepting on the already-bound
//! UNIX listener needs no path right.

use std::collections::BTreeSet;

use leyline_schema_spec::connections::ConnectionsDocument;
use nono::CapabilitySet;

use crate::ExecutionError;

/// `mach-lookup` services the confined broker may still reach. Empty: the
/// relay needs none after it is confined (the spike for bead
/// `ley-line-open-52b5b3` found no lookup on the accept, dial or copy path).
/// `broker_profile` pins this list, so it cannot grow without a test change.
pub const MACH_LOOKUP_ALLOW: &[&str] = &[];

/// The mDNSResponder socket, denied so the broker has no resolver ("no names").
pub const MDNS_RESPONDER: &str = "/private/var/run/mDNSResponder";

/// The Seatbelt denies the broker adds to nono's base profile, in emission order.
pub fn deny_rules() -> Vec<String> {
    let mut rules = vec![
        "(deny process-exec*)".to_owned(),
        "(deny process-fork)".to_owned(),
        "(deny file-read*)".to_owned(),
        "(deny file-write*)".to_owned(),
        format!("(deny network-outbound (remote unix-socket (path-literal \"{MDNS_RESPONDER}\")))"),
        "(deny mach-lookup)".to_owned(),
    ];
    for service in MACH_LOOKUP_ALLOW {
        rules.push(format!("(allow mach-lookup (global-name \"{service}\"))"));
    }
    rules
}

/// The declared upstream ports, once each, ascending.
pub fn ports(document: &ConnectionsDocument) -> Vec<u16> {
    document
        .connections()
        .iter()
        .map(|c| c.port)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The broker's capability set for `document`.
pub fn capabilities(document: &ConnectionsDocument) -> Result<CapabilitySet, ExecutionError> {
    let ports = ports(document);
    let Some((first, further)) = ports.split_first() else {
        return Err(ExecutionError::invalid_grant(
            "a connections document names at least one connection",
        ));
    };
    #[cfg(target_os = "macos")]
    {
        // ProxyOnly carries one destination; each further port is the exact
        // rule ProxyOnly emits for its own (as confinement's connectLocal
        // list does, ley-line-open-e2c33f).
        let mut capabilities = CapabilitySet::new().proxy_only(*first);
        for port in further {
            capabilities = capabilities
                .platform_rule(format!(
                    "(allow network-outbound (remote tcp \"localhost:{port}\"))"
                ))
                .map_err(nono_error)?;
        }
        for rule in deny_rules() {
            capabilities = capabilities.platform_rule(rule).map_err(nono_error)?;
        }
        Ok(capabilities)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (first, further);
        Err(ExecutionError::unsupported(
            "the connection broker's profile is delivered on macOS only (ADR-0046 S6)",
        ))
    }
}

/// Confine the calling process. Irreversible.
pub fn apply(document: &ConnectionsDocument) -> Result<(), ExecutionError> {
    let capabilities = capabilities(document)?;
    nono::Sandbox::apply_auto(&capabilities)
        .map(|_| ())
        .map_err(nono_error)
}

fn nono_error(error: nono::NonoError) -> ExecutionError {
    ExecutionError::unsupported(format!("connection broker profile: {error}"))
}
