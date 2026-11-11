//! `cloister/connections/v1`: parser and canonicalizer (ADR-0046 Slice 1, S2).
//!
//! The connections document is the broker's input contract: one entry per
//! declared connection, each joining a host UNIX endpoint to one loopback
//! TCP target. `connectionsDigest` is BLAKE3-256 over the bytes
//! [`ConnectionsDocument::canonical_bytes`] returns. It names those bytes and
//! nothing else: it is not `confinementDigest`, a blob hash, or any arena or
//! store root. This module computes no digest; callers hash the canonical
//! bytes, so the crate keeps no runtime dependency.
//!
//! The reader is strict on purpose and hand-rolled for that reason: a general
//! JSON library decodes escapes, merges duplicate keys and reads `30080.0` as
//! a number, and each of those hides a refusal the spec requires
//! (`connections/v1/README.md`, "Parse refusals"). Every refusal names the
//! connection, the field and a stable [`Refusal`] code; the test vectors pin
//! those three, and the independent Python reference
//! (`connections/v1/ref-impl-py/connections_v1.py`) reaches the same ones.
//!
//! Slice 1 delivers a strict subset of ADR-0046 D2. Members of D2 that slice 1
//! does not deliver (`credentialRef`, `endpoint.tcp`, `target.unix`,
//! `target.https`, `target.cloudSql`, `target.tcp.tls`, every `appProtocol`
//! but `art.tcp`) are refused by name rather than ignored. The type
//! represents every loopback literal (`127.0.0.0/8`, `::1`); which of them a
//! runtime delivers is a runtime check, not a parse rule.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The document's `version` string.
pub const VERSION: &str = "cloister/connections/v1";

/// The one `appProtocol` slice 1 delivers: opaque bytes.
pub const APP_PROTOCOL_TCP: &str = "art.tcp";

/// The `appProtocol` values D2 defines that slice 1 does not deliver.
pub const APP_PROTOCOLS_NOT_DELIVERED: &[&str] = &[
    "art.grpc",
    "art.http",
    "art.mysql",
    "art.postgres",
    "art.redis",
];

/// The longest endpoint path, in bytes: the smallest `sun_path` capacity
/// among the supported platforms (macOS: 104 including the NUL).
pub const MAX_ENDPOINT_PATH_BYTES: usize = 103;

/// Nesting deeper than this is refused as malformed. A slice-1 document
/// nests five levels; the bound keeps the recursive reader's stack fixed.
const MAX_DEPTH: usize = 16;

/// A parsed, validated connections document, connections sorted by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionsDocument {
    connections: Vec<Connection>,
}

/// One declared connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// Unique within the document; a DNS label.
    pub name: String,
    /// The host UNIX socket path the broker listens on.
    pub endpoint_unix: String,
    /// The loopback address the broker dials, exactly as written.
    pub host: IpAddr,
    /// The upstream TCP port, 1–65535.
    pub port: u16,
}

impl Connection {
    /// The canonical spelling of `host`. A validated document holds only
    /// canonical spellings, so this is the committed string.
    pub fn host_text(&self) -> String {
        self.host.to_string()
    }

    /// The target as a receipt records it: `127.0.0.1:30080`, `[::1]:30080`.
    pub fn target_text(&self) -> String {
        match self.host {
            IpAddr::V4(v4) => format!("{v4}:{}", self.port),
            IpAddr::V6(v6) => format!("[{v6}]:{}", self.port),
        }
    }
}

impl ConnectionsDocument {
    /// The connections, in canonical (name byte) order.
    pub fn connections(&self) -> &[Connection] {
        &self.connections
    }

    /// The canonical serialization (README "Canonical form"): UTF-8 without a
    /// BOM, keys sorted at every level, two-space indent, no trailing
    /// newline, connections sorted by name. Every string is printable ASCII
    /// without `"` or `\`, so no escape is ever emitted.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str("{\n  \"connections\": [\n");
        for (i, c) in self.connections.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!("      \"appProtocol\": \"{APP_PROTOCOL_TCP}\",\n"));
            out.push_str("      \"endpoint\": {\n");
            out.push_str(&format!("        \"unix\": \"{}\"\n", c.endpoint_unix));
            out.push_str("      },\n");
            out.push_str(&format!("      \"name\": \"{}\",\n", c.name));
            out.push_str("      \"target\": {\n        \"tcp\": {\n");
            out.push_str(&format!("          \"host\": \"{}\",\n", c.host_text()));
            out.push_str(&format!("          \"port\": {}\n", c.port));
            out.push_str("        }\n      }\n    }");
            if i + 1 < self.connections.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ],\n");
        out.push_str(&format!("  \"version\": \"{VERSION}\"\n}}"));
        out.into_bytes()
    }
}

