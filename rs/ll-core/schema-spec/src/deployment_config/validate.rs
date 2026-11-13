//! Validation and resolution: from a [`Json`] tree to a typed [`Document`].
//!
//! The order of checks is part of the contract (README "Rules"): within an
//! object, keys in input order for `duplicate-key` and `unknown-key`; then
//! each field in the order this file reads it, which is the schema's order;
//! then the kind's cross-field rules. The first refusal is returned.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use super::model::*;
use super::reader::{Json, Str, clean_str, join};
use super::{API_VERSION, ConfigRefusal, HostFacts, Kind, Os, Reason};

type R<T> = Result<T, ConfigRefusal>;

fn refuse<T>(field: &str, reason: Reason, message: impl Into<String>) -> R<T> {
    Err(ConfigRefusal::new(field, reason, message))
}

/// The largest integer any v1 field admits (2^53 − 1).
pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// An object's members after the key checks, keyed by their clean names.
struct Obj<'j> {
    field: String,
    members: Vec<(&'j str, &'j Json)>,
}

impl<'j> Obj<'j> {
    /// Check `value` is an object whose keys, in input order, are each
    /// distinct and in `allowed`.
    fn new(field: &str, value: &'j Json, allowed: &[&str]) -> R<Obj<'j>> {
        let Json::Object(members) = value else {
            return refuse(
                field,
                Reason::WrongType,
                format!("expected an object, found {}", value.type_name()),
            );
        };
        let mut seen: Vec<&[u8]> = Vec::new();
        let mut out = Vec::new();
        for (key, value) in members {
            if seen.contains(&key.raw.as_slice()) {
                return refuse(
                    &join(field, &key.display()),
                    Reason::DuplicateKey,
                    "a key appears twice in one object",
                );
            }
            seen.push(&key.raw);
            match key.clean() {
                Some(name) if allowed.contains(&name) => out.push((name, value)),
                _ => {
                    return refuse(
                        &join(field, &key.display()),
                        Reason::UnknownKey,
                        format!(
                            "not a key of {}",
                            if field.is_empty() {
                                "the document"
                            } else {
                                field
                            }
                        ),
                    );
                }
            }
        }
        Ok(Obj {
            field: field.to_owned(),
            members: out,
        })
    }

    fn child(&self, key: &str) -> String {
        join(&self.field, key)
    }

    fn get(&self, key: &str) -> Option<&'j Json> {
        self.members
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
    }

    /// A field whose default is not `null`: absent gives `None` (use the
    /// default), `null` is refused.
    fn non_null(&self, key: &str) -> R<Option<&'j Json>> {
        match self.get(key) {
            Some(Json::Null) => refuse(
                &self.child(key),
                Reason::NullNotAllowed,
                "null is accepted only where the default is null",
            ),
            other => Ok(other),
        }
    }

    /// A field whose default is `null`: absent and `null` are the same.
    fn nullable(&self, key: &str) -> Option<&'j Json> {
        match self.get(key) {
            Some(Json::Null) | None => None,
            Some(v) => Some(v),
        }
    }

    /// A required field.
    fn required(&self, key: &str) -> R<&'j Json> {
        match self.non_null(key)? {
            Some(v) => Ok(v),
            None => refuse(
                &self.child(key),
                Reason::MissingKey,
                "a required key is absent",
            ),
        }
    }

    /// An optional object field (default `{}`): its members after the key
    /// checks, or an empty object when absent.
    fn section(&self, key: &str, allowed: &[&str]) -> R<Obj<'j>> {
        match self.non_null(key)? {
            Some(v) => Obj::new(&self.child(key), v, allowed),
            None => Ok(Obj {
                field: self.child(key),
                members: Vec::new(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Scalars.

fn string<'j>(field: &str, value: &'j Json) -> R<&'j str> {
    match value {
        Json::String(s) => clean(field, s),
        other => refuse(
            field,
            Reason::WrongType,
            format!("expected a string, found {}", other.type_name()),
        ),
    }
}

fn clean<'j>(field: &str, s: &'j Str) -> R<&'j str> {
    s.clean().ok_or_else(|| {
        ConfigRefusal::new(
            field,
            Reason::BadString,
            format!(
                "{:?} holds an escape, a refused character or invalid UTF-8",
                s.display()
            ),
        )
    })
}

fn boolean(field: &str, value: &Json) -> R<bool> {
    match value {
        Json::Bool(b) => Ok(*b),
        other => refuse(
            field,
            Reason::WrongType,
            format!("expected a boolean, found {}", other.type_name()),
        ),
    }
}

fn integer(field: &str, value: &Json, min: u64, max: u64) -> R<u64> {
    let Json::Number(text) = value else {
        return refuse(
            field,
            Reason::WrongType,
            format!("expected an integer, found {}", value.type_name()),
        );
    };
    if text.contains(['.', 'e', 'E']) {
        return refuse(
            field,
            Reason::NotAnInteger,
            format!("{text} has a fraction or exponent"),
        );
    }
    let range = || {
        ConfigRefusal::new(
            field,
            Reason::OutOfRange,
            format!("{text} is outside {min}..={max}"),
        )
    };
    if text.starts_with('-') {
        return Err(range());
    }
    let n: u64 = text.parse().map_err(|_| range())?;
    if n < min || n > max {
        return Err(range());
    }
    Ok(n)
}

fn bool_or(obj: &Obj, key: &str, default: bool) -> R<bool> {
    obj.non_null(key)?
        .map_or(Ok(default), |v| boolean(&obj.child(key), v))
}

fn int_or(obj: &Obj, key: &str, min: u64, max: u64, default: u64) -> R<u64> {
    obj.non_null(key)?
        .map_or(Ok(default), |v| integer(&obj.child(key), v, min, max))
}

