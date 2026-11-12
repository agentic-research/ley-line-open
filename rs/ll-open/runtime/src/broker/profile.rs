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

/// The Seatbelt rule that admits one further outbound port, exactly as nono's
/// ProxyOnly emits it for its own (as confinement's connectLocal list does,
/// `ley-line-open-e2c33f`).
pub fn egress_rule(port: u16) -> String {
    format!("(allow network-outbound (remote tcp \"localhost:{port}\"))")
}

/// Every platform rule the profile adds, in emission order: one egress rule
/// per declared port after the first (which ProxyOnly carries), then the
/// denies. Split out so it is tested on every platform.
pub fn platform_rules(document: &ConnectionsDocument) -> Result<(u16, Vec<String>), ExecutionError> {
    let ports = ports(document);
    let Some((first, further)) = ports.split_first() else {
        return Err(ExecutionError::invalid_grant(
            "a connections document names at least one connection",
        ));
    };
    let mut rules: Vec<String> = further.iter().map(|port| egress_rule(*port)).collect();
    rules.extend(deny_rules());
    Ok((*first, rules))
}

/// The broker's capability set for `document`. macOS only: elsewhere the
/// broker refuses by name (ADR-0046 S6).
pub fn capabilities(document: &ConnectionsDocument) -> Result<CapabilitySet, ExecutionError> {
    let (first, rules) = platform_rules(document)?;
    if !cfg!(target_os = "macos") {
        return Err(ExecutionError::unsupported(
            "the connection broker's profile is delivered on macOS only (ADR-0046 S6)",
        ));
    }
    let mut capabilities = CapabilitySet::new().proxy_only(first);
    for rule in rules {
        capabilities = capabilities.platform_rule(rule).map_err(nono_error)?;
    }
    Ok(capabilities)
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

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_schema_spec::connections::Connection;

    fn document(ports: &[u16]) -> ConnectionsDocument {
        ConnectionsDocument::new(
            ports
                .iter()
                .enumerate()
                .map(|(i, port)| Connection {
                    name: format!("c{i}"),
                    endpoint_unix: format!("/run/c{i}.sock"),
                    host: "127.0.0.1".parse().unwrap(),
                    port: *port,
                })
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn ports_are_unique_and_ascending() {
        assert_eq!(ports(&document(&[30432, 30080, 30432])), vec![30080, 30432]);
    }

    #[test]
    fn rules_admit_each_further_port_then_deny_in_a_fixed_order() {
        let (first, rules) = platform_rules(&document(&[9, 7, 8])).unwrap();
        assert_eq!(first, 7, "ProxyOnly carries the lowest port");
        assert_eq!(
            rules,
            vec![
                "(allow network-outbound (remote tcp \"localhost:8\"))".to_owned(),
                "(allow network-outbound (remote tcp \"localhost:9\"))".to_owned(),
                "(deny process-exec*)".to_owned(),
                "(deny process-fork)".to_owned(),
                "(deny file-read*)".to_owned(),
                "(deny file-write*)".to_owned(),
                "(deny network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))"
                    .to_owned(),
                "(deny mach-lookup)".to_owned(),
            ],
            "the mach-lookup allow-set is pinned empty: growing it is a test change"
        );
        assert!(MACH_LOOKUP_ALLOW.is_empty());
        let (_, single) = platform_rules(&document(&[7])).unwrap();
        assert_eq!(single, deny_rules(), "one port needs no further egress rule");
    }

    #[test]
    fn capabilities_are_built_on_macos_and_refused_elsewhere() {
        let result = capabilities(&document(&[7, 8]));
        if cfg!(target_os = "macos") {
            let caps = result.expect("macOS profile");
            assert_eq!(caps.platform_rules(), platform_rules(&document(&[7, 8])).unwrap().1);
        } else {
            let error = result.expect_err("not delivered");
            assert_eq!(error.code, crate::ErrorCode::UnsupportedBackend);
            assert!(error.detail.contains("macOS only"), "{}", error.detail);
        }
    }
}
