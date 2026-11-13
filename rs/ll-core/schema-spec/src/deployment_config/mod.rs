//! `deployment-config/v1`: the deployment configuration documents of LLO's
//! daemons (ADR-0049), apiVersion [`API_VERSION`].
//!
//! One typed, versioned document per daemon kind ([`Kind`]): `DaemonConfig`
//! (`leyline daemon`), `ExecutionDaemonConfig` (`leyline execution-daemon`)
//! and `SignHelperConfig` (`leyline-sign-helper`). JSON is the native form;
//! TOML is an adapter onto the same data model ([`Syntax`]). [`resolve`]
//! reads a document, refuses it by field and stable [`Reason`] code, fills
//! every default (some from [`HostFacts`]), and returns the typed
//! configuration with its canonical resolved bytes. `configDigest` is
//! BLAKE3-256 over those bytes ([`Resolved::config_digest`]).
//!
//! `configDigest` names one resolved configuration of one LLO release on one
//! host. It is not an authority: nothing is admitted by comparing it, it is
//! never signed or carried in a grant or a head, and although it is written
//! like a CAS address it is never resolved against a CAS. It covers the
//! document, not the files the document names.
//!
//! Resolution is pure: it reads no environment and no file. The read-only
//! host checks and the secret and document file rules (D4) are in
//! [`files`]; the removed-variable refusal (D3) is in [`removed`].
//!
//! ```
//! use leyline_schema_spec::deployment_config::{resolve, HostFacts, Kind, Os, Syntax};
//!
//! let host = HostFacts {
//!     home: Some("/Users/fixture".into()),
//!     xdg_state_home: None,
//!     data_dir: Some("/Users/fixture/Library/Application Support".into()),
//!     os: Os::MacOs,
//! };
//! let text = br#"{"apiVersion": "config.leyline/v1", "kind": "DaemonConfig"}"#;
//! let resolved = resolve(text, Syntax::Json, &[Kind::Daemon], &host)?;
//! assert!(resolved.config_digest().starts_with("blake3-256:"));
//! # Ok::<(), leyline_schema_spec::deployment_config::ConfigRefusal>(())
//! ```

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::Path;

/// The D4 file rules need `openat`, `fstat` and owner ids: Unix only, as LLO
/// is (macOS and Linux).
#[cfg(unix)]
pub mod files;
mod model;
mod reader;
pub mod removed;
mod validate;

pub use model::*;
pub use removed::{
    REMOVED_VARIABLES_JSON, RemovedVariable, RemovedVariables, TEST_SEAM_VARIABLES,
    check_removed_environment, removed_variables, test_seams_set,
};

/// The apiVersion every v1 document carries.
pub const API_VERSION: &str = "config.leyline/v1";

/// The `DigestRef.algorithm` of a `configDigest`.
pub const DIGEST_ALGORITHM: &str = "blake3-256";

/// The published, self-contained JSON Schema of each kind, byte for byte as
/// released (`deployment-config/v1/published/`).
pub fn published_schema(kind: Kind) -> &'static str {
    match kind {
        Kind::Daemon => {
            include_str!("../../deployment-config/v1/published/leyline-daemon-config.schema.json")
        }
        Kind::ExecutionDaemon => include_str!(
            "../../deployment-config/v1/published/leyline-execution-daemon-config.schema.json"
        ),
        Kind::SignHelper => include_str!(
            "../../deployment-config/v1/published/leyline-sign-helper-config.schema.json"
        ),
    }
}

/// A document kind: one per long-running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `leyline daemon`.
    Daemon,
    /// `leyline execution-daemon`.
    ExecutionDaemon,
    /// `leyline-sign-helper`.
    SignHelper,
}

impl Kind {
    /// Every kind.
    pub const ALL: &'static [Kind] = &[Kind::Daemon, Kind::ExecutionDaemon, Kind::SignHelper];