fn enum_value<'a>(field: &str, value: &Json, allowed: &[&'a str]) -> R<&'a str> {
    let s = string(field, value)?;
    allowed.iter().copied().find(|a| *a == s).ok_or_else(|| {
        ConfigRefusal::new(
            field,
            Reason::NotInEnum,
            format!("{s:?} is not one of {allowed:?}"),
        )
    })
}

/// A label: 1-64 of `[A-Za-z0-9._-]`, starting with a letter. Never all
/// digits, so a JavaScript consumer (which orders integer-like object keys
/// first) orders `signAllow.callers` keys by their bytes.
pub(crate) fn is_label(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_alphabetic()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn label(field: &str, value: &Json) -> R<String> {
    let s = string(field, value)?;
    if is_label(s) {
        Ok(s.to_owned())
    } else {
        refuse(field, Reason::BadLabel, format!("{s:?} is not a label"))
    }
}

fn query_hash(field: &str, value: &Json) -> R<String> {
    let s = string(field, value)?;
    if s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(s.to_owned())
    } else {
        refuse(
            field,
            Reason::BadQueryHash,
            format!("{s:?} is not 64 lowercase hex characters"),
        )
    }
}

/// A URL prefix: a lowercase scheme, `://`, then printable ASCII without
/// space, `"` or `\`; at most 2048 bytes.
pub(crate) fn is_url_prefix(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    let sb = scheme.as_bytes();
    s.len() <= 2048
        && !sb.is_empty()
        && sb[0].is_ascii_lowercase()
        && sb.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'+' | b'.' | b'-')
        })
        && rest
            .bytes()
            .all(|c| (0x21..=0x7e).contains(&c) && c != b'"' && c != b'\\')
}

fn url_prefix(field: &str, value: &Json) -> R<String> {
    let s = string(field, value)?;
    if is_url_prefix(s) {
        Ok(s.to_owned())
    } else {
        refuse(
            field,
            Reason::BadUrlPrefix,
            format!("{s:?} is not a URL prefix"),
        )
    }
}