/// Why a document was refused. [`Refusal::code`] is the stable spelling the
/// test vectors pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// The text is not JSON this reader accepts (syntax, trailing bytes,
    /// nesting, a byte outside a string that JSON does not allow).
    MalformedJson,
    /// One object holds a key twice.
    DuplicateKey,
    /// A string holds an escape or a byte outside U+0020–U+007E.
    StringNotPrintableAscii,
    /// A value has the wrong JSON type (including `null`).
    WrongType,
    /// A required key is absent.
    MissingKey,
    /// A key this version does not define.
    UnknownKey,
    /// `version` is not `cloister/connections/v1`.
    BadVersion,
    /// `connections` is an empty list.
    EmptyConnections,
    /// `name` is not a DNS label.
    BadName,
    /// Two connections share a name.
    DuplicateName,
    /// An `appProtocol` D2 defines but slice 1 does not deliver.
    AppProtocolNotDelivered,
    /// An `appProtocol` outside D2's closed set.
    UnknownAppProtocol,
    /// `endpoint.tcp` (D5's `connectLocal` form), not delivered in slice 1.
    EndpointTcpNotDelivered,
    /// The endpoint path does not start with `/`.
    EndpointNotAbsolute,
    /// The endpoint path is over [`MAX_ENDPOINT_PATH_BYTES`].
    EndpointTooLong,
    /// The endpoint path holds `//`.
    EndpointEmptyComponent,
    /// The endpoint path ends with `/`.
    EndpointTrailingSlash,
    /// The endpoint path holds a `.` or `..` component.
    EndpointDotComponent,
    /// Two connections share an endpoint path.
    DuplicateEndpoint,
    /// `target.unix`, `target.https` or `target.cloudSql`: D2 members slice 1
    /// does not deliver.
    TargetNotDelivered,
    /// `target.tcp.tls`: slice 1 has no TLS client.
    TlsNotDelivered,
    /// `target.tcp.host` is a name (including `localhost`), not an IP literal.
    HostIsName,
    /// `target.tcp.host` is an IP literal outside loopback.
    NonLoopbackTarget,
    /// `target.tcp.host` is a loopback address in a non-canonical spelling.
    NonCanonicalLoopback,
    /// `target.tcp.port` is not a JSON integer.
    PortNotInteger,
    /// `target.tcp.port` is outside 1–65535.
    PortOutOfRange,
    /// `credentialRef` is present; slice 1 carries no credential.
    CredentialRefPresent,
}