    /// The `kind` string.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Daemon => "DaemonConfig",
            Kind::ExecutionDaemon => "ExecutionDaemonConfig",
            Kind::SignHelper => "SignHelperConfig",
        }
    }

    /// The short name: `daemon-config`, the stem of the schema files.
    pub fn short_name(self) -> &'static str {
        match self {
            Kind::Daemon => "daemon-config",
            Kind::ExecutionDaemon => "execution-daemon-config",
            Kind::SignHelper => "sign-helper-config",
        }
    }

    /// The kind a `kind` string names.
    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.iter().copied().find(|k| k.name() == name)
    }
}

/// A document's syntax, chosen by file extension and never guessed from the
/// content (ADR-0049 D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Syntax {
    /// `.json`: the native form.
    Json,
    /// `.toml`: an adapter onto the same data model.
    Toml,
}

impl Syntax {
    /// The syntax a file's extension names: `.json` or `.toml`; any other
    /// extension, or none, is `unknown-config-syntax`.
    pub fn for_path(path: &Path) -> Result<Syntax, ConfigRefusal> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => Ok(Syntax::Json),
            Some("toml") => Ok(Syntax::Toml),
            _ => Err(ConfigRefusal::new(
                "",
                Reason::UnknownConfigSyntax,
                format!(
                    "{}: a config document is a .json or .toml file",
                    path.display()
                ),
            )),
        }
    }
}

/// The operating system a resolution assumes. Host defaults (the mount
/// backend, the CAS volume layout) and one cross-field rule
/// (`cgroup-root-linux-only`) depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    /// macOS.
    MacOs,
    /// Linux.
    Linux,
}

impl Os {
    /// `macos` or `linux`.
    pub fn name(self) -> &'static str {
        match self {
            Os::MacOs => "macos",
            Os::Linux => "linux",
        }
    }
}

/// The host conventions resolution reads, injected so resolution is pure
/// (ADR-0049 D3, D13). A value is checked only when a default needs it: an
/// unset, relative, non-UTF-8 or otherwise refused `home` is
/// `bad-host-default` for the field whose default needed it, never replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    /// `HOME`: expands `~/` and roots the arena and state defaults.
    pub home: Option<OsString>,
    /// `XDG_STATE_HOME`: when set, non-empty and absolute, the state
    /// directory is `<it>/leyline`.
    pub xdg_state_home: Option<OsString>,
    /// The platform data directory (`dirs::data_dir`), parent of the MCP
    /// token's default `leyline/daemon.token`.
    pub data_dir: Option<OsString>,
    /// The operating system.
    pub os: Os,
}

impl HostFacts {
    /// This process's host: `HOME`, `XDG_STATE_HOME`, the platform data
    /// directory, and the OS this binary was built for. Reads nothing else.
    /// Refused (`unsupported-os`) on an OS other than macOS and Linux.
    pub fn current() -> Result<HostFacts, ConfigRefusal> {
        let os = if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            return Err(ConfigRefusal::new(
                "",
                Reason::UnsupportedOs,
                format!(
                    "deployment configuration supports macOS and Linux, not {}",
                    std::env::consts::OS
                ),
            ));
        };
        let home = std::env::var_os("HOME");
        let data_dir = match os {
            Os::MacOs => home.as_ref().map(|h| {
                Path::new(h)
                    .join("Library/Application Support")
                    .into_os_string()
            }),
            Os::Linux => std::env::var_os("XDG_DATA_HOME")
                .filter(|d| Path::new(d).is_absolute())
                .map(|d| d.to_owned())
                .or_else(|| {
                    home.as_ref()
                        .map(|h| Path::new(h).join(".local/share").into_os_string())
                }),
        };
        Ok(HostFacts {
            home,
            xdg_state_home: std::env::var_os("XDG_STATE_HOME"),
            data_dir,
            os,
        })
    }
}