/// `text` as a dotted quad in any spelling that is a valid address, leading
/// zeros included: used to tell a non-canonical spelling from a non-address.
fn lenient_quad(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            (1..=3).contains(&p.len())
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// An IP literal in its canonical spelling.
fn ip_text(field: &str, s: &str) -> R<IpAddr> {
    let parsed: Option<IpAddr> = if s.contains(':') {
        s.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
    } else {
        s.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
    };
    match parsed {
        Some(ip) if ip.to_string() == s => Ok(ip),
        Some(ip) => refuse(
            field,
            Reason::NonCanonicalAddress,
            format!("{s:?} is spelled {ip} canonically"),
        ),
        None if lenient_quad(s) => refuse(
            field,
            Reason::NonCanonicalAddress,
            format!("{s:?}: an IPv4 octet has a leading zero"),
        ),
        None => refuse(
            field,
            Reason::BadAddress,
            format!("{s:?} is not an IP literal"),
        ),
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 127,
        IpAddr::V6(v6) => v6 == Ipv6Addr::LOCALHOST,
    }
}

/// The sign helper's `bind`: `127.x.y.z:port` or `[::1]:port`.
fn socket(field: &str, value: &Json) -> R<SocketAddr> {
    let s = string(field, value)?;
    let bad = || ConfigRefusal::new(field, Reason::BadAddress, format!("{s:?} is not IP:port"));
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (host, port) = rest.split_once("]:").ok_or_else(bad)?;
        let ip = ip_text(field, host)?;
        if !ip.is_ipv6() {
            return Err(bad());
        }
        (ip, port)
    } else {
        let (host, port) = s.rsplit_once(':').ok_or_else(bad)?;
        if host.contains(':') {
            return Err(bad());
        }
        (ip_text(field, host)?, port)
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    if port.len() > 1 && port.starts_with('0') {
        return refuse(
            field,
            Reason::NonCanonicalAddress,
            format!("{s:?}: the port has a leading zero"),
        );
    }
    let port = match port.parse::<u16>() {
        Ok(p) if p >= 1 => p,
        _ => {
            return refuse(
                field,
                Reason::OutOfRange,
                format!("{s:?}: the port is outside 1..=65535"),
            );
        }
    };
    if !is_loopback(host) {
        return refuse(
            field,
            Reason::BindNotLoopback,
            format!("{s:?}: the sign helper binds loopback only (ADR-0019 requirement 2)"),
        );
    }
    Ok(SocketAddr::new(host, port))
}

// ---------------------------------------------------------------------------
// Paths.

/// Whether `rest` (which starts with `/`) has no empty, `.` or `..`
/// component and no trailing `/`.
fn components_ok(rest: &str) -> bool {
    rest.strip_prefix('/').is_some_and(|tail| {
        tail.split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
    })
}

/// A host directory a default or `~/` is rooted at: UTF-8, clean, absolute,
/// and either `/` or a path with valid components.
fn host_dir(field: &str, name: &str, value: Option<&OsStr>) -> R<String> {
    let bad = |why: &str| {
        ConfigRefusal::new(
            field,
            Reason::BadHostDefault,
            format!("{name} {why}, so this field's value cannot be resolved"),
        )
    };
    let value = value.ok_or_else(|| bad("is unset"))?;
    let s = value.to_str().ok_or_else(|| bad("is not UTF-8"))?;
    if !clean_str(s) {
        return Err(bad("holds a refused character"));
    }
    if s == "/" {
        return Ok(String::new());
    }
    if !s.starts_with('/') {
        return Err(bad("is not absolute"));
    }
    if !components_ok(s) {
        return Err(bad("has an empty, . or .. component or a trailing /"));
    }
    Ok(s.to_owned())
}

/// `base` (as [`host_dir`] returns it: no trailing `/`, empty for `/`)
/// joined with `rest`, which starts with `/`.
fn under(base: &str, rest: &str) -> String {
    format!("{base}{rest}")
}

fn path(field: &str, value: &Json, host: &HostFacts) -> R<String> {
    let s = string(field, value)?;
    if let Some(rest) = s.strip_prefix('~') {
        if !rest.starts_with('/') {
            return refuse(
                field,
                Reason::PathNotAbsolute,
                format!("{s:?}: only ~/ is expanded"),
            );
        }
        if !components_ok(rest) {
            return refuse(
                field,
                Reason::BadPath,
                format!("{s:?} has an empty, . or .. component or a trailing /"),
            );
        }
        let home = host_dir(field, "HOME", host.home.as_deref())?;
        return Ok(under(&home, rest));
    }
    if !s.starts_with('/') {
        return refuse(
            field,
            Reason::PathNotAbsolute,
            format!("{s:?} is neither absolute nor ~/"),
        );
    }
    if !components_ok(s) {
        return refuse(
            field,
            Reason::BadPath,
            format!("{s:?} has an empty, . or .. component or a trailing /"),
        );
    }
    Ok(s.to_owned())
}

fn path_or_null(obj: &Obj, key: &str, host: &HostFacts) -> R<Option<String>> {
    obj.nullable(key)
        .map(|v| path(&obj.child(key), v, host))
        .transpose()
}

/// A list whose items `item` reads, sorted in byte order with a duplicate
/// (after resolution) refused.
fn set<T: Ord + Clone>(
    field: &str,
    value: &Json,
    min_items: usize,
    mut item: impl FnMut(&str, &Json) -> R<T>,
) -> R<Vec<T>> {
    let Json::Array(items) = value else {
        return refuse(
            field,
            Reason::WrongType,
            format!("expected an array, found {}", value.type_name()),
        );
    };
    if items.len() < min_items {
        return refuse(
            field,
            Reason::OutOfRange,
            format!("at least {min_items} entries are required"),
        );
    }
    let mut out = Vec::with_capacity(items.len());
    for (index, v) in items.iter().enumerate() {
        out.push(item(&format!("{field}[{index}]"), v)?);
    }
    out.sort();
    if out.windows(2).any(|w| w[0] == w[1]) {
        return refuse(field, Reason::DuplicateEntry, "a set lists one entry twice");
    }
    Ok(out)
}

fn path_set(obj: &Obj, key: &str, host: &HostFacts, min_items: usize) -> R<Vec<String>> {
    match obj.non_null(key)? {
        Some(v) => set(&obj.child(key), v, min_items, |f, v| path(f, v, host)),
        None => Ok(Vec::new()),
    }
}

/// A secret field's value (D4): `{"file": path}`. `{"value": …}` and any
/// non-object is `secret-inlined`; `keychain` and `vault` are
/// `secret-ref-not-delivered`; any other key `unknown-key`.
fn secret(field: &str, value: &Json, host: &HostFacts) -> R<SecretRef> {
    let Json::Object(members) = value else {
        return refuse(
            field,
            Reason::SecretInlined,
            format!(
                "a secret field takes a reference such as {{\"file\": …}}, not {}",
                value.type_name()
            ),
        );
    };
    let mut seen: Vec<&[u8]> = Vec::new();
    let mut file = None;
    for (key, v) in members {
        if seen.contains(&key.raw.as_slice()) {
            return refuse(
                &join(field, &key.display()),
                Reason::DuplicateKey,
                "a key appears twice in one object",
            );
        }
        seen.push(&key.raw);
        match key.clean() {
            Some("value") => {
                return refuse(
                    field,
                    Reason::SecretInlined,
                    "a secret's value is never written in the document; reference a file",
                );
            }
            Some(variant @ ("keychain" | "vault")) => {
                return refuse(
                    field,
                    Reason::SecretRefNotDelivered,
                    format!("the {variant} reference is reserved and not delivered in v1"),
                );
            }
            Some("file") => file = Some(v),
            _ => {
                return refuse(
                    &join(field, &key.display()),
                    Reason::UnknownKey,
                    "a secret reference's one v1 variant is file",
                );
            }
        }
    }
    let file_field = join(field, "file");
    match file {
        Some(Json::Null) => refuse(
            &file_field,
            Reason::NullNotAllowed,
            "null is accepted only where the default is null",
        ),
        Some(v) => Ok(SecretRef {
            file: path(&file_field, v, host)?,
        }),
        None => refuse(&file_field, Reason::MissingKey, "a required key is absent"),
    }
}

/// A union object: exactly one key of `allowed`.
fn union<'j>(field: &str, value: &'j Json, allowed: &[&str]) -> R<(&'j str, &'j Json)> {
    let obj = Obj::new(field, value, allowed)?;
    match obj.members.as_slice() {
        [] => refuse(
            field,
            Reason::UnionEmpty,
            format!("name one of {allowed:?}"),
        ),
        [one] => Ok(*one),
        _ => refuse(
            field,
            Reason::UnionAmbiguous,
            format!("name exactly one of {allowed:?}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// The document.

pub(crate) fn document(tree: &Json, accepted: &[Kind], host: &HostFacts) -> R<Document> {
    let kind = discriminator(tree, accepted)?;
    match kind {
        Kind::Daemon => daemon(tree, host).map(Document::Daemon),
        Kind::ExecutionDaemon => execution(tree, host).map(Document::ExecutionDaemon),
        Kind::SignHelper => sign_helper(tree, host).map(Document::SignHelper),
    }
}

/// The one value of top-level key `key`, wherever it appears.
fn top_level<'j>(members: &'j [(Str, Json)], key: &str) -> R<Option<&'j Json>> {
    let mut found = members.iter().filter(|(k, _)| k.raw == key.as_bytes());
    let first = found.next().map(|(_, v)| v);
    if found.next().is_some() {
        return refuse(
            key,
            Reason::DuplicateKey,
            "a key appears twice in one object",
        );
    }
    Ok(first)
}

/// `apiVersion`, then `kind`, before anything else (D1).
fn discriminator(tree: &Json, accepted: &[Kind]) -> R<Kind> {
    let Json::Object(members) = tree else {
        return refuse(
            "",
            Reason::WrongType,
            format!("a document is an object, not {}", tree.type_name()),
        );
    };
    let text = |key: &str, value: Option<&Json>| -> R<String> {
        match value {
            None => refuse(key, Reason::MissingKey, "a required key is absent"),
            Some(Json::String(s)) => Ok(s.display()),
            Some(other) => refuse(
                key,
                Reason::WrongType,
                format!("expected a string, found {}", other.type_name()),
            ),
        }
    };
    let api = text("apiVersion", top_level(members, "apiVersion")?)?;
    if api != API_VERSION {
        return refuse(
            "apiVersion",
            Reason::BadApiVersion,
            format!("{api:?}: this binary reads {API_VERSION:?}"),
        );
    }
    let name = text("kind", top_level(members, "kind")?)?;
    let Some(kind) = Kind::from_name(&name) else {
        return refuse(
            "kind",
            Reason::UnknownKind,
            format!(
                "{name:?} is not a kind of {API_VERSION}; kinds: {:?}",
                Kind::ALL.iter().map(|k| k.name()).collect::<Vec<_>>()
            ),
        );
    };
    if !accepted.contains(&kind) {
        return refuse(
            "kind",
            Reason::WrongKind,
            format!(
                "{} configures another process; this one reads {:?}",
                kind.name(),
                accepted.iter().map(|k| k.name()).collect::<Vec<_>>()
            ),
        );
    }
    Ok(kind)
}

fn source(top: &Obj, host: &HostFacts) -> R<Option<SourceSettings>> {
    let Some(v) = top.nullable("source") else {
        return Ok(None);
    };
    let obj = Obj::new(&top.child("source"), v, &["path", "language"])?;
    let path = path(&obj.child("path"), obj.required("path")?, host)?;
    let language = obj
        .nullable("language")
        .map(|v| label(&obj.child("language"), v))
        .transpose()?;
    Ok(Some(SourceSettings { path, language }))
}

fn parser(top: &Obj) -> R<ParserSettings> {
    let obj = top.section(
        "parser",
        &[
            "trustedQueryHashes",
            "queryMatchLimit",
            "queryProgressBudget",
        ],
    )?;
    let trusted_query_hashes = match obj.non_null("trustedQueryHashes")? {
        Some(v) => set(&obj.child("trustedQueryHashes"), v, 0, query_hash)?,
        None => Vec::new(),
    };
    let query_match_limit = int_or(&obj, "queryMatchLimit", 1, 65_536, 4_096)? as u32;
    let query_progress_budget = int_or(&obj, "queryProgressBudget", 1, MAX_SAFE_INTEGER, 100_000)?;
    Ok(ParserSettings {
        trusted_query_hashes,
        query_match_limit,
        query_progress_budget,
    })
}

fn head(top: &Obj, host: &HostFacts) -> R<HeadSettings> {
    let obj = top.section("head", &["signingKey", "trustedKeys", "requireSignature"])?;
    let signing_key = obj
        .nullable("signingKey")
        .map(|v| secret(&obj.child("signingKey"), v, host))
        .transpose()?;
    let trusted_keys = path_set(&obj, "trustedKeys", host, 0)?;
    let require_signature = bool_or(&obj, "requireSignature", false)?;
    Ok(HeadSettings {
        signing_key,
        trusted_keys,
        require_signature,
    })
}

fn diagnostics(top: &Obj, host: &HostFacts) -> R<Diagnostics> {
    let obj = top.section("diagnostics", &["profile", "sheafAblationLog"])?;
    Ok(Diagnostics {
        profile: bool_or(&obj, "profile", false)?,
        sheaf_ablation_log: path_or_null(&obj, "sheafAblationLog", host)?,
    })
}

fn enrichment(top: &Obj, host: &HostFacts) -> R<Enrichment> {
    let obj = top.section("enrichment", &["sessionCorpus"])?;
    Ok(Enrichment {
        session_corpus: path_or_null(&obj, "sessionCorpus", host)?,
    })
}

/// `arena.path` with its extension replaced by `.ctrl`, as
/// `lifecycle::control_path_for` derives it.
fn control_default(arena_path: &str) -> String {
    Path::new(arena_path)
        .with_extension("ctrl")
        .to_string_lossy()
        .into_owned()
}

fn control(top: &Obj, arena_path: &str, host: &HostFacts) -> R<String> {
    match top.non_null("control")? {
        Some(v) => path(&top.child("control"), v, host),
        None => Ok(control_default(arena_path)),
    }
}

const DAEMON_KEYS: &[&str] = &[
    "apiVersion",
    "kind",
    "arena",
    "control",
    "mount",
    "source",
    "shutdownAfterSeconds",
    "mcp",
    "cdc",
    "parser",
    "head",
    "diagnostics",
    "enrichment",
];

fn daemon(tree: &Json, host: &HostFacts) -> R<DaemonConfig> {
    let top = Obj::new("", tree, DAEMON_KEYS)?;

    let arena = top.section("arena", &["path", "sizeMib"])?;
    let arena_path = match arena.non_null("path")? {
        Some(v) => path(&arena.child("path"), v, host)?,
        None => under(
            &host_dir("arena.path", "HOME", host.home.as_deref())?,
            "/.mache/default.arena",
        ),
    };
    let arena_size_mib = int_or(&arena, "sizeMib", 1, 1_048_576, 64)?;
    let control = control(&top, &arena_path, host)?;

    let mount = match top.nullable("mount") {
        None => None,
        Some(v) => {
            let obj = Obj::new("mount", v, &["path", "backend", "nfsPort"])?;
            let path = path(&obj.child("path"), obj.required("path")?, host)?;
            let backend = match obj.non_null("backend")? {
                Some(v) => match enum_value(&obj.child("backend"), v, &["nfs", "fuse"])? {
                    "nfs" => MountBackend::Nfs,
                    _ => MountBackend::Fuse,
                },
                None => match host.os {
                    Os::MacOs => MountBackend::Nfs,
                    Os::Linux => MountBackend::Fuse,
                },
            };
            let nfs_port = int_or(&obj, "nfsPort", 0, 65_535, 0)? as u16;
            Some(MountSettings {
                path,
                backend,
                nfs_port,
            })
        }
    };

    let source = source(&top, host)?;
    let shutdown_after_seconds = top
        .nullable("shutdownAfterSeconds")
        .map(|v| integer("shutdownAfterSeconds", v, 0, MAX_SAFE_INTEGER))
        .transpose()?;

    let mcp_obj = top.section("mcp", &["http", "noAuth", "uds", "groups", "tokenFile"])?;
    let http = match mcp_obj.nullable("http") {
        None => None,
        Some(v) => {
            let obj = Obj::new("mcp.http", v, &["port", "bind", "allowPublic"])?;
            let port = integer("mcp.http.port", obj.required("port")?, 1, 65_535)? as u16;
            let bind = match obj.non_null("bind")? {
                Some(v) => {
                    let s = string("mcp.http.bind", v)?;
                    ip_text("mcp.http.bind", s)?
                }
                None => IpAddr::V4(Ipv4Addr::LOCALHOST),
            };
            let allow_public = bool_or(&obj, "allowPublic", false)?;
            Some(McpHttp {
                port,
                bind,
                allow_public,
            })
        }
    };
    let no_auth = bool_or(&mcp_obj, "noAuth", false)?;
    let uds = path_or_null(&mcp_obj, "uds", host)?;
    let groups = mcp_obj
        .nullable("groups")
        .map(|v| set("mcp.groups", v, 0, label))
        .transpose()?;
    let token_file = match mcp_obj.non_null("tokenFile")? {
        Some(v) => secret("mcp.tokenFile", v, host)?,
        None => SecretRef {
            file: under(
                &host_dir(
                    "mcp.tokenFile",
                    "the platform data directory",
                    host.data_dir.as_deref(),
                )?,
                "/leyline/daemon.token",
            ),
        },
    };
    let mcp = McpSettings {
        http,
        no_auth,
        uds,
        groups,
        token_file,
    };

    let cdc = top.section("cdc", &["enabled", "target"])?;
    let cdc_enabled = bool_or(&cdc, "enabled", false)?;
    if let Some(v) = cdc.non_null("target")? {
        enum_value("cdc.target", v, &[CDC_TARGET_SOURCE_BLOBS])?;
    }

    let config = DaemonConfig {
        arena_path,
        arena_size_mib,
        control,
        mount,
        source,
        shutdown_after_seconds,
        mcp,
        cdc_enabled,
        parser: parser(&top)?,
        head: head(&top, host)?,
        diagnostics: diagnostics(&top, host)?,
        enrichment: enrichment(&top, host)?,
    };

    // Cross-field rules.
    if let Some(http) = &config.mcp.http
        && !is_loopback(http.bind)
        && !http.allow_public
    {
        return refuse(
            "mcp.http.bind",
            Reason::PublicBindWithoutOptIn,
            format!(
                "{} is not loopback; a public bind needs mcp.http.allowPublic: true",
                http.bind
            ),
        );
    }
    Ok(config)
}

const EXECUTION_KEYS: &[&str] = &[
    "apiVersion",
    "kind",
    "arena",
    "control",
    "stateDir",
    "cas",
    "worker",
    "catalog",
    "backend",
    "cdc",
    "evidence",
    "source",
    "parser",
    "head",
    "diagnostics",
    "enrichment",
];

/// The state directory's host default: `<XDG_STATE_HOME>/leyline` when that
/// is set and absolute, else `<HOME>/.local/state/leyline`.
fn state_dir_default(host: &HostFacts) -> R<String> {
    if let Some(xdg) = host
        .xdg_state_home
        .as_deref()
        .filter(|x| x.as_encoded_bytes().first() == Some(&b'/'))
    {
        return Ok(under(
            &host_dir("stateDir", "XDG_STATE_HOME", Some(xdg))?,
            "/leyline",
        ));
    }
    Ok(under(
        &host_dir("stateDir", "HOME", host.home.as_deref())?,
        "/.local/state/leyline",
    ))
}

/// Whether one of `a` and `b` is the other or beneath it.
fn overlaps(a: &str, b: &str) -> bool {
    let beneath = |x: &str, y: &str| x.strip_prefix(y).is_some_and(|r| r.starts_with('/'));
    a == b || beneath(a, b) || beneath(b, a)
}

fn execution(tree: &Json, host: &HostFacts) -> R<ExecutionDaemonConfig> {
    let top = Obj::new("", tree, EXECUTION_KEYS)?;

    let arena = top.section("arena", &["path"])?;
    let arena_path = match arena.non_null("path")? {
        Some(v) => path("arena.path", v, host)?,
        None => under(
            &host_dir("arena.path", "HOME", host.home.as_deref())?,
            "/.mache/execution.arena",
        ),
    };
    let control = control(&top, &arena_path, host)?;
    let state_dir = match top.non_null("stateDir")? {
        Some(v) => path("stateDir", v, host)?,
        None => state_dir_default(host)?,
    };

    let (branch, body) = union("cas", top.required("cas")?, &["directory", "volume"])?;
    let cas = if branch == "directory" {
        let obj = Obj::new("cas.directory", body, &["root", "runRoot"])?;
        CasSettings::Directory {
            root: path("cas.directory.root", obj.required("root")?, host)?,
            run_root: path("cas.directory.runRoot", obj.required("runRoot")?, host)?,
        }
    } else {
        let obj = Obj::new("cas.volume", body, &["sizeCapGib", "runRoot"])?;
        let size_cap_gib = int_or(&obj, "sizeCapGib", 1, 16_777_216, 64)?;
        let run_root = match obj.non_null("runRoot")? {
            Some(v) => path("cas.volume.runRoot", v, host)?,
            None => volume_default_run_root(&state_dir, host.os),
        };
        CasSettings::Volume {
            size_cap_gib,
            run_root,
        }
    };

    let worker_obj = Obj::new(
        "worker",
        top.required("worker")?,
        &["path", "runtimeFiles", "readyTimeoutMs"],
    )?;
    let worker = WorkerSettings {
        path: path("worker.path", worker_obj.required("path")?, host)?,
        runtime_files: path_set(&worker_obj, "runtimeFiles", host, 0)?,
        ready_timeout_ms: int_or(&worker_obj, "readyTimeoutMs", 1, MAX_SAFE_INTEGER, 60_000)?,
    };

    let catalog = path("catalog", top.required("catalog")?, host)?;

    let backend = match top.non_null("backend")? {
        None => BackendSettings::Native { cgroup_root: None },
        Some(v) => {
            let (branch, body) = union("backend", v, &["native", "microVm"])?;
            if branch == "native" {
                let obj = Obj::new("backend.native", body, &["cgroupRoot"])?;
                BackendSettings::Native {
                    cgroup_root: path_or_null(&obj, "cgroupRoot", host)?,
                }
            } else {
                let obj = Obj::new(
                    "backend.microVm",
                    body,
                    &["devices", "tsiHijackInet", "workspace"],
                )?;
                let devices = path_set(&obj, "devices", host, 0)?;
                let tsi_hijack_inet = bool_or(&obj, "tsiHijackInet", false)?;
                let ws = obj.section("workspace", &["ttlMs", "changedTtlMs", "changeWindowMs"])?;
                let workspace = WorkspaceFreshness {
                    ttl_ms: int_or(&ws, "ttlMs", 0, MAX_SAFE_INTEGER, 60_000)?,
                    changed_ttl_ms: int_or(&ws, "changedTtlMs", 0, MAX_SAFE_INTEGER, 1_000)?,
                    change_window_ms: int_or(&ws, "changeWindowMs", 0, MAX_SAFE_INTEGER, 60_000)?,
                };
                BackendSettings::MicroVm {
                    devices,
                    tsi_hijack_inet,
                    workspace,
                }
            }
        }
    };

    let cdc = top.section("cdc", &["enabled"])?;
    let cdc_enabled = bool_or(&cdc, "enabled", false)?;

    let (branch, body) = union(
        "evidence",
        top.required("evidence")?,
        &["unverified", "verified"],
    )?;
    let evidence = if branch == "unverified" {
        Obj::new("evidence.unverified", body, &[])?;
        EvidenceSettings::Unverified
    } else {
        let obj = Obj::new("evidence.verified", body, &["trustKeys", "store"])?;
        let trust_keys = set(
            "evidence.verified.trustKeys",
            obj.required("trustKeys")?,
            1,
            |f, v| path(f, v, host),
        )?;
        let store = path("evidence.verified.store", obj.required("store")?, host)?;
        EvidenceSettings::Verified { trust_keys, store }
    };

    let config = ExecutionDaemonConfig {
        arena_path,
        control,
        state_dir,
        cas,
        worker,
        catalog,
        backend,
        cdc_enabled,
        evidence,
        source: source(&top, host)?,
        parser: parser(&top)?,
        head: head(&top, host)?,
        diagnostics: diagnostics(&top, host)?,
        enrichment: enrichment(&top, host)?,
    };

    // Cross-field rules, in README order.
    let cas_root = config.cas_root(host.os);
    if overlaps(config.run_root(), &cas_root) {
        let field = match config.cas {
            CasSettings::Directory { .. } => "cas.directory.runRoot",
            CasSettings::Volume { .. } => "cas.volume.runRoot",
        };
        return refuse(
            field,
            Reason::RunRootOverlapsCasRoot,
            format!(
                "the run root {} and the CAS root {cas_root} must be disjoint",
                config.run_root()
            ),
        );
    }
    if let BackendSettings::Native {
        cgroup_root: Some(_),
    } = config.backend
        && host.os != Os::Linux
    {
        return refuse(
            "backend.native.cgroupRoot",
            Reason::CgroupRootLinuxOnly,
            format!("cgroup v2 roots exist on Linux, not {}", host.os.name()),
        );
    }
    Ok(config)
}

const SIGN_HELPER_KEYS: &[&str] = &[
    "apiVersion",
    "kind",
    "bind",
    "rateLimitPerSecond",
    "auth",
    "signAllow",
    "resolveAllow",
    "resolveCache",
    "subprocess",
    "health",
    "keychainAccount",
];

fn sign_helper(tree: &Json, host: &HostFacts) -> R<SignHelperConfig> {
    let top = Obj::new("", tree, SIGN_HELPER_KEYS)?;

    let bind = match top.non_null("bind")? {
        Some(v) => socket("bind", v)?,
        None => SocketAddr::from((Ipv4Addr::LOCALHOST, 8786)),
    };
    let rate_limit_per_second =
        int_or(&top, "rateLimitPerSecond", 1, u32::MAX.into(), 1_000)? as u32;

    let auth = top.section("auth", &["required", "callerTokens"])?;
    let auth_required = bool_or(&auth, "required", true)?;
    let caller_tokens = auth
        .nullable("callerTokens")
        .map(|v| secret("auth.callerTokens", v, host))
        .transpose()?;

    let allow = top.section("signAllow", &["required", "callers"])?;
    let sign_allow_required = bool_or(&allow, "required", true)?;
    let mut sign_allow_callers = BTreeMap::new();
    if let Some(v) = allow.non_null("callers")? {
        let Json::Object(members) = v else {
            return refuse(
                "signAllow.callers",
                Reason::WrongType,
                format!("expected an object, found {}", v.type_name()),
            );
        };
        let mut seen: Vec<&[u8]> = Vec::new();
        for (key, prefixes) in members {
            let field = join("signAllow.callers", &key.display());
            if seen.contains(&key.raw.as_slice()) {
                return refuse(
                    &field,
                    Reason::DuplicateKey,
                    "a key appears twice in one object",
                );
            }
            seen.push(&key.raw);
            let name = clean(&field, key)?;
            if !is_label(name) {
                return refuse(&field, Reason::BadLabel, format!("{name:?} is not a label"));
            }
            if let Json::Null = prefixes {
                return refuse(
                    &field,
                    Reason::NullNotAllowed,
                    "null is accepted only where the default is null",
                );
            }
            sign_allow_callers.insert(name.to_owned(), set(&field, prefixes, 0, url_prefix)?);
        }
    }

    let resolve_allow = match top.non_null("resolveAllow")? {
        Some(v) => set("resolveAllow", v, 0, url_prefix)?,
        None => Vec::new(),
    };

    let cache = top.section("resolveCache", &["maxEntries", "ttlMs"])?;
    let resolve_cache_max_entries = int_or(&cache, "maxEntries", 1, MAX_SAFE_INTEGER, 1_024)?;
    let resolve_cache_ttl_ms = cache
        .nullable("ttlMs")
        .map(|v| integer("resolveCache.ttlMs", v, 0, MAX_SAFE_INTEGER))
        .transpose()?;

    let sub = top.section("subprocess", &["opBin", "securityBin"])?;
    let op_bin = path_or_null(&sub, "opBin", host)?;
    let security_bin = path_or_null(&sub, "securityBin", host)?;

    let health = top.section("health", &["probeUrl"])?;
    let health_probe_url = health
        .nullable("probeUrl")
        .map(|v| url_prefix("health.probeUrl", v))
        .transpose()?;

    let keychain_account = match top.non_null("keychainAccount")? {
        Some(v) => label("keychainAccount", v)?,
        None => "cloister".to_owned(),
    };

    let config = SignHelperConfig {
        bind,
        rate_limit_per_second,
        auth_required,
        caller_tokens,
        sign_allow_required,
        sign_allow_callers,
        resolve_allow,
        resolve_cache_max_entries,
        resolve_cache_ttl_ms,
        op_bin,
        security_bin,
        health_probe_url,
        keychain_account,
    };

    // Cross-field rules, in README order. Whether a referenced caller-tokens
    // file holds a caller is a host check (it reads the file).
    if config.auth_required && config.caller_tokens.is_none() {
        return refuse(
            "auth.callerTokens",
            Reason::CallerTokensRequired,
            "auth.required is true and no caller-tokens file is named",
        );
    }
    if config.sign_allow_required && config.sign_allow_callers.values().all(|p| p.is_empty()) {
        return refuse(
            "signAllow.callers",
            Reason::SignAllowRequired,
            "signAllow.required is true and no caller has a URL prefix",
        );
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(raw: &str) -> Json {
        Json::String(Str {
            raw: raw.as_bytes().to_vec(),
        })
    }

    fn host() -> HostFacts {
        HostFacts {
            home: Some("/Users/fixture".into()),
            xdg_state_home: None,
            data_dir: Some("/Users/fixture/Library/Application Support".into()),
            os: Os::MacOs,
        }
    }

    fn reason<T: std::fmt::Debug>(r: R<T>) -> Reason {
        r.unwrap_err().reason
    }

    #[test]
    fn labels() {
        assert!(is_label("a"));
        assert!(is_label("go.mod_x-1"));
        assert!(is_label(&"a".repeat(64)));
        assert!(!is_label(&"a".repeat(65)));
        assert!(!is_label(""));
        assert!(!is_label("-a"));
        assert!(
            !is_label("9"),
            "an integer-like key sorts first in JavaScript"
        );
        assert!(!is_label("10a"));
        assert!(is_label("a10"));
        assert!(!is_label(".a"));
        assert!(!is_label("a b"));
        assert!(!is_label("é"));
    }

    #[test]
    fn url_prefixes() {
        assert!(is_url_prefix("keychain://com.cloister/vault-kek-"));
        assert!(is_url_prefix("op://"));
        assert!(is_url_prefix("a+b.c-1://x~"));
        assert!(!is_url_prefix("Op://x"));
        assert!(!is_url_prefix("://x"));
        assert!(!is_url_prefix("1op://x"));
        assert!(!is_url_prefix("op:/x"));
        assert!(!is_url_prefix("op://a b"));
        assert!(!is_url_prefix("op://a\"b"));
        assert!(!is_url_prefix("op://a\\b"));
        assert!(!is_url_prefix("o_p://x"));
        assert!(is_url_prefix(&format!("op://{}", "a".repeat(2043))));
        assert!(!is_url_prefix(&format!("op://{}", "a".repeat(2044))));
    }

    #[test]
    fn integers() {
        let n = |t: &str| Json::Number(t.into());
        assert_eq!(integer("f", &n("5"), 1, 9).unwrap(), 5);
        assert_eq!(integer("f", &n("1"), 1, 9).unwrap(), 1);
        assert_eq!(integer("f", &n("9"), 1, 9).unwrap(), 9);
        assert_eq!(reason(integer("f", &n("0"), 1, 9)), Reason::OutOfRange);
        assert_eq!(reason(integer("f", &n("10"), 1, 9)), Reason::OutOfRange);
        assert_eq!(reason(integer("f", &n("-0"), 0, 9)), Reason::OutOfRange);
        assert_eq!(reason(integer("f", &n("5.0"), 0, 9)), Reason::NotAnInteger);
        assert_eq!(reason(integer("f", &n("5e0"), 0, 9)), Reason::NotAnInteger);
        assert_eq!(reason(integer("f", &n("5E0"), 0, 9)), Reason::NotAnInteger);
        assert_eq!(
            reason(integer("f", &n("99999999999999999999999"), 0, u64::MAX)),
            Reason::OutOfRange
        );
        assert_eq!(reason(integer("f", &s("5"), 0, 9)), Reason::WrongType);
    }

    #[test]
    fn ip_spellings() {
        assert_eq!(
            ip_text("f", "127.0.0.1").unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            ip_text("f", "::1").unwrap(),
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        );
        assert!(ip_text("f", "0.0.0.0").is_ok());
        assert_eq!(
            reason(ip_text("f", "127.000.0.1")),
            Reason::NonCanonicalAddress
        );
        assert_eq!(
            reason(ip_text("f", "0:0:0:0:0:0:0:1")),
            Reason::NonCanonicalAddress
        );
        assert_eq!(
            reason(ip_text("f", "::FFFF:1.2.3.4")),
            Reason::NonCanonicalAddress
        );
        assert_eq!(reason(ip_text("f", "localhost")), Reason::BadAddress);
        assert_eq!(reason(ip_text("f", "256.0.0.1")), Reason::BadAddress);
        assert_eq!(reason(ip_text("f", "1.2.3")), Reason::BadAddress);
        assert_eq!(reason(ip_text("f", "1.2.3.4444")), Reason::BadAddress);
        assert!(!lenient_quad("1.2.3.a"));
        assert!(is_loopback("127.9.9.9".parse().unwrap()));
        assert!(!is_loopback("128.0.0.1".parse().unwrap()));
        assert!(!is_loopback("::2".parse().unwrap()));
    }

    #[test]
    fn sockets() {
        let ok = |t: &str| socket("bind", &s(t)).unwrap();
        assert_eq!(ok("127.0.0.1:8786").to_string(), "127.0.0.1:8786");
        assert_eq!(ok("[::1]:1").to_string(), "[::1]:1");
        assert_eq!(ok("127.0.0.2:65535").port(), 65535);
        assert_eq!(
            reason(socket("bind", &s("0.0.0.0:8786"))),
            Reason::BindNotLoopback
        );
        assert_eq!(
            reason(socket("bind", &s("[::2]:8786"))),
            Reason::BindNotLoopback
        );
        assert_eq!(
            reason(socket("bind", &s("127.0.0.1:0"))),
            Reason::OutOfRange
        );
        assert_eq!(
            reason(socket("bind", &s("127.0.0.1:65536"))),
            Reason::OutOfRange
        );
        assert_eq!(
            reason(socket("bind", &s("127.0.0.1:08786"))),
            Reason::NonCanonicalAddress
        );
        assert_eq!(
            reason(socket("bind", &s("127.00.0.1:8786"))),
            Reason::NonCanonicalAddress
        );
        for bad in [
            "127.0.0.1",
            "127.0.0.1:",
            "127.0.0.1:x",
            "::1:8786",
            "[127.0.0.1]:1",
            "[::1]8786",
            "a:1",
        ] {
            assert_eq!(reason(socket("bind", &s(bad))), Reason::BadAddress, "{bad}");
        }
    }

    #[test]
    fn paths_and_home() {
        let h = host();
        assert_eq!(path("f", &s("/a/b"), &h).unwrap(), "/a/b");
        assert_eq!(path("f", &s("~/a"), &h).unwrap(), "/Users/fixture/a");
        for (bad, why) in [
            ("a/b", Reason::PathNotAbsolute),
            ("~user/a", Reason::PathNotAbsolute),
            ("~", Reason::PathNotAbsolute),
            ("", Reason::PathNotAbsolute),
            ("/", Reason::BadPath),
            ("~/", Reason::BadPath),
            ("/a/", Reason::BadPath),
            ("/a//b", Reason::BadPath),
            ("/a/./b", Reason::BadPath),
            ("/a/..", Reason::BadPath),
            ("~/./a", Reason::BadPath),
        ] {
            assert_eq!(reason(path("f", &s(bad), &h)), why, "{bad}");
        }
        let root = HostFacts {
            home: Some("/".into()),
            ..host()
        };
        assert_eq!(path("f", &s("~/a"), &root).unwrap(), "/a");
        for home in [
            None,
            Some("rel"),
            Some("/a/"),
            Some("/a\u{2028}"),
            Some("/a/../b"),
        ] {
            let h = HostFacts {
                home: home.map(Into::into),
                ..host()
            };
            let e = path("f", &s("~/a"), &h).unwrap_err();
            assert_eq!(
                (e.field.as_str(), e.reason),
                ("f", Reason::BadHostDefault),
                "{home:?}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let h = HostFacts {
                home: Some(std::ffi::OsString::from_vec(vec![b'/', 0xff])),
                ..host()
            };
            assert_eq!(reason(path("f", &s("~/a"), &h)), Reason::BadHostDefault);
        }
    }

    #[test]
    fn overlap() {
        assert!(overlaps("/a", "/a"));
        assert!(overlaps("/a/b", "/a"));
        assert!(overlaps("/a", "/a/b"));
        assert!(!overlaps("/a", "/ab"));
        assert!(!overlaps("/ab", "/a"));
        assert!(!overlaps("/a/b", "/a/c"));
    }

    #[test]
    fn control_default_replaces_the_extension() {
        assert_eq!(control_default("/x/default.arena"), "/x/default.ctrl");
        assert_eq!(control_default("/x/arena"), "/x/arena.ctrl");
        assert_eq!(control_default("/x/a.b.arena"), "/x/a.b.ctrl");
    }

    #[test]
    fn state_dir_default_prefers_an_absolute_xdg_state_home() {
        let mut h = host();
        assert_eq!(
            state_dir_default(&h).unwrap(),
            "/Users/fixture/.local/state/leyline"
        );
        h.xdg_state_home = Some("relative".into());
        assert_eq!(
            state_dir_default(&h).unwrap(),
            "/Users/fixture/.local/state/leyline"
        );
        h.xdg_state_home = Some("/xdg/state".into());
        assert_eq!(state_dir_default(&h).unwrap(), "/xdg/state/leyline");
        h.xdg_state_home = Some("/xdg/state/".into());
        assert_eq!(reason(state_dir_default(&h)), Reason::BadHostDefault);
        h.xdg_state_home = None;
        h.home = None;
        assert_eq!(reason(state_dir_default(&h)), Reason::BadHostDefault);
    }
}