impl Refusal {
    /// Every refusal, so a test can require a vector for each.
    pub const ALL: &'static [Refusal] = &[
        Refusal::MalformedJson,
        Refusal::DuplicateKey,
        Refusal::StringNotPrintableAscii,
        Refusal::WrongType,
        Refusal::MissingKey,
        Refusal::UnknownKey,
        Refusal::BadVersion,
        Refusal::EmptyConnections,
        Refusal::BadName,
        Refusal::DuplicateName,
        Refusal::AppProtocolNotDelivered,
        Refusal::UnknownAppProtocol,
        Refusal::EndpointTcpNotDelivered,
        Refusal::EndpointNotAbsolute,
        Refusal::EndpointTooLong,
        Refusal::EndpointEmptyComponent,
        Refusal::EndpointTrailingSlash,
        Refusal::EndpointDotComponent,
        Refusal::DuplicateEndpoint,
        Refusal::TargetNotDelivered,
        Refusal::TlsNotDelivered,
        Refusal::HostIsName,
        Refusal::NonLoopbackTarget,
        Refusal::NonCanonicalLoopback,
        Refusal::PortNotInteger,
        Refusal::PortOutOfRange,
        Refusal::CredentialRefPresent,
    ];

    /// The stable code the vectors pin.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::MalformedJson => "malformed-json",
            Refusal::DuplicateKey => "duplicate-key",
            Refusal::StringNotPrintableAscii => "string-not-printable-ascii",
            Refusal::WrongType => "wrong-type",
            Refusal::MissingKey => "missing-key",
            Refusal::UnknownKey => "unknown-key",
            Refusal::BadVersion => "bad-version",
            Refusal::EmptyConnections => "empty-connections",
            Refusal::BadName => "bad-name",
            Refusal::DuplicateName => "duplicate-name",
            Refusal::AppProtocolNotDelivered => "app-protocol-not-delivered",
            Refusal::UnknownAppProtocol => "unknown-app-protocol",
            Refusal::EndpointTcpNotDelivered => "endpoint-tcp-not-delivered",
            Refusal::EndpointNotAbsolute => "endpoint-not-absolute",
            Refusal::EndpointTooLong => "endpoint-too-long",
            Refusal::EndpointEmptyComponent => "endpoint-empty-component",
            Refusal::EndpointTrailingSlash => "endpoint-trailing-slash",
            Refusal::EndpointDotComponent => "endpoint-dot-component",
            Refusal::DuplicateEndpoint => "duplicate-endpoint",
            Refusal::TargetNotDelivered => "target-not-delivered",
            Refusal::TlsNotDelivered => "tls-not-delivered",
            Refusal::HostIsName => "host-is-name",
            Refusal::NonLoopbackTarget => "non-loopback-target",
            Refusal::NonCanonicalLoopback => "non-canonical-loopback",
            Refusal::PortNotInteger => "port-not-integer",
            Refusal::PortOutOfRange => "port-out-of-range",
            Refusal::CredentialRefPresent => "credential-ref-present",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Refusal::MalformedJson => "not a JSON document this reader accepts",
            Refusal::DuplicateKey => "a key appears twice in one object",
            Refusal::StringNotPrintableAscii => {
                "a string holds an escape or a byte outside printable ASCII"
            }
            Refusal::WrongType => "wrong JSON type",
            Refusal::MissingKey => "required key is absent",
            Refusal::UnknownKey => "unknown key",
            Refusal::BadVersion => "version is not cloister/connections/v1",
            Refusal::EmptyConnections => {
                "an empty connections list is refused; a bundle with no connections emits no document"
            }
            Refusal::BadName => "name is not a DNS label",
            Refusal::DuplicateName => "two connections share this name",
            Refusal::AppProtocolNotDelivered => "appProtocol not delivered in slice 1",
            Refusal::UnknownAppProtocol => "appProtocol outside the closed set",
            Refusal::EndpointTcpNotDelivered => "endpoint tcp not delivered in slice 1",
            Refusal::EndpointNotAbsolute => "endpoint path is not absolute",
            Refusal::EndpointTooLong => "endpoint path is over 103 bytes",
            Refusal::EndpointEmptyComponent => "endpoint path holds an empty component (//)",
            Refusal::EndpointTrailingSlash => "endpoint path ends with /",
            Refusal::EndpointDotComponent => "endpoint path holds a . or .. component",
            Refusal::DuplicateEndpoint => "two connections share this endpoint",
            Refusal::TargetNotDelivered => "target kind not delivered in slice 1",
            Refusal::TlsNotDelivered => "tls not delivered in slice 1",
            Refusal::HostIsName => "target host is a name, not an IP literal",
            Refusal::NonLoopbackTarget => "non-loopback target",
            Refusal::NonCanonicalLoopback => "non-canonical spelling of a loopback address",
            Refusal::PortNotInteger => "port is not a JSON integer",
            Refusal::PortOutOfRange => "port is outside 1-65535",
            Refusal::CredentialRefPresent => "credentialRef present; slice 1 carries no credential",
        }
    }
}

/// A refused document: which connection, which field, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionsError {
    /// The connection's `name` when it is a valid DNS label, otherwise `#<index>`;
    /// `None` for a document-level refusal.
    pub connection: Option<String>,
    /// The field, dotted from the connection (`target.tcp.host`) or from the
    /// document (`version`); empty for a syntax refusal.
    pub field: String,
    /// The stable reason.
    pub reason: Refusal,
    /// Byte offset, for a syntax refusal only.
    pub offset: Option<usize>,
}