/// Why a document, or this host's ability to run it, was refused. The codes
/// are stable within v1 (`deployment-config/v1/README.md`, "Refusals").
///
/// Non-exhaustive: a later v1 release may add a reason (a new field brings
/// new refusals); match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Reason {
    /// Not JSON (or TOML) this reader accepts, or nested deeper than 16.
    Malformed,
    /// One object holds a key twice.
    DuplicateKey,
    /// A string holds a refused character, an escape, or is not UTF-8.
    BadString,
    /// A value has the wrong JSON type.
    WrongType,
    /// A required key is absent.
    MissingKey,
    /// A key this kind does not define.
    UnknownKey,
    /// `apiVersion` is not [`API_VERSION`].
    BadApiVersion,
    /// `kind` names no kind.
    UnknownKind,
    /// `kind` names another process's kind.
    WrongKind,
    /// An integer outside its field's range, or negative.
    OutOfRange,
    /// A number with a fraction or exponent.
    NotAnInteger,
    /// A valid IP address in a spelling other than its canonical one.
    NonCanonicalAddress,
    /// Not an IP literal (or `IP:port`) at all.
    BadAddress,
    /// `null` in a field whose default is not `null`.
    NullNotAllowed,
    /// A union object with no key (`{}`, or an empty TOML table).
    UnionEmpty,
    /// A union object with more than one key.
    UnionAmbiguous,
    /// A string outside its field's closed set.
    NotInEnum,
    /// A path neither absolute nor beginning `~/`.
    PathNotAbsolute,
    /// A path with an empty, `.` or `..` component or a trailing `/`.
    BadPath,
    /// A set list holds one entry twice (after `~/` expansion).
    DuplicateEntry,
    /// A non-reference value in a secret field.
    SecretInlined,
    /// A reserved `secretRef` variant (`keychain`, `vault`).
    SecretRefNotDelivered,
    /// Not a label.
    BadLabel,
    /// Not 64 lowercase hex characters.
    BadQueryHash,
    /// Not a URL prefix.
    BadUrlPrefix,
    /// A non-loopback `mcp.http.bind` without `mcp.http.allowPublic`.
    PublicBindWithoutOptIn,
    /// `mcp.groups` names a group this binary does not have.
    UnknownMcpGroup,
    /// The run root is inside, a parent of, or equal to the CAS root.
    RunRootOverlapsCasRoot,
    /// `backend.native.cgroupRoot` on a host that is not Linux.
    CgroupRootLinuxOnly,
    /// `auth.required` with an empty caller map.
    CallerTokensRequired,
    /// `signAllow.required` with no caller holding a prefix.
    SignAllowRequired,
    /// The sign helper's `bind` is not loopback.
    BindNotLoopback,
    /// A `resolveAllow` prefix that would authorize a signing-key URL.
    ResolveAllowSigningKey,
    /// A TOML float or datetime: no v1 field has either type.
    TomlTypeNotMapped,
    /// A config file whose extension is neither `.json` nor `.toml`.
    UnknownConfigSyntax,
    /// A host default could not be computed from this host's facts.
    BadHostDefault,
    /// This OS is neither macOS nor Linux.
    UnsupportedOs,
    /// The document file cannot be read.
    Unreadable,
    /// A secret file fails the D4 ownership, mode, type or directory rules.
    SecretFilePermissions,
    /// An existing secret file is empty, or its contents are not valid.
    SecretFileInvalid,
    /// The `--config` file fails the D4 rules for a document file.
    ConfigFilePermissions,
    /// The resolved document's digest differs from `--expect-config-digest`.
    ConfigDigestMismatch,
    /// The document needs the `mount` feature and this binary lacks it.
    MountFeatureNotBuilt,
    /// The document needs the `fuse` feature and this binary lacks it.
    FuseFeatureNotBuilt,
    /// A file or directory the document names is missing or of the wrong type,
    /// or does not parse.
    HostCheckFailed,
}

