//! The version string a build reports (bead `ley-line-open-5139a3`).
//!
//! Shared by `build.rs`, which computes it into `LEYLINE_BUILD_VERSION`, and
//! by this crate's tests. It has no dependencies so the build script can
//! `#[path]`-include it.
//!
//! * A release build prints the bare crate version `X.Y.Z`. A build is a
//!   release build only when `LEYLINE_RELEASE_VERSION` is set, which
//!   `.github/workflows/release.yml` does from the pushed tag, and it must
//!   name the crate version (with or without a leading `v`); any other value
//!   fails the build.
//! * Every other build prints `X.Y.Z-dev+<shortsha>`, with `.dirty` appended
//!   when tracked files differ from `HEAD`, or `X.Y.Z-dev+unknown` when git
//!   cannot name the revision. A SemVer build-metadata suffix, so it never
//!   equals a release's bare version.
//!
//! The inputs are the crate version, the environment variable and the git
//! revision; wall-clock time is never one of them, so two builds of one
//! revision report one version.

/// The git revision a non-release build was made from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revision {
    /// `git rev-parse --short=12 HEAD`.
    pub short_sha: String,
    /// Whether tracked files differ from `HEAD`.
    pub dirty: bool,
}

/// The version a build reports. `release` is `LEYLINE_RELEASE_VERSION`
/// (`None` when unset or empty); `revision` is `None` when git is
/// unavailable. Errors when `release` names another version.
pub fn build_version(
    crate_version: &str,
    release: Option<&str>,
    revision: Option<&Revision>,
) -> Result<String, String> {
    if let Some(tag) = release {
        let tag = tag.strip_prefix('v').unwrap_or(tag);
        return if tag == crate_version {
            Ok(crate_version.to_string())
        } else {
            Err(format!(
                "LEYLINE_RELEASE_VERSION names {tag}, but the crate version is \
                 {crate_version}; a release build must be of the tagged version"
            ))
        };
    }
    Ok(match revision {
        Some(r) => format!(
            "{crate_version}-dev+{}{}",
            r.short_sha,
            if r.dirty { ".dirty" } else { "" }
        ),
        None => format!("{crate_version}-dev+unknown"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev(dirty: bool) -> Revision {
        Revision {
            short_sha: "0123456789ab".into(),
            dirty,
        }
    }

    #[test]
    fn only_a_release_build_prints_the_bare_version() {
        assert_eq!(
            build_version("0.24.0", Some("v0.24.0"), Some(&rev(false))).unwrap(),
            "0.24.0"
        );
        assert_eq!(
            build_version("0.24.0", Some("0.24.0"), None).unwrap(),
            "0.24.0"
        );
        assert_eq!(
            build_version("0.24.0", None, Some(&rev(false))).unwrap(),
            "0.24.0-dev+0123456789ab"
        );
        assert_eq!(
            build_version("0.24.0", None, Some(&rev(true))).unwrap(),
            "0.24.0-dev+0123456789ab.dirty"
        );
        assert_eq!(
            build_version("0.24.0", None, None).unwrap(),
            "0.24.0-dev+unknown"
        );
    }

    #[test]
    fn a_release_build_of_another_version_fails() {
        let err = build_version("0.24.0", Some("v0.25.0"), Some(&rev(false))).unwrap_err();
        assert!(err.contains("0.25.0") && err.contains("0.24.0"), "{err}");
    }

    /// This build's own version: the bare crate version only when it was
    /// built as a release, and otherwise the dev form, which differs from
    /// the tag's.
    #[test]
    fn this_build_reports_the_dev_form_unless_built_as_a_release() {
        let built = env!("LEYLINE_BUILD_VERSION");
        let crate_version = env!("CARGO_PKG_VERSION");
        match option_env!("LEYLINE_RELEASE_VERSION").filter(|v| !v.is_empty()) {
            Some(_) => assert_eq!(built, crate_version),
            None => {
                assert_ne!(
                    built, crate_version,
                    "a non-tag build differs from the tag form"
                );
                assert!(
                    built.starts_with(&format!("{crate_version}-dev+")),
                    "{built}"
                );
            }
        }
        assert_eq!(crate::daemon::version::BINARY_VERSION, built);
        assert_eq!(crate::daemon::version::RELEASE_VERSION, crate_version);
    }
}