impl fmt::Display for ConnectionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "connections/v1 refused")?;
        if let Some(c) = &self.connection {
            write!(f, ": connection `{c}`")?;
        }
        if !self.field.is_empty() {
            write!(f, ": field `{}`", self.field)?;
        }
        write!(f, ": {} ({})", self.reason.describe(), self.reason.code())?;
        if let Some(o) = self.offset {
            write!(f, " at byte {o}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConnectionsError {}

/// Parse and validate a connections document. A valid document is returned
/// sorted by name; it may not be canonical text (key order, whitespace). A
/// caller that requires canonical input compares the input with
/// [`ConnectionsDocument::canonical_bytes`].
pub fn parse(text: &[u8]) -> Result<ConnectionsDocument, ConnectionsError> {
    let value = Reader::new(text).document()?;
    validate(&value)
}

// ---------------------------------------------------------------------------
// The reader: JSON (RFC 8259) with strings kept raw.

#[derive(Debug)]
enum Json {
    Object(Vec<(RawString, Json)>),
    Array(Vec<Json>),
    String(RawString),
    /// The number's text exactly as written.
    Number(String),
    Bool,
    Null,
}

#[derive(Debug)]
struct RawString {
    /// The bytes between the quotes, escapes not decoded.
    raw: Vec<u8>,
}

impl RawString {
    /// Printable ASCII with no escape: the only strings this version admits.
    fn clean(&self) -> Option<&str> {
        if self
            .raw
            .iter()
            .all(|&b| (0x20..=0x7e).contains(&b) && b != b'\\' && b != b'"')
        {
            std::str::from_utf8(&self.raw).ok()
        } else {
            None
        }
    }

    /// For error messages: the raw bytes with anything outside printable
    /// ASCII shown as `\xNN`.
    fn display(&self) -> String {
        let mut s = String::new();
        for &b in &self.raw {
            if (0x20..=0x7e).contains(&b) {
                s.push(b as char);
            } else {
                s.push_str(&format!("\\x{b:02x}"));
            }
        }
        s
    }
}

struct Reader<'a> {
    text: &'a [u8],
    pos: usize,
}

fn syntax(offset: usize) -> ConnectionsError {
    ConnectionsError {
        connection: None,
        field: String::new(),
        reason: Refusal::MalformedJson,
        offset: Some(offset),
    }
}

impl<'a> Reader<'a> {
    fn new(text: &'a [u8]) -> Self {
        Reader { text, pos: 0 }
    }

    fn document(mut self) -> Result<Json, ConnectionsError> {
        self.ws();
        let value = self.value(0)?;
        self.ws();
        if self.pos != self.text.len() {
            return Err(syntax(self.pos));
        }
        Ok(value)
    }

    fn ws(&mut self) {
        while let Some(&b) = self.text.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), ConnectionsError> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(syntax(self.pos))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ConnectionsError> {
        if depth > MAX_DEPTH {
            return Err(syntax(self.pos));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.literal(b"true", Json::Bool),
            Some(b'f') => self.literal(b"false", Json::Bool),
            Some(b'n') => self.literal(b"null", Json::Null),
            _ => Err(syntax(self.pos)),
        }
    }

    fn literal(&mut self, word: &[u8], value: Json) -> Result<Json, ConnectionsError> {
        if self.text[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(syntax(self.pos))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ConnectionsError> {
        self.expect(b'{')?;
        let mut members = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(syntax(self.pos));
            }
            let key = self.string()?;
            self.ws();
            self.expect(b':')?;
            self.ws();
            let value = self.value(depth + 1)?;
            members.push((key, value));
            self.ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(members));
                }
                _ => return Err(syntax(self.pos)),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, ConnectionsError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(syntax(self.pos)),
            }
        }
    }

    /// A string, kept raw. An escape is scanned past (so `\"` does not end
    /// the string) but never decoded; raw control and non-ASCII bytes are
    /// kept, and [`RawString::clean`] refuses all of them later with the field
    /// named. Only an unterminated string or a malformed escape is a syntax
    /// refusal here.
    fn string(&mut self) -> Result<RawString, ConnectionsError> {
        self.expect(b'"')?;
        let start = self.pos;
        loop {
            match self.peek() {
                None => return Err(syntax(self.pos)),
                Some(b'"') => {
                    let raw = self.text[start..self.pos].to_vec();
                    self.pos += 1;
                    return Ok(RawString { raw });
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.pos += 1
                        }
                        Some(b'u') => {
                            self.pos += 1;
                            for _ in 0..4 {
                                match self.peek() {
                                    Some(h) if h.is_ascii_hexdigit() => self.pos += 1,
                                    _ => return Err(syntax(self.pos)),
                                }
                            }
                        }
                        _ => return Err(syntax(self.pos)),
                    }
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// A number per RFC 8259's grammar, returned as its text.
    fn number(&mut self) -> Result<Json, ConnectionsError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(syntax(self.pos)),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            self.digits()?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            self.digits()?;
        }
        let text = std::str::from_utf8(&self.text[start..self.pos])
            .map_err(|_| syntax(start))?
            .to_owned();
        Ok(Json::Number(text))
    }

    fn digits(&mut self) -> Result<(), ConnectionsError> {
        if !matches!(self.peek(), Some(b'0'..=b'9')) {
            return Err(syntax(self.pos));
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Validation. The order of checks is part of the contract: the Python
// reference follows it, and each vector pins the first refusal it meets.

struct Ctx {
    connection: Option<String>,
    /// Dotted prefix for fields, e.g. `target.tcp`.
    prefix: String,
}

impl Ctx {
    fn field(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_owned()
        } else if key.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}.{key}", self.prefix)
        }
    }

    fn refuse(&self, key: &str, reason: Refusal) -> ConnectionsError {
        ConnectionsError {
            connection: self.connection.clone(),
            field: self.field(key),
            reason,
            offset: None,
        }
    }

    fn child(&self, key: &str) -> Ctx {
        Ctx {
            connection: self.connection.clone(),
            prefix: self.field(key),
        }
    }
}