impl Reason {
    /// Every reason, so a test can require each to be distinct and documented.
    pub const ALL: &'static [Reason] = &[
        Reason::Malformed,
        Reason::DuplicateKey,
        Reason::BadString,
        Reason::WrongType,
        Reason::MissingKey,
        Reason::UnknownKey,
        Reason::BadApiVersion,
        Reason::UnknownKind,
        Reason::WrongKind,
        Reason::OutOfRange,
        Reason::NotAnInteger,
        Reason::NonCanonicalAddress,
        Reason::BadAddress,
        Reason::NullNotAllowed,
        Reason::UnionEmpty,
        Reason::UnionAmbiguous,
        Reason::NotInEnum,
        Reason::PathNotAbsolute,
        Reason::BadPath,
        Reason::DuplicateEntry,
        Reason::SecretInlined,
        Reason::SecretRefNotDelivered,
        Reason::BadLabel,
        Reason::BadQueryHash,
        Reason::BadUrlPrefix,
        Reason::PublicBindWithoutOptIn,
        Reason::UnknownMcpGroup,
        Reason::RunRootOverlapsCasRoot,
        Reason::CgroupRootLinuxOnly,
        Reason::CallerTokensRequired,
        Reason::SignAllowRequired,
        Reason::BindNotLoopback,
        Reason::ResolveAllowSigningKey,
        Reason::TomlTypeNotMapped,
        Reason::UnknownConfigSyntax,
        Reason::BadHostDefault,
        Reason::UnsupportedOs,
        Reason::Unreadable,
        Reason::SecretFilePermissions,
        Reason::SecretFileInvalid,
        Reason::ConfigFilePermissions,
        Reason::ConfigDigestMismatch,
        Reason::MountFeatureNotBuilt,
        Reason::FuseFeatureNotBuilt,
        Reason::HostCheckFailed,
    ];

    /// The stable code.
    pub fn code(self) -> &'static str {
        match self {
            Reason::Malformed => "malformed",
            Reason::DuplicateKey => "duplicate-key",
            Reason::BadString => "bad-string",
            Reason::WrongType => "wrong-type",
            Reason::MissingKey => "missing-key",
            Reason::UnknownKey => "unknown-key",
            Reason::BadApiVersion => "bad-api-version",
            Reason::UnknownKind => "unknown-kind",
            Reason::WrongKind => "wrong-kind",
            Reason::OutOfRange => "out-of-range",
            Reason::NotAnInteger => "not-an-integer",
            Reason::NonCanonicalAddress => "non-canonical-address",
            Reason::BadAddress => "bad-address",
            Reason::NullNotAllowed => "null-not-allowed",
            Reason::UnionEmpty => "union-empty",
            Reason::UnionAmbiguous => "union-ambiguous",
            Reason::NotInEnum => "not-in-enum",
            Reason::PathNotAbsolute => "path-not-absolute",
            Reason::BadPath => "bad-path",
            Reason::DuplicateEntry => "duplicate-entry",
            Reason::SecretInlined => "secret-inlined",
            Reason::SecretRefNotDelivered => "secret-ref-not-delivered",
            Reason::BadLabel => "bad-label",
            Reason::BadQueryHash => "bad-query-hash",
            Reason::BadUrlPrefix => "bad-url-prefix",
            Reason::PublicBindWithoutOptIn => "public-bind-without-opt-in",
            Reason::UnknownMcpGroup => "unknown-mcp-group",
            Reason::RunRootOverlapsCasRoot => "run-root-overlaps-cas-root",
            Reason::CgroupRootLinuxOnly => "cgroup-root-linux-only",
            Reason::CallerTokensRequired => "caller-tokens-required",
            Reason::SignAllowRequired => "sign-allow-required",
            Reason::BindNotLoopback => "bind-not-loopback",
            Reason::ResolveAllowSigningKey => "resolve-allow-signing-key",
            Reason::TomlTypeNotMapped => "toml-type-not-mapped",
            Reason::UnknownConfigSyntax => "unknown-config-syntax",
            Reason::BadHostDefault => "bad-host-default",
            Reason::UnsupportedOs => "unsupported-os",
            Reason::Unreadable => "unreadable",
            Reason::SecretFilePermissions => "secret-file-permissions",
            Reason::SecretFileInvalid => "secret-file-invalid",
            Reason::ConfigFilePermissions => "config-file-permissions",
            Reason::ConfigDigestMismatch => "config-digest-mismatch",
            Reason::MountFeatureNotBuilt => "mount-feature-not-built",
            Reason::FuseFeatureNotBuilt => "fuse-feature-not-built",
            Reason::HostCheckFailed => "host-check-failed",
        }
    }

    /// The reason a code names.
    pub fn from_code(code: &str) -> Option<Reason> {
        Reason::ALL.iter().copied().find(|r| r.code() == code)
    }

    /// The process exit status this refusal ends `--check-config` and start
    /// with (ADR-0049 D5): 65 for a refused document, 66 for an unreadable
    /// file, 78 for a valid document this host or binary cannot run.
    pub fn exit_code(self) -> u8 {
        match self {
            Reason::Unreadable => 66,
            Reason::CgroupRootLinuxOnly
            | Reason::BadHostDefault
            | Reason::UnsupportedOs
            | Reason::SecretFilePermissions
            | Reason::SecretFileInvalid
            | Reason::ConfigFilePermissions
            | Reason::ConfigDigestMismatch
            | Reason::MountFeatureNotBuilt
            | Reason::FuseFeatureNotBuilt
            | Reason::HostCheckFailed => 78,
            _ => 65,
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// One refusal: the field (dotted from the document root, empty for the
/// whole document), the stable reason, and a message for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRefusal {
    /// Dotted from the document root; empty for a whole-document refusal.
    pub field: String,
    /// The stable reason.
    pub reason: Reason,
    /// A human-readable message; not part of the contract.
    pub message: String,
}

impl ConfigRefusal {
    /// A refusal of `field` for `reason`.
    pub fn new(field: impl Into<String>, reason: Reason, message: impl Into<String>) -> Self {
        ConfigRefusal {
            field: field.into(),
            reason,
            message: message.into(),
        }
    }

    /// The stderr line `--check-config` writes:
    /// `{"field":"…","message":"…","reason":"…"}`, keys sorted, with the
    /// standard JSON escapes.
    pub fn to_json_line(&self) -> String {
        format!(
            "{{\"field\":{},\"message\":{},\"reason\":{}}}",
            json_escape(&self.field),
            json_escape(&self.message),
            json_escape(self.reason.code())
        )
    }
}

impl fmt::Display for ConfigRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.field.is_empty() {
            write!(f, "{}: {}", self.reason.code(), self.message)
        } else {
            write!(
                f,
                "{}: {}: {}",
                self.field,
                self.reason.code(),
                self.message
            )
        }
    }
}

