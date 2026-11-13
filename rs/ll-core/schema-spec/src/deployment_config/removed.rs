//! The removed environment variables (ADR-0049 D3): every variable v0.30.0
//! removed, with the document key that replaces it, and the check each
//! binary runs at process start and an embedder calls at its own start.

use std::ffi::{OsStr, OsString};
use std::fmt;

use super::reader::{Json, read_json};

/// The published list, `leyline-removed-variables.json`, byte for byte as
/// released. Generated from `removed-variables.source.json`; drift-gated.
pub const REMOVED_VARIABLES_JSON: &str =
    include_str!("../../deployment-config/v1/published/leyline-removed-variables.json");

/// Variables that stay readable in release builds as named test seams, not
/// configuration (D3). Each is folded into `Head.parserId`; a process that
/// sees one set logs a warning naming it.
pub const TEST_SEAM_VARIABLES: &[&str] = &["LLO_EXTRACTION_EPOCH", "LLO_INJECTIONS_SCM"];

/// One removed variable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemovedVariable {
    /// The variable's name.
    pub name: String,
    /// The dotted document key that replaces it.
    pub replacement: String,
}

/// The removed variables, sorted by name, from [`REMOVED_VARIABLES_JSON`].
pub fn removed_variables() -> Vec<RemovedVariable> {
    parse_list(REMOVED_VARIABLES_JSON.as_bytes())
        .expect("the embedded removed-variable list is drift-gated and parses")
}

fn parse_list(text: &[u8]) -> Option<Vec<RemovedVariable>> {
    let Json::Object(top) = read_json(text).ok()? else {
        return None;
    };
    let (_, Json::Array(items)) = top.iter().find(|(k, _)| k.raw == b"variables")? else {
        return None;
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Json::Object(members) = item else {
            return None;
        };
        let field = |key: &[u8]| -> Option<String> {
            match members.iter().find(|(k, _)| k.raw == key)? {
                (_, Json::String(s)) => s.clean().map(str::to_owned),
                _ => None,
            }
        };
        out.push(RemovedVariable {
            name: field(b"name")?,
            replacement: field(b"replacement")?,
        });
    }
    Some(out)
}

/// The refusal: every removed variable found set, sorted by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedVariables {
    /// Every offender, each with its replacement key.
    pub offenders: Vec<RemovedVariable>,
}

impl RemovedVariables {
    /// The process exit status of the refusal (`EX_CONFIG`).
    pub const EXIT_CODE: u8 = 78;

    /// The stderr message, `program` naming the binary: one header line, then
    /// one line per offender.
    pub fn message(&self, program: &str) -> String {
        let mut out = format!(
            "{program}: refusing to start: removed environment variables are set (ADR-0049):"
        );
        for v in &self.offenders {
            out.push_str(&format!(
                "\n  {}: use {} in the config document",
                v.name, v.replacement
            ));
        }
        out
    }
}

impl fmt::Display for RemovedVariables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message("leyline"))
    }
}

impl std::error::Error for RemovedVariables {}

/// Refuse when any removed variable is set (present in `vars`, whatever its
/// value), naming every offender with its replacement key. Binaries call it
/// with `std::env::vars_os()` before any socket, lock, arena or control
/// file is touched, and exit [`RemovedVariables::EXIT_CODE`]; an embedder
/// calls it the same way at its own start.
pub fn check_removed_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<(), RemovedVariables> {
    let names: Vec<OsString> = vars.into_iter().map(|(k, _)| k).collect();
    let mut offenders: Vec<RemovedVariable> = removed_variables()
        .into_iter()
        .filter(|v| names.iter().any(|n| n.as_os_str() == OsStr::new(&v.name)))
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    offenders.sort();
    Err(RemovedVariables { offenders })
}

/// The test-seam variables set in `vars`, sorted, for the start warning.
pub fn test_seams_set(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<&'static str> {
    let names: Vec<OsString> = vars.into_iter().map(|(k, _)| k).collect();
    TEST_SEAM_VARIABLES
        .iter()
        .copied()
        .filter(|seam| names.iter().any(|n| n.as_os_str() == OsStr::new(seam)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn the_embedded_list_parses_sorted() {
        let list = removed_variables();
        assert_eq!(list.len(), 19);
        assert!(list.windows(2).all(|w| w[0].name < w[1].name));
        assert!(
            list.iter()
                .any(|v| v.name == "LEYLINE_PROFILE" && v.replacement == "diagnostics.profile")
        );
        for seam in TEST_SEAM_VARIABLES {
            assert!(
                list.iter().all(|v| v.name != *seam),
                "a test seam is not removed"
            );
        }
        assert!(list.iter().all(|v| v.name != "LEYLINE_INSTALL_DIR"));
    }

    #[test]
    fn every_offender_is_named_with_its_key() {
        assert_eq!(
            check_removed_environment(env(&[("PATH", "/bin"), ("HOME", "/h")])),
            Ok(())
        );
        let err = check_removed_environment(env(&[
            ("LLO_TRUSTED_QUERY_HASHES", "x"),
            ("PATH", "/bin"),
            ("LEYLINE_PROFILE", ""),
            ("KEYCHAIN_ACCOUNT", "a"),
        ]))
        .unwrap_err();
        assert_eq!(
            err.offenders
                .iter()
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>(),
            [
                "KEYCHAIN_ACCOUNT",
                "LEYLINE_PROFILE",
                "LLO_TRUSTED_QUERY_HASHES"
            ]
        );
        assert_eq!(
            err.message("leyline"),
            "leyline: refusing to start: removed environment variables are set (ADR-0049):\n  \
             KEYCHAIN_ACCOUNT: use keychainAccount in the config document\n  \
             LEYLINE_PROFILE: use diagnostics.profile in the config document\n  \
             LLO_TRUSTED_QUERY_HASHES: use parser.trustedQueryHashes in the config document"
        );
        assert_eq!(err.to_string(), err.message("leyline"));
        assert_eq!(RemovedVariables::EXIT_CODE, 78);
    }

    #[test]
    fn test_seams_are_reported_not_refused() {
        let vars = env(&[
            ("LLO_INJECTIONS_SCM", "1"),
            ("LLO_EXTRACTION_EPOCH", "9"),
            ("X", ""),
        ]);
        assert_eq!(check_removed_environment(vars.clone()), Ok(()));
        assert_eq!(
            test_seams_set(vars),
            ["LLO_EXTRACTION_EPOCH", "LLO_INJECTIONS_SCM"]
        );
        assert!(test_seams_set(env(&[])).is_empty());
        assert!(
            test_seams_set(env(&[("PATH", "/bin")])).is_empty(),
            "an unrelated variable is no seam"
        );
    }

    #[test]
    fn malformed_lists_do_not_parse() {
        for bad in [
            &b"[]"[..],
            b"{}",
            b"{\"variables\": {}}",
            b"{\"variables\": [1]}",
            b"{\"variables\": [{\"name\": \"A\"}]}",
            b"{\"variables\": [{\"name\": 1, \"replacement\": \"b\"}]}",
            b"nope",
        ] {
            assert_eq!(parse_list(bad), None, "{}", String::from_utf8_lossy(bad));
        }
        assert_eq!(
            parse_list(b"{\"variables\": [{\"name\": \"A\", \"replacement\": \"b\"}]}"),
            Some(vec![RemovedVariable {
                name: "A".into(),
                replacement: "b".into()
            }])
        );
    }
}