/// Check an object's keys in input order: a duplicate, then a key that one of
/// `refused` names (with its reason), then a key outside `allowed`. Then the
/// keys in `required` (in the order given) must be present. Returns the
/// members keyed by their clean names.
fn members<'j>(
    ctx: &Ctx,
    members: &'j [(RawString, Json)],
    allowed: &[&str],
    refused: &[(&str, Refusal)],
    required: &[&str],
) -> Result<Vec<(&'j str, &'j Json)>, ConnectionsError> {
    let mut seen: Vec<&[u8]> = Vec::new();
    let mut out = Vec::new();
    for (key, value) in members {
        let shown = key.display();
        if seen.contains(&key.raw.as_slice()) {
            return Err(ctx.refuse(&shown, Refusal::DuplicateKey));
        }
        seen.push(&key.raw);
        let name = key.clean();
        if let Some(name) = name
            && let Some((_, reason)) = refused.iter().find(|(k, _)| *k == name)
        {
            return Err(ctx.refuse(name, *reason));
        }
        match name {
            Some(name) if allowed.contains(&name) => out.push((name, value)),
            _ => return Err(ctx.refuse(&shown, Refusal::UnknownKey)),
        }
    }
    for key in required {
        if !out.iter().any(|(k, _)| k == key) {
            return Err(ctx.refuse(key, Refusal::MissingKey));
        }
    }
    Ok(out)
}

fn get<'j>(members: &[(&str, &'j Json)], key: &str) -> Option<&'j Json> {
    members.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn as_object<'j>(
    ctx: &Ctx,
    key: &str,
    value: &'j Json,
) -> Result<&'j [(RawString, Json)], ConnectionsError> {
    match value {
        Json::Object(m) => Ok(m),
        _ => Err(ctx.refuse(key, Refusal::WrongType)),
    }
}

fn as_str<'j>(ctx: &Ctx, key: &str, value: &'j Json) -> Result<&'j str, ConnectionsError> {
    match value {
        Json::String(s) => s
            .clean()
            .ok_or_else(|| ctx.refuse(key, Refusal::StringNotPrintableAscii)),
        _ => Err(ctx.refuse(key, Refusal::WrongType)),
    }
}

fn validate(value: &Json) -> Result<ConnectionsDocument, ConnectionsError> {
    let top = Ctx {
        connection: None,
        prefix: String::new(),
    };
    let Json::Object(raw) = value else {
        return Err(top.refuse("", Refusal::WrongType));
    };

    // `version` first: a document for another version is refused as that,
    // not for a shape this version does not define.
    let version = raw
        .iter()
        .find(|(k, _)| k.raw == b"version")
        .map(|(_, v)| v)
        .ok_or_else(|| top.refuse("version", Refusal::MissingKey))?;
    if as_str(&top, "version", version)? != VERSION {
        return Err(top.refuse("version", Refusal::BadVersion));
    }

    let doc = members(
        &top,
        raw,
        &["connections", "version"],
        &[],
        &["connections", "version"],
    )?;
    let Some(Json::Array(list)) = get(&doc, "connections") else {
        return Err(top.refuse("connections", Refusal::WrongType));
    };
    if list.is_empty() {
        return Err(top.refuse("connections", Refusal::EmptyConnections));
    }

    let mut connections: Vec<Connection> = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        let connection = validate_connection(index, item)?;
        let ctx = Ctx {
            connection: Some(connection.name.clone()),
            prefix: String::new(),
        };
        if connections.iter().any(|c| c.name == connection.name) {
            return Err(ctx.refuse("name", Refusal::DuplicateName));
        }
        if connections
            .iter()
            .any(|c| c.endpoint_unix == connection.endpoint_unix)
        {
            return Err(ctx.refuse("endpoint.unix", Refusal::DuplicateEndpoint));
        }
        connections.push(connection);
    }
    connections.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    Ok(ConnectionsDocument { connections })
}

/// The label a refusal names a connection by: its `name` when that is a valid
/// DNS label, otherwise its position.
fn label(index: usize, item: &Json) -> String {
    if let Json::Object(m) = item
        && let Some((_, Json::String(s))) = m.iter().find(|(k, _)| k.raw == b"name")
        && let Some(name) = s.clean()
        && is_dns_label(name)
    {
        return name.to_owned();
    }
    format!("#{index}")
}