impl std::error::Error for ConfigRefusal {}

/// `s` as a JSON string with the standard escapes.
pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON value as a resolved document holds it: the canonical writer's
/// input, and the form a flag adapter builds a document in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A non-negative integer (no v1 field is negative or a float).
    Int(u64),
    /// A string.
    Str(String),
    /// A list.
    Array(Vec<Value>),
    /// An object; a `BTreeMap` keeps keys in byte order.
    Object(BTreeMap<String, Value>),
}

impl Value {
    /// An object from `(key, value)` pairs.
    pub fn object<I, K>(members: I) -> Value
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<String>,
    {
        Value::Object(members.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// A string value.
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }

    /// `Null` for `None`, else `f(value)`.
    pub fn opt<T>(value: Option<T>, f: impl FnOnce(T) -> Value) -> Value {
        value.map_or(Value::Null, f)
    }

    /// A list of strings.
    pub fn strs<I, S>(items: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Value::Array(items.into_iter().map(|s| Value::Str(s.into())).collect())
    }

    /// The canonical bytes (ADR-0049 D6): keys in byte order at every level,
    /// two-space indentation, `": "` between key and value, strings written
    /// raw with no escape, `[]` and `{}` for empty containers, no trailing
    /// newline. The caller guarantees every string is clean (a resolved
    /// document's are).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        self.write(&mut out, 0, false);
        out.into_bytes()
    }

    /// JSON text for a document's *input* (a flag adapter's), with the
    /// standard escapes, so a refused character reaches the reader as an
    /// escape and is refused as `bad-string` rather than breaking the syntax.
    pub fn input_json(&self) -> Vec<u8> {
        let mut out = String::new();
        self.write(&mut out, 0, true);
        out.into_bytes()
    }

