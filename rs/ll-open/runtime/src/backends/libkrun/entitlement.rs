//! Whether a worker binary may enter the hypervisor (bead ley-line-open-831bf1).
//!
//! On macOS, Hypervisor.framework admits only a process whose code signature
//! carries `com.apple.security.hypervisor`. A worker without it passes every
//! file-existence check, accepts a run, reaches `krun_start_enter`, and fails
//! VM creation with EINVAL. The entitlement is a property of the BINARY, so it
//! is checked against the binary, before any run is accepted.
//!
//! The check reads the signature with `/usr/bin/codesign -d --entitlements -
//! --xml`, the same inspection `tools/sign_krun_worker.sh` asserts after
//! signing. Elsewhere there is no entitlement to hold: Linux grants KVM by
//! device access, which the backend's device grants cover.

use std::path::Path;

/// The entitlement Hypervisor.framework requires of a VMM process.
pub const HYPERVISOR_ENTITLEMENT: &str = "com.apple.security.hypervisor";

/// `Ok` when `worker`'s signature grants [`HYPERVISOR_ENTITLEMENT`] (always
/// `Ok` off macOS); otherwise why not, naming the entitlement.
pub fn hypervisor_entitlement(worker: &Path) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        macos_hypervisor_entitlement(worker)
    } else {
        Ok(())
    }
}

fn macos_hypervisor_entitlement(worker: &Path) -> Result<(), String> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(worker)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| {
            format!(
                "cannot inspect {} for the {HYPERVISOR_ENTITLEMENT} entitlement: \
                 /usr/bin/codesign: {error}",
                worker.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "worker {} lacks the {HYPERVISOR_ENTITLEMENT} entitlement Hypervisor.framework \
             requires (codesign: {}); sign it with tools/sign_krun_worker.sh",
            worker.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if grants(
        &String::from_utf8_lossy(&output.stdout),
        HYPERVISOR_ENTITLEMENT,
    ) {
        Ok(())
    } else {
        Err(format!(
            "worker {} lacks the {HYPERVISOR_ENTITLEMENT} entitlement Hypervisor.framework \
             requires; sign it with tools/sign_krun_worker.sh",
            worker.display()
        ))
    }
}

/// Whether a codesign `--xml` entitlements plist sets `key` to `<true/>`.
/// The plist is a flat dict of key/value pairs, so the value is the element
/// that follows the key; whitespace between them is not significant.
fn grants(plist: &str, key: &str) -> bool {
    let needle = format!("<key>{key}</key>");
    plist.match_indices(&needle).any(|(at, _)| {
        plist[at + needle.len()..]
            .trim_start()
            .starts_with("<true/>")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_true_value_after_the_key_grants_it() {
        let plist = r#"<?xml version="1.0"?><plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>"#;
        assert!(grants(plist, HYPERVISOR_ENTITLEMENT));
        let spaced = "<dict>\n  <key>com.apple.security.hypervisor</key>\n  <true/>\n</dict>";
        assert!(grants(spaced, HYPERVISOR_ENTITLEMENT));
    }

    #[test]
    fn a_false_absent_or_other_key_does_not_grant_it() {
        assert!(!grants(
            "<dict><key>com.apple.security.hypervisor</key><false/></dict>",
            HYPERVISOR_ENTITLEMENT
        ));
        assert!(!grants("", HYPERVISOR_ENTITLEMENT));
        assert!(!grants(
            "<dict><key>com.apple.security.network.client</key><true/></dict>",
            HYPERVISOR_ENTITLEMENT
        ));
        // A longer key sharing the prefix is a different entitlement.
        assert!(!grants(
            "<dict><key>com.apple.security.hypervisor.extra</key><true/></dict>",
            HYPERVISOR_ENTITLEMENT
        ));
    }
}
