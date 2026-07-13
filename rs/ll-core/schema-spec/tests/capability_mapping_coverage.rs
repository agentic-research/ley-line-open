//! `capability_mapping_coverage` — enforce that every spec-version
//! directory (`cloister/<name>/v<n>/`) that ships in the schema-spec
//! tree has a corresponding row in `_capability-mapping.md`'s §4
//! crosswalk table (as either a lane-1 grant → lane-3 interface
//! mapping OR an `n/a (substrate-internal)` row).
//!
//! ## Why (bead `ley-line-open-f47f43`)
//!
//! `_capability-mapping.md` §4 is the ONE place where the substrate
//! cert verifier bridges a verified lane-1 grant to a lane-3
//! capability interface. Every capability that ships in
//! `schema-spec/<name>/v<n>/` MUST appear in that table — otherwise
//! the cert verifier has no way to bridge a grant to the interface
//! and the capability is silently unreachable from cert-scoped
//! authorization.
//!
//! The **empty row policy** (see `_capability-mapping.md` §4) covers
//! capabilities with no lane-1 grant analog: the row is still
//! required, with the lane-1 column reading
//! `n/a (substrate-internal)` or a similar sentinel. This forces
//! spec authors to consciously decide whether a grant is
//! appropriate, rather than silently omitting the mapping.
//!
//! Behavior:
//! - Walks every `<crate_root>/<name>/v<n>/` directory.
//! - Parses `_capability-mapping.md` and extracts every `cloister/<X>/v<n>`
//!   string mention.
//! - Asserts every filesystem spec dir has a corresponding mention.
//!
//! Directions of drift this catches:
//! - New spec dir added without adding a crosswalk row (the whole
//!   point of the gate).
//! - Spec dir renamed but `_capability-mapping.md` still references
//!   the old name.
//!
//! Non-goals:
//! - Does NOT check the lane-1 grant on each row is well-formed
//!   (validated by a future lint per §6 of the mapping doc).
//! - Does NOT check reverse coverage (that every mentioned
//!   `cloister/<X>/v<n>` in the doc corresponds to a real dir) —
//!   the doc may cite planned-but-not-shipped versions in text
//!   without ambiguity.

use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn capability_mapping_coverage() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mapping_path = root.join("_capability-mapping.md");
    let mapping = fs::read_to_string(&mapping_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", mapping_path.display()));

    let filesystem_caps = enumerate_spec_versions(&root);
    assert!(
        !filesystem_caps.is_empty(),
        "capability_mapping_coverage found no spec-version directories — did the tree shape change?"
    );

    let mut missing = Vec::new();
    for cap in &filesystem_caps {
        let cap_string = format!("cloister/{}/{}", cap.name, cap.version);
        if !mapping.contains(&cap_string) {
            missing.push(cap_string.clone());
        }
    }

    assert!(
        missing.is_empty(),
        "capability_mapping_coverage: {} filesystem spec-version dir(s) have no row in {}:\n  - {}\n\
         Add a row to §4 (or §5 if lane-2 workload-implicit) — see the doc's \"empty row policy\" \
         for capabilities with no lane-1 grant analog (`n/a (substrate-internal)`).",
        missing.len(),
        mapping_path.display(),
        missing.join("\n  - "),
    );
}

/// Capability identity: `<name>/<version>` from the filesystem
/// (e.g. `credential-isolation/v1`).
#[derive(Debug)]
struct Capability {
    name: String,
    version: String,
}

/// Enumerate every `<crate_root>/<name>/v<n>/` directory. Excludes
/// dot-prefixed dirs, underscore-prefixed pseudo-dirs (`_traits`,
/// `_capability-mapping`), `src/`, `tests/`, `target/`, and any name
/// that doesn't look like a kebab-case capability name.
fn enumerate_spec_versions(root: &Path) -> Vec<Capability> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.')
            || name.starts_with('_')
            || name == "src"
            || name == "tests"
            || name == "target"
        {
            continue;
        }
        if let Ok(sub) = fs::read_dir(&path) {
            for sub_entry in sub.flatten() {
                let sub_path = sub_entry.path();
                if !sub_path.is_dir() {
                    continue;
                }
                let Some(sub_name) = sub_path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if is_version_dir(sub_name) {
                    out.push(Capability {
                        name: name.to_string(),
                        version: sub_name.to_string(),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    out
}

fn is_version_dir(name: &str) -> bool {
    name.starts_with('v') && name.len() >= 2 && name[1..].chars().all(|c| c.is_ascii_digit())
}