    fn write(&self, out: &mut String, indent: usize, escape: bool) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Str(s) => write_str(out, s, escape),
            Value::Array(items) if items.is_empty() => out.push_str("[]"),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, indent + 1);
                    item.write(out, indent + 1, escape);
                }
                newline(out, indent);
                out.push(']');
            }
            Value::Object(members) if members.is_empty() => out.push_str("{}"),
            Value::Object(members) => {
                out.push('{');
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, indent + 1);
                    write_str(out, key, escape);
                    out.push_str(": ");
                    value.write(out, indent + 1, escape);
                }
                newline(out, indent);
                out.push('}');
            }
        }
    }
}

fn write_str(out: &mut String, s: &str, escape: bool) {
    if escape {
        out.push_str(&json_escape(s));
    } else {
        out.push('"');
        out.push_str(s);
        out.push('"');
    }
}

fn newline(out: &mut String, indent: usize) {
    out.push('\n');
    for _ in 0..indent {
        out.push_str("  ");
    }
}

/// A resolved document: the typed configuration, and the canonical bytes
/// `configDigest` is taken over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The typed configuration.
    pub document: Document,
    /// The canonical resolved bytes (D6): what `--print-config` writes.
    pub canonical: Vec<u8>,
}

impl Resolved {
    /// The kind resolved.
    pub fn kind(&self) -> Kind {
        self.document.kind()
    }

    /// BLAKE3-256 over [`Resolved::canonical`], 64 lowercase hex characters.
    pub fn digest_hex(&self) -> String {
        blake3::hash(&self.canonical).to_hex().to_string()
    }

    /// `configDigest`: `blake3-256:<hex>`.
    pub fn config_digest(&self) -> String {
        format!("{DIGEST_ALGORITHM}:{}", self.digest_hex())
    }
}