fn validate_connection(index: usize, item: &Json) -> Result<Connection, ConnectionsError> {
    let ctx = Ctx {
        connection: Some(label(index, item)),
        prefix: String::new(),
    };
    let raw = as_object(&ctx, "", item)?;
    let m = members(
        &ctx,
        raw,
        &["appProtocol", "endpoint", "name", "target"],
        &[("credentialRef", Refusal::CredentialRefPresent)],
        &["appProtocol", "endpoint", "name", "target"],
    )?;

    // Values in key order: appProtocol, endpoint, name, target.
    let protocol = as_str(&ctx, "appProtocol", get(&m, "appProtocol").expect("required"))?;
    if protocol != APP_PROTOCOL_TCP {
        let reason = if APP_PROTOCOLS_NOT_DELIVERED.contains(&protocol) {
            Refusal::AppProtocolNotDelivered
        } else {
            Refusal::UnknownAppProtocol
        };
        return Err(ctx.refuse("appProtocol", reason));
    }

    let endpoint = as_object(&ctx, "endpoint", get(&m, "endpoint").expect("required"))?;
    let ectx = ctx.child("endpoint");
    let em = members(
        &ectx,
        endpoint,
        &["unix"],
        &[("tcp", Refusal::EndpointTcpNotDelivered)],
        &["unix"],
    )?;
    let path = as_str(&ectx, "unix", get(&em, "unix").expect("required"))?;
    if let Some(reason) = endpoint_path_refusal(path) {
        return Err(ectx.refuse("unix", reason));
    }

    let name = as_str(&ctx, "name", get(&m, "name").expect("required"))?;
    if !is_dns_label(name) {
        return Err(ctx.refuse("name", Refusal::BadName));
    }

    let target = as_object(&ctx, "target", get(&m, "target").expect("required"))?;
    let tctx = ctx.child("target");
    let tm = members(
        &tctx,
        target,
        &["tcp"],
        &[
            ("cloudSql", Refusal::TargetNotDelivered),
            ("https", Refusal::TargetNotDelivered),
            ("unix", Refusal::TargetNotDelivered),
        ],
        &["tcp"],
    )?;
    let tcp = as_object(&tctx, "tcp", get(&tm, "tcp").expect("required"))?;
    let pctx = tctx.child("tcp");
    let pm = members(
        &pctx,
        tcp,
        &["host", "port"],
        &[("tls", Refusal::TlsNotDelivered)],
        &["host", "port"],
    )?;
    let host_text = as_str(&pctx, "host", get(&pm, "host").expect("required"))?;
    let host = loopback_host(host_text).map_err(|reason| pctx.refuse("host", reason))?;
    let port = port(get(&pm, "port").expect("required")).map_err(|r| pctx.refuse("port", r))?;

    Ok(Connection {
        name: name.to_owned(),
        endpoint_unix: path.to_owned(),
        host,
        port,
    })
}

/// `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`
pub fn is_dns_label(s: &str) -> bool {
    let b = s.as_bytes();
    let inner = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty()
        && b.len() <= 63
        && inner(b[0])
        && inner(b[b.len() - 1])
        && b.iter().all(|&c| inner(c) || c == b'-')
}

/// The endpoint path rules, in the order the spec lists them.
fn endpoint_path_refusal(path: &str) -> Option<Refusal> {
    if !path.starts_with('/') {
        return Some(Refusal::EndpointNotAbsolute);
    }
    if path.len() > MAX_ENDPOINT_PATH_BYTES {
        return Some(Refusal::EndpointTooLong);
    }
    if path.contains("//") {
        return Some(Refusal::EndpointEmptyComponent);
    }
    if path.ends_with('/') {
        return Some(Refusal::EndpointTrailingSlash);
    }
    if path[1..].split('/').any(|c| c == "." || c == "..") {
        return Some(Refusal::EndpointDotComponent);
    }
    None
}

/// A loopback IP literal in its one canonical spelling: an IPv4 dotted quad in
/// `127.0.0.0/8` with no leading zeros, or exactly `::1`.
fn loopback_host(text: &str) -> Result<IpAddr, Refusal> {
    if text == "::1" {
        return Ok(IpAddr::V6(Ipv6Addr::LOCALHOST));
    }
    if let Some(octets) = dotted_quad(text, false) {
        let v4 = Ipv4Addr::from(octets);
        return if octets[0] == 127 {
            Ok(IpAddr::V4(v4))
        } else {
            Err(Refusal::NonLoopbackTarget)
        };
    }
    // Not canonical. Classify what the text spells, so the refusal says
    // whether it is loopback written another way or not loopback at all.
    if let Some(octets) = dotted_quad(text, true) {
        return Err(if octets[0] == 127 {
            Refusal::NonCanonicalLoopback
        } else {
            Refusal::NonLoopbackTarget
        });
    }
    if text.bytes().all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        && let Ok(v6) = text.parse::<Ipv6Addr>()
    {
        let loopback = v6.is_loopback()
            || v6
                .to_ipv4_mapped()
                .is_some_and(|v4| v4.octets()[0] == 127);
        return Err(if loopback {
            Refusal::NonCanonicalLoopback
        } else {
            Refusal::NonLoopbackTarget
        });
    }
    Err(Refusal::HostIsName)
}

