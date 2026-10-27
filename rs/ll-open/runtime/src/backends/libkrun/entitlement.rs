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
//!
//! The platform is a parameter, not a `cfg`, so the macOS rule is compiled and
//! tested on every platform (the mutation gate runs on Linux).

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// The entitlement Hypervisor.framework requires of a VMM process.
pub const HYPERVISOR_ENTITLEMENT: &str = "com.apple.security.hypervisor";

/// The signature inspector. An absolute path: a `PATH` lookup would let the
/// environment choose what vouches for the worker.
const CODESIGN: &str = "/usr/bin/codesign";

/// `Ok` when a worker on platform `os` (a [`std::env::consts::OS`] value) may
/// enter the hypervisor: always on a platform with no entitlement rule; on
/// macOS, when `worker`'s signature grants [`HYPERVISOR_ENTITLEMENT`].
/// Otherwise why not, naming the entitlement.
pub fn hypervisor_entitlement(os: &str, worker: &Path) -> Result<(), String> {
    if os != "macos" {
        return Ok(());
    }
    let inspection = Command::new(CODESIGN)
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(worker)
        .stdin(Stdio::null())
        .output();
    verdict(worker, inspection)
}

/// The verdict on one `codesign -d --entitlements - --xml` inspection.
fn verdict(worker: &Path, inspection: std::io::Result<Output>) -> Result<(), String> {
    let output = inspection.map_err(|error| {
        format!(
            "cannot inspect worker {} for the {HYPERVISOR_ENTITLEMENT} entitlement: \
             {CODESIGN}: {error}",
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
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;

    const GRANTED: &str = r#"<?xml version="1.0"?><plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>"#;

    fn inspected(exit_code: i32, stdout: &str, stderr: &str) -> std::io::Result<Output> {
        Ok(Output {
            // A wait status: the exit code lives in the second byte.
            status: ExitStatus::from_raw(exit_code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[test]
    fn a_true_value_after_the_key_grants_it() {
        assert!(grants(GRANTED, HYPERVISOR_ENTITLEMENT));
        let spaced = "<dict>\n  <key>com.apple.security.hypervisor</key>\n  <true/>\n</dict>";
        assert!(grants(spaced, HYPERVISOR_ENTITLEMENT));
        // Not only the first occurrence: a later true value grants it.
        let repeated = "<key>com.apple.security.hypervisor</key><false/>\
                        <key>com.apple.security.hypervisor</key><true/>";
        assert!(grants(repeated, HYPERVISOR_ENTITLEMENT));
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
        assert!(!grants(
            "<dict><key>com.apple.security.hypervisor.extra</key><true/></dict>",
            HYPERVISOR_ENTITLEMENT
        ));
    }

    #[test]
    fn the_verdict_follows_the_inspection() {
        let worker = Path::new("/w/leyline-krun-worker");
        assert_eq!(verdict(worker, inspected(0, GRANTED, "")), Ok(()));

        // Signed, but without the entitlement.
        let ungranted = verdict(worker, inspected(0, "<dict></dict>", "")).unwrap_err();
        assert!(ungranted.contains(HYPERVISOR_ENTITLEMENT), "{ungranted}");
        assert!(ungranted.contains("/w/leyline-krun-worker"), "{ungranted}");

        // Not signed at all: codesign fails, and its words are kept. A
        // failed inspection is never read as a grant, whatever it printed.
        let unsigned = verdict(
            worker,
            inspected(1, GRANTED, "code object is not signed at all"),
        )
        .unwrap_err();
        assert!(unsigned.contains(HYPERVISOR_ENTITLEMENT), "{unsigned}");
        assert!(unsigned.contains("not signed at all"), "{unsigned}");

        // No inspector.
        let missing = verdict(
            worker,
            Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        )
        .unwrap_err();
        assert!(missing.contains(HYPERVISOR_ENTITLEMENT), "{missing}");
        assert!(missing.contains(CODESIGN), "{missing}");
    }

    #[test]
    fn only_macos_has_an_entitlement_rule() {
        let nowhere = Path::new("/nonexistent/leyline-krun-worker");
        assert_eq!(hypervisor_entitlement("linux", nowhere), Ok(()));
        // On macOS a worker that does not exist has no signature; elsewhere
        // there is no codesign to ask. Either way it is not admitted.
        let refused = hypervisor_entitlement("macos", nowhere).unwrap_err();
        assert!(refused.contains(HYPERVISOR_ENTITLEMENT), "{refused}");
    }
}