/// Read, validate and resolve a document whose kind must be one of
/// `accepted`. A real kind outside `accepted` is `wrong-kind`, naming both.
///
/// Checks run in the order `README.md` "Rules" lists: syntax, `apiVersion`,
/// `kind`, every field in schema order (within an object, keys in input order
/// for `duplicate-key` and `unknown-key`, then each field), then the
/// cross-field rules. The first refusal is returned, so two runs report the
/// same one.
pub fn resolve(
    text: &[u8],
    syntax: Syntax,
    accepted: &[Kind],
    host: &HostFacts,
) -> Result<Resolved, ConfigRefusal> {
    let tree = match syntax {
        Syntax::Json => reader::read_json(text)?,
        Syntax::Toml => reader::read_toml(text)?,
    };
    let document = validate::document(&tree, accepted, host)?;
    let canonical = document.to_value().canonical_bytes();
    Ok(Resolved {
        document,
        canonical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_is_distinct_and_round_trips() {
        let mut codes: Vec<&str> = Reason::ALL.iter().map(|r| r.code()).collect();
        for reason in Reason::ALL {
            assert_eq!(Reason::from_code(reason.code()), Some(*reason));
            assert_eq!(reason.to_string(), reason.code());
        }
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), Reason::ALL.len());
        assert_eq!(Reason::from_code("nope"), None);
    }

    #[test]
    fn exit_codes_follow_d5() {
        assert_eq!(Reason::Unreadable.exit_code(), 66);
        assert_eq!(Reason::UnknownKey.exit_code(), 65);
        assert_eq!(Reason::PublicBindWithoutOptIn.exit_code(), 65);
        assert_eq!(Reason::SignAllowRequired.exit_code(), 65);
        assert_eq!(Reason::CallerTokensRequired.exit_code(), 65);
        for reason in [
            Reason::CgroupRootLinuxOnly,
            Reason::BadHostDefault,
            Reason::UnsupportedOs,
            Reason::SecretFilePermissions,
            Reason::SecretFileInvalid,
            Reason::ConfigFilePermissions,
            Reason::ConfigDigestMismatch,
            Reason::MountFeatureNotBuilt,
            Reason::FuseFeatureNotBuilt,
            Reason::HostCheckFailed,
        ] {
            assert_eq!(reason.exit_code(), 78, "{reason}");
        }
    }

    #[test]
    fn kinds_and_syntaxes_name_themselves() {
        for kind in Kind::ALL {
            assert_eq!(Kind::from_name(kind.name()), Some(*kind));
            assert!(published_schema(*kind).contains(kind.name()));
        }
        assert_eq!(Kind::from_name("FooConfig"), None);
        assert_eq!(Kind::Daemon.short_name(), "daemon-config");
        assert_eq!(
            Kind::ExecutionDaemon.short_name(),
            "execution-daemon-config"
        );
        assert_eq!(Kind::SignHelper.short_name(), "sign-helper-config");
        assert_eq!(
            Syntax::for_path(Path::new("a/b.json")).unwrap(),
            Syntax::Json
        );
        assert_eq!(Syntax::for_path(Path::new("b.toml")).unwrap(), Syntax::Toml);
        for bad in ["b.yaml", "b", "b.JSON"] {
            assert_eq!(
                Syntax::for_path(Path::new(bad)).unwrap_err().reason,
                Reason::UnknownConfigSyntax
            );
        }
        assert_eq!(Os::MacOs.name(), "macos");
        assert_eq!(Os::Linux.name(), "linux");
    }

    #[test]
    fn refusal_line_is_json_with_sorted_keys() {
        let r = ConfigRefusal::new("mcp.http.bind", Reason::PublicBindWithoutOptIn, "a \"q\"\n");
        assert_eq!(
            r.to_json_line(),
            r#"{"field":"mcp.http.bind","message":"a \"q\"\n","reason":"public-bind-without-opt-in"}"#
        );
        assert_eq!(
            r.to_string(),
            "mcp.http.bind: public-bind-without-opt-in: a \"q\"\n"
        );
        let whole = ConfigRefusal::new("", Reason::Malformed, "m");
        assert_eq!(whole.to_string(), "malformed: m");
        assert_eq!(
            json_escape("\u{1}\u{7f}\u{2028}\t\r\\é"),
            "\"\\u0001\\u007f\\u2028\\t\\r\\\\é\""
        );
    }

    #[test]
    fn canonical_writer_matches_d6() {
        let v = Value::object([
            ("b", Value::Array(vec![])),
            ("a", Value::object::<_, String>([])),
            (
                "c",
                Value::Array(vec![Value::Int(1), Value::Null, Value::Bool(false)]),
            ),
            (
                "d",
                Value::object([("x", Value::Bool(true)), ("é", Value::str("ü"))]),
            ),
        ]);
        assert_eq!(
            String::from_utf8(v.canonical_bytes()).unwrap(),
            "{\n  \"a\": {},\n  \"b\": [],\n  \"c\": [\n    1,\n    null,\n    false\n  ],\n  \"d\": {\n    \"x\": true,\n    \"é\": \"ü\"\n  }\n}"
        );
        assert_eq!(
            String::from_utf8(Value::str("a\"b").input_json()).unwrap(),
            "\"a\\\"b\""
        );
        assert_eq!(Value::opt(None::<u64>, Value::Int), Value::Null);
        assert_eq!(Value::opt(Some(3), Value::Int), Value::Int(3));
        assert_eq!(Value::strs(["x"]), Value::Array(vec![Value::str("x")]));
    }

    /// Containers inside a list are indented one level past the list.
    #[test]
    fn nested_containers_in_a_list_are_indented() {
        let v = Value::Array(vec![
            Value::object([("a", Value::Int(1))]),
            Value::Array(vec![Value::Int(2)]),
        ]);
        assert_eq!(
            String::from_utf8(v.canonical_bytes()).unwrap(),
            "[\n  {\n    \"a\": 1\n  },\n  [\n    2\n  ]\n]"
        );
    }

    fn mac() -> HostFacts {
        HostFacts {
            home: Some("/h".into()),
            xdg_state_home: None,
            data_dir: Some("/h/data".into()),
            os: Os::MacOs,
        }
    }

    fn resolved(kind: Kind, json: &str) -> Document {
        resolve(json.as_bytes(), Syntax::Json, &[kind], &mac())
            .unwrap_or_else(|e| panic!("{json}: {e}"))
            .document
    }

    /// Every loosening field is reported, and a default document reports
    /// none (D5, "Loosening is visible").
    #[test]
    fn loosenings_name_each_loosened_field() {
        let Document::Daemon(d) = resolved(
            Kind::Daemon,
            r#"{"apiVersion":"config.leyline/v1","kind":"DaemonConfig",
                "mcp":{"noAuth":true,"http":{"port":1,"bind":"0.0.0.0","allowPublic":true}}}"#,
        ) else {
            panic!()
        };
        let fields: Vec<&str> = d.loosenings().into_iter().map(|(f, _)| f).collect();
        assert_eq!(fields, ["mcp.noAuth", "mcp.http.allowPublic"]);
        assert!(d.loosenings().iter().all(|(_, n)| !n.is_empty()));
        let Document::Daemon(d) = resolved(
            Kind::Daemon,
            r#"{"apiVersion":"config.leyline/v1","kind":"DaemonConfig"}"#,
        ) else {
            panic!()
        };
        assert!(d.loosenings().is_empty());

        let exec = |evidence: &str, backend: &str| {
            let Document::ExecutionDaemon(d) = resolved(
                Kind::ExecutionDaemon,
                &format!(
                    r#"{{"apiVersion":"config.leyline/v1","kind":"ExecutionDaemonConfig",
                    "cas":{{"directory":{{"root":"/c","runRoot":"/r"}}}},"worker":{{"path":"/w"}},
                    "catalog":"/k","evidence":{evidence},"backend":{backend}}}"#
                ),
            ) else {
                panic!()
            };
            d.loosenings()
                .into_iter()
                .map(|(f, _)| f)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            exec(
                r#"{"unverified":{}}"#,
                r#"{"microVm":{"tsiHijackInet":true}}"#
            ),
            ["evidence.unverified", "backend.microVm.tsiHijackInet"]
        );
        let verified = r#"{"verified":{"trustKeys":["/t"],"store":"/s"}}"#;
        assert!(exec(verified, r#"{"microVm":{}}"#).is_empty());
        assert!(exec(verified, r#"{"native":{}}"#).is_empty());

        let sign = |auth: bool, allow: bool| {
            let Document::SignHelper(d) = resolved(
                Kind::SignHelper,
                &format!(
                    r#"{{"apiVersion":"config.leyline/v1","kind":"SignHelperConfig",
                    "auth":{{"required":{auth},"callerTokens":{{"file":"/t"}}}},
                    "signAllow":{{"required":{allow},"callers":{{"a":["op://x"]}}}}}}"#
                ),
            ) else {
                panic!()
            };
            d.loosenings()
                .into_iter()
                .map(|(f, _)| f)
                .collect::<Vec<_>>()
        };
        assert_eq!(sign(false, false), ["auth.required", "signAllow.required"]);
        assert!(sign(true, true).is_empty());
    }

    /// The daemon kinds expose their shared sections; the sign helper has
    /// none.
    #[test]
    fn shared_sections_belong_to_the_daemon_kinds() {
        let d = resolved(
            Kind::Daemon,
            r#"{"apiVersion":"config.leyline/v1","kind":"DaemonConfig",
                "parser":{"queryMatchLimit":7},"head":{"requireSignature":true},
                "diagnostics":{"profile":true}}"#,
        );
        let (p, h, g) = d.shared_sections().unwrap();
        assert_eq!(
            (p.query_match_limit, h.require_signature, g.profile),
            (7, true, true)
        );
        let s = resolved(
            Kind::SignHelper,
            r#"{"apiVersion":"config.leyline/v1","kind":"SignHelperConfig",
                "auth":{"required":false},"signAllow":{"required":false}}"#,
        );
        assert!(s.shared_sections().is_none());
        assert_eq!(s.kind(), Kind::SignHelper);
    }

    #[test]
    fn current_host_reads_only_its_conventions() {
        let host = HostFacts::current().unwrap();
        assert_eq!(host.home, std::env::var_os("HOME"));
        assert_eq!(host.xdg_state_home, std::env::var_os("XDG_STATE_HOME"));
        if cfg!(target_os = "macos") {
            assert_eq!(host.os, Os::MacOs);
        }
    }
}