/// Four `.`-separated decimal octets, each 0–255. With `lenient` false, an
/// octet may not have a leading zero; with it true, 1–3 digits are accepted.
fn dotted_quad(text: &str, lenient: bool) -> Option<[u8; 4]> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (slot, part) in octets.iter_mut().zip(parts) {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if !lenient && part.len() > 1 && part.starts_with('0') {
            return None;
        }
        *slot = part.parse::<u8>().ok()?;
    }
    Some(octets)
}

/// A JSON integer 1–65535 written with no sign, fraction or exponent.
fn port(value: &Json) -> Result<u16, Refusal> {
    let Json::Number(text) = value else {
        return Err(Refusal::PortNotInteger);
    };
    if text.contains(['.', 'e', 'E']) {
        return Err(Refusal::PortNotInteger);
    }
    if text.starts_with('-') || text.len() > 5 {
        return Err(Refusal::PortOutOfRange);
    }
    match text.parse::<u32>() {
        Ok(n @ 1..=65535) => Ok(n as u16),
        _ => Err(Refusal::PortOutOfRange),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped vectors, compiled in, so `--lib` (the mutation gate's
    /// generic slice) runs the same contract `tests/verify_connections_digest.rs`
    /// checks against the digests and the Python reference.
    #[test]
    fn shipped_vectors_hold() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../connections/v1/test-vectors/connections-cases.json"
        ))
        .unwrap();
        for case in cases["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let doc = parse(case["input"].as_str().unwrap().as_bytes())
                .unwrap_or_else(|e| panic!("case {name}: {e}"));
            let bytes = doc.canonical_bytes();
            assert_eq!(
                std::str::from_utf8(&bytes).unwrap(),
                case["canonical"].as_str().unwrap(),
                "case {name}"
            );
            assert_eq!(parse(&bytes).unwrap(), doc, "case {name}");
        }
        for case in cases["rejects"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let err = parse(case["input"].as_str().unwrap().as_bytes())
                .expect_err(name);
            let want = &case["refusal"];
            assert_eq!(err.reason.code(), want["reason"].as_str().unwrap(), "{name}: {err}");
            assert_eq!(err.field, want["field"].as_str().unwrap(), "{name}: {err}");
            assert_eq!(err.connection.as_deref(), want["connection"].as_str(), "{name}");
        }
    }

    #[test]
    fn multi_document_is_sorted_and_exposes_connections() {
        let doc = parse(include_bytes!(
            "../connections/v1/test-vectors/connections-multi.json"
        ))
        .unwrap();
        let names: Vec<&str> = doc.connections().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["orders-api", "orders-cache-1", "orders-db"]);
        assert_eq!(doc.connections()[0].target_text(), "127.0.0.1:30080");
        assert_eq!(doc.connections()[0].host_text(), "127.0.0.1");
        assert_eq!(doc.connections()[2].port, 30432);
    }

    #[test]
    fn error_display_names_connection_field_and_code() {
        let err = parse(br#"{"version":"cloister/connections/v1","connections":[{"name":"a","appProtocol":"art.tcp","endpoint":{"unix":"/a"},"target":{"tcp":{"host":"localhost","port":1}}}]}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "connections/v1 refused: connection `a`: field `target.tcp.host`: \
             target host is a name, not an IP literal (host-is-name)"
        );
        let err = parse(b"{").unwrap_err();
        assert_eq!(
            err.to_string(),
            "connections/v1 refused: not a JSON document this reader accepts (malformed-json) at byte 1"
        );
        for r in Refusal::ALL {
            assert!(!r.describe().is_empty());
        }
    }

    #[test]
    fn whitespace_and_literals_between_tokens() {
        // Every JSON whitespace byte, `false`, and nested empty containers,
        // reach the shape check (so the reader accepted them) rather than a
        // syntax refusal.
        let text = b" \t\r\n{\"version\" :\t\"cloister/connections/v1\" ,\r\n \"connections\": false } \n";
        assert_eq!(parse(text).unwrap_err().reason, Refusal::WrongType);
        let text = br#"{"version":"cloister/connections/v1","connections":[{"name":"a","appProtocol":"art.tcp","endpoint":{"unix":"/a"},"target":{"tcp":{"host":"127.0.0.1","port":1,"x":[[],{},null,true,-0.5e+2,1E-1]}}}]}"#;
        let err = parse(text).unwrap_err();
        assert_eq!((err.reason, err.field.as_str()), (Refusal::UnknownKey, "target.tcp.x"));
        for bad in [&b"tru"[..], b"[1,]", b"{\"a\" 1}", b"{\"a\":1,}", b"[1 2]", b"\"\\x\"", b"\"\\u12g4\"", b"01", b"-", b"1.", b"1e", b"[", b"{\"a\":1"] {
            assert_eq!(parse(bad).unwrap_err().reason, Refusal::MalformedJson, "{:?}", std::str::from_utf8(bad));
        }
    }

    #[test]
    fn dns_label_bounds() {
        assert!(is_dns_label("a"));
        assert!(is_dns_label(&"a".repeat(63)));
        assert!(!is_dns_label(&"a".repeat(64)));
        assert!(!is_dns_label("-a"));
        assert!(!is_dns_label("a-"));
        assert!(!is_dns_label("A"));
        assert!(!is_dns_label(""));
        assert!(is_dns_label("a-1"));
    }

    #[test]
    fn port_bounds() {
        let n = |s: &str| Json::Number(s.to_owned());
        assert_eq!(port(&n("1")), Ok(1));
        assert_eq!(port(&n("65535")), Ok(65535));
        assert_eq!(port(&n("0")), Err(Refusal::PortOutOfRange));
        assert_eq!(port(&n("65536")), Err(Refusal::PortOutOfRange));
        assert_eq!(port(&n("100000")), Err(Refusal::PortOutOfRange));
        assert_eq!(port(&n("-1")), Err(Refusal::PortOutOfRange));
        assert_eq!(port(&n("1.0")), Err(Refusal::PortNotInteger));
        assert_eq!(port(&n("1e3")), Err(Refusal::PortNotInteger));
        assert_eq!(port(&n("1E3")), Err(Refusal::PortNotInteger));
        assert_eq!(port(&Json::Bool), Err(Refusal::PortNotInteger));
    }

    #[test]
    fn hosts() {
        assert!(loopback_host("127.0.0.1").is_ok());
        assert!(loopback_host("127.255.255.254").is_ok());
        assert!(loopback_host("::1").is_ok());
        assert_eq!(loopback_host("128.0.0.1"), Err(Refusal::NonLoopbackTarget));
        assert_eq!(loopback_host("126.0.0.1"), Err(Refusal::NonLoopbackTarget));
        assert_eq!(loopback_host("010.0.0.1"), Err(Refusal::NonLoopbackTarget));
        assert_eq!(loopback_host("127.0.0.01"), Err(Refusal::NonCanonicalLoopback));
        assert_eq!(loopback_host("127.0.0.256"), Err(Refusal::HostIsName));
        assert_eq!(loopback_host("127.0.0"), Err(Refusal::HostIsName));
        assert_eq!(loopback_host("::2"), Err(Refusal::NonLoopbackTarget));
        assert_eq!(loopback_host("::ffff:10.0.0.1"), Err(Refusal::NonLoopbackTarget));
        assert_eq!(loopback_host("::ffff:127.0.0.1"), Err(Refusal::NonCanonicalLoopback));
        assert_eq!(loopback_host("::"), Err(Refusal::NonLoopbackTarget));
        assert_eq!(loopback_host("[::1]"), Err(Refusal::HostIsName));
        assert_eq!(loopback_host("fe80::1%en0"), Err(Refusal::HostIsName));
    }

    #[test]
    fn endpoint_paths() {
        assert_eq!(endpoint_path_refusal("/a/b.sock"), None);
        assert_eq!(endpoint_path_refusal("/a/.b.sock"), None);
        assert_eq!(endpoint_path_refusal("/a/..b"), None);
        assert_eq!(endpoint_path_refusal("/"), Some(Refusal::EndpointTrailingSlash));
        assert_eq!(endpoint_path_refusal("/."), Some(Refusal::EndpointDotComponent));
        assert_eq!(endpoint_path_refusal("/a/./b"), Some(Refusal::EndpointDotComponent));
        let max = format!("/{}", "a".repeat(MAX_ENDPOINT_PATH_BYTES - 1));
        assert_eq!(endpoint_path_refusal(&max), None);
        let over = format!("/{}", "a".repeat(MAX_ENDPOINT_PATH_BYTES));
        assert_eq!(endpoint_path_refusal(&over), Some(Refusal::EndpointTooLong));
    }

    #[test]
    fn deep_nesting_is_refused_without_overflow() {
        let text = "[".repeat(10_000);
        let err = parse(text.as_bytes()).unwrap_err();
        assert_eq!(err.reason, Refusal::MalformedJson);
    }

    #[test]
    fn every_code_is_distinct() {
        let mut codes: Vec<&str> = Refusal::ALL.iter().map(|r| r.code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), Refusal::ALL.len());
    }

    #[test]
    fn target_text_brackets_ipv6() {
        let c = Connection {
            name: "a".into(),
            endpoint_unix: "/a".into(),
            host: IpAddr::V6(Ipv6Addr::LOCALHOST),
            port: 1,
        };
        assert_eq!(c.target_text(), "[::1]:1");
    }
}
