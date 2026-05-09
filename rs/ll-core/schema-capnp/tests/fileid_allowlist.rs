//! ADR-0014 F8.6.3 falsifiable claim: the `(filename, @0x... fileId)`
//! pairs in `schemas/*.capnp` must match a hardcoded allowlist. Any
//! drift — accidental fileId regen, schema rename, file move, or
//! a new schema added without updating this allowlist — fails CI.
//!
//! Why hardcoded rather than auto-discovered: the allowlist is the
//! contract. Adding a new schema is a deliberate, reviewable act, not
//! a side effect of dropping a file in `schemas/`. The whole point
//! of the gate is to make schema identity an audit-trail item.
//!
//! Per ADR-0014 §2 ("File ID is file identity, not file version"),
//! these IDs are stable for the life of the file. They only change
//! if the file is renamed or moved AND we deliberately want a fresh
//! ID — both of which warrant updating this allowlist.

use std::path::Path;

/// (file_basename, expected_fileId_hex_with_0x_prefix). One row per
/// `schemas/*.capnp`. Update via deliberate edit + ADR review.
const SCHEMA_FILEID_ALLOWLIST: &[(&str, &str)] = &[
    ("ast.capnp", "0x9e1e4e1af2b578d9"),
    ("binding.capnp", "0x9c0c8cd3c5b1329a"),
    ("common.capnp", "0xb0c0debaadc0deb0"),
    ("head.capnp", "0xc7c7ada1403b9f78"),
    ("source.capnp", "0x9bd2953355bd438c"),
];

/// Read each `schemas/*.capnp`, parse the leading `@0x...` literal,
/// confirm it matches the allowlist, and confirm no extra files exist
/// that are not in the allowlist.
#[test]
fn schema_fileids_match_allowlist() {
    let schemas_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas");
    let entries: Vec<_> = std::fs::read_dir(&schemas_dir)
        .expect("read schemas/ dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == "capnp")
                .unwrap_or(false)
        })
        .collect();

    // Every file in the allowlist must exist on disk.
    for (name, _id) in SCHEMA_FILEID_ALLOWLIST {
        let path = schemas_dir.join(name);
        assert!(
            path.exists(),
            "F8.6.3 allowlist drift: schemas/{name} listed in allowlist \
             but missing from disk. If the file was renamed/removed, \
             update SCHEMA_FILEID_ALLOWLIST in tests/fileid_allowlist.rs."
        );
    }

    // Every file on disk must be in the allowlist (no extras).
    for entry in &entries {
        let name = entry
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .map(String::from)
            .unwrap_or_default();
        let in_allowlist = SCHEMA_FILEID_ALLOWLIST.iter().any(|(n, _)| *n == name);
        assert!(
            in_allowlist,
            "F8.6.3 allowlist drift: schemas/{name} exists on disk but \
             is NOT in SCHEMA_FILEID_ALLOWLIST. Adding a schema is a \
             deliberate ADR-reviewable act — update the allowlist + \
             update docs/adr/0014-capnp-as-protocol.md if the new file \
             changes the public surface."
        );
    }

    // Every (file, fileId) pair must match the literal in the file's
    // first non-comment, non-blank line. The capnp grammar requires
    // `@0x...;` as the first declaration; we parse it without a full
    // capnp parser by scanning for the first `@0x...;` token.
    for (name, expected_id) in SCHEMA_FILEID_ALLOWLIST {
        let path = schemas_dir.join(name);
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("read schemas/{name}"));
        let actual = extract_file_id(&body).unwrap_or_else(|| {
            panic!("F8.6.3: no `@0x...;` literal found in schemas/{name}")
        });
        assert_eq!(
            actual, *expected_id,
            "F8.6.3 fileId drift in schemas/{name}: \
             allowlist says {expected_id}, file says {actual}. \
             A capnp fileId change is NEVER an accidental refactor — \
             ADR-0014 §2 says the fileId is the file's identity. \
             If this drift is intentional, update the allowlist + \
             ADR + cross-runtime fixture suite."
        );
    }
}

/// Find the first `@0x[hex]+;` token in a `.capnp` file. Returns
/// `Some("0x...")` on match, `None` if the file has no such literal
/// (which is itself a schema bug — capnp requires a fileId).
fn extract_file_id(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Strip leading `@`, find the matching `0x...` then `;`.
        let after_at = trimmed.strip_prefix('@')?;
        let semi = after_at.find(';')?;
        let id = &after_at[..semi];
        if !id.starts_with("0x") {
            return None;
        }
        if !id[2..].chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        return Some(id.to_string());
    }
    None
}
