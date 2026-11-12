//! The document reader: JSON (RFC 8259) with strings kept raw, and the TOML
//! adapter, both producing one [`Json`] tree (ADR-0049 D7: one data model).
//!
//! The JSON reader is hand-rolled for the reason `connections` gives: a
//! general library decodes escapes, merges duplicate keys and reads `64.0` as
//! a number, and each of those hides a refusal the spec requires.

use super::{ConfigRefusal, Reason};

/// Nesting deeper than this is `malformed` (ADR-0049 D5).
pub(crate) const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Json {
    Object(Vec<(Str, Json)>),
    Array(Vec<Json>),
    String(Str),
    /// The number's text exactly as written (a TOML integer's decimal text).
    Number(String),
    Bool(bool),
    Null,
}

impl Json {
    /// The JSON type's name, for messages.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Json::Object(_) => "an object",
            Json::Array(_) => "an array",
            Json::String(_) => "a string",
            Json::Number(_) => "a number",
            Json::Bool(_) => "a boolean",
            Json::Null => "null",
        }
    }
}

/// A string as the document holds it. From JSON text these are the bytes
/// between the quotes with no escape decoded, so an escape leaves a `\` that
/// [`Str::clean`] refuses; from TOML they are the decoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Str {
    pub(crate) raw: Vec<u8>,
}

/// Whether `c` may appear in a v1 string (ADR-0049 D5): not U+0000-U+001F,
/// U+007F-U+009F, U+2028, U+2029, `"` or `\`.
pub(crate) fn allowed_char(c: char) -> bool {
    !matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}' | '"' | '\\')
}

/// Whether every character of `s` is allowed.
pub(crate) fn clean_str(s: &str) -> bool {
    s.chars().all(allowed_char)
}

impl Str {
    /// The string, when it is UTF-8 made only of allowed characters.
    pub(crate) fn clean(&self) -> Option<&str> {
        let s = std::str::from_utf8(&self.raw).ok()?;
        clean_str(s).then_some(s)
    }

    /// For messages: the string with every refused byte or character shown
    /// as an escape.
    pub(crate) fn display(&self) -> String {
        let text = String::from_utf8_lossy(&self.raw);
        let mut s = String::new();
        for c in text.chars() {
            if allowed_char(c) {
                s.push(c);
            } else {
                s.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
        }
        s
    }
}

fn syntax(offset: usize) -> ConfigRefusal {
    ConfigRefusal::new(
        "",
        Reason::Malformed,
        format!("not a JSON document this reader accepts (byte {offset})"),
    )
}

/// Read JSON text into a tree. Only syntax and nesting are refused here.
pub(crate) fn read_json(text: &[u8]) -> Result<Json, ConfigRefusal> {
    let mut reader = Reader { text, pos: 0 };
    if text.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(syntax(0));
    }
    reader.ws();
    let value = reader.value(0)?;
    reader.ws();
    if reader.pos != text.len() {
        return Err(syntax(reader.pos));
    }
    Ok(value)
}

struct Reader<'a> {
    text: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), ConfigRefusal> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(syntax(self.pos))
        }
    }

    fn enter(&self, depth: usize) -> Result<usize, ConfigRefusal> {
        if depth < MAX_DEPTH {
            Ok(depth + 1)
        } else {
            Err(syntax(self.pos))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ConfigRefusal> {
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'n') => self.literal(b"null", Json::Null),
            _ => Err(syntax(self.pos)),
        }
    }

    fn literal(&mut self, word: &[u8], value: Json) -> Result<Json, ConfigRefusal> {
        if self.text[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(syntax(self.pos))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ConfigRefusal> {
        let inner = self.enter(depth)?;
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
            let value = self.value(inner)?;
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

    fn array(&mut self, depth: usize) -> Result<Json, ConfigRefusal> {
        let inner = self.enter(depth)?;
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value(inner)?);
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

    /// A string, kept raw: an escape is scanned past so `\"` does not end
    /// the string, but never decoded, so the `\` it leaves is refused later
    /// (`bad-string`, with the field named).
    fn string(&mut self) -> Result<Str, ConfigRefusal> {
        self.expect(b'"')?;
        let start = self.pos;
        loop {
            match self.peek() {
                None => return Err(syntax(self.pos)),
                Some(b'"') => {
                    let raw = self.text[start..self.pos].to_vec();
                    self.pos += 1;
                    return Ok(Str { raw });
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

    /// A number per RFC 8259's grammar, kept as its text.
    fn number(&mut self) -> Result<Json, ConfigRefusal> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while let Some(b'0'..=b'9') = self.peek() {
                    self.pos += 1;
                }
            }
            _ => return Err(syntax(self.pos)),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            self.digits()?;
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.pos += 1;
            }
            self.digits()?;
        }
        // The grammar above admits ASCII only, so this cannot fail.
        let text = String::from_utf8_lossy(&self.text[start..self.pos]).into_owned();
        Ok(Json::Number(text))
    }

    fn digits(&mut self) -> Result<(), ConfigRefusal> {
        if !matches!(self.peek(), Some(b'0'..=b'9')) {
            return Err(syntax(self.pos));
        }
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
        Ok(())
    }
}

/// The TOML adapter (ADR-0049 D7): tables become objects, arrays arrays,
/// strings and booleans map directly, integers keep their decimal text. A
/// datetime or float is `toml-type-not-mapped`, naming its field; TOML's own
/// duplicate-key rule refuses repeats as `malformed`.
pub(crate) fn read_toml(text: &[u8]) -> Result<Json, ConfigRefusal> {
    let text = std::str::from_utf8(text).map_err(|e| {
        ConfigRefusal::new(
            "",
            Reason::Malformed,
            format!("TOML text is not UTF-8 (byte {})", e.valid_up_to()),
        )
    })?;
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        ConfigRefusal::new(
            "",
            Reason::Malformed,
            format!("not a TOML document: {}", e.message()),
        )
    })?;
    toml_table(&table, "", 1)
}

fn toml_table(table: &toml::Table, field: &str, depth: usize) -> Result<Json, ConfigRefusal> {
    if depth > MAX_DEPTH {
        return Err(ConfigRefusal::new(
            field,
            Reason::Malformed,
            format!("nesting deeper than {MAX_DEPTH}"),
        ));
    }
    let mut members = Vec::new();
    for (key, value) in table {
        let child = join(field, key);
        members.push((
            Str {
                raw: key.as_bytes().to_vec(),
            },
            toml_value(value, &child, depth)?,
        ));
    }
    Ok(Json::Object(members))
}

fn toml_value(value: &toml::Value, field: &str, depth: usize) -> Result<Json, ConfigRefusal> {
    Ok(match value {
        toml::Value::String(s) => Json::String(Str {
            raw: s.as_bytes().to_vec(),
        }),
        toml::Value::Integer(i) => Json::Number(i.to_string()),
        toml::Value::Boolean(b) => Json::Bool(*b),
        toml::Value::Float(_) | toml::Value::Datetime(_) => {
            return Err(ConfigRefusal::new(
                field,
                Reason::TomlTypeNotMapped,
                format!(
                    "a TOML {} has no v1 field type",
                    if value.is_float() {
                        "float"
                    } else {
                        "datetime"
                    }
                ),
            ));
        }
        toml::Value::Array(items) => {
            if depth + 1 > MAX_DEPTH {
                return Err(ConfigRefusal::new(
                    field,
                    Reason::Malformed,
                    format!("nesting deeper than {MAX_DEPTH}"),
                ));
            }
            let mut out = Vec::new();
            for (index, item) in items.iter().enumerate() {
                out.push(toml_value(item, &format!("{field}[{index}]"), depth + 1)?);
            }
            Json::Array(out)
        }
        toml::Value::Table(table) => toml_table(table, field, depth + 1)?,
    })
}

/// `prefix.key`, or `key` at the root.
pub(crate) fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting_limit_is_exact() {
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(read_json(ok.as_bytes()).is_ok());
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert_eq!(
            read_json(deep.as_bytes()).unwrap_err().reason,
            Reason::Malformed
        );
        let very = "[".repeat(100_000);
        assert_eq!(
            read_json(very.as_bytes()).unwrap_err().reason,
            Reason::Malformed
        );
    }

    #[test]
    fn escapes_are_scanned_not_decoded() {
        let Json::String(s) = read_json(br#""a\"bA""#).unwrap() else {
            panic!()
        };
        assert_eq!(s.raw, br#"a\"bA"#);
        assert_eq!(s.clean(), None);
        for bad in [&br#""\x""#[..], br#""\u00g0""#, br#""abc"#, b"\"\\"] {
            assert_eq!(read_json(bad).unwrap_err().reason, Reason::Malformed);
        }
    }

    #[test]
    fn numbers_keep_their_text_and_literals_read() {
        assert_eq!(
            read_json(b" [0, -1, 1.5, 2e3, 7E+1, true, false, null] ").unwrap(),
            Json::Array(vec![
                Json::Number("0".into()),
                Json::Number("-1".into()),
                Json::Number("1.5".into()),
                Json::Number("2e3".into()),
                Json::Number("7E+1".into()),
                Json::Bool(true),
                Json::Bool(false),
                Json::Null,
            ])
        );
        for bad in [
            &b"01"[..],
            b"-",
            b"1.",
            b"1e",
            b"tru",
            b"nul",
            b"[1,]",
            b"{\"a\" 1}",
            b"{\"a\":1,}",
            b"{1:1}",
            b"[1 2]",
            b"{} {}",
            b"",
            b"\xEF\xBB\xBF{}",
        ] {
            assert_eq!(
                read_json(bad).unwrap_err().reason,
                Reason::Malformed,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn clean_refuses_every_listed_character() {
        for c in [
            '\u{0}', '\u{1f}', '\u{7f}', '\u{85}', '\u{9f}', '\u{2028}', '\u{2029}', '"', '\\',
        ] {
            assert!(!allowed_char(c), "{c:?}");
        }
        for c in [' ', '~', '\u{a0}', '\u{ff61}', '\u{1f600}', 'é'] {
            assert!(allowed_char(c), "{c:?}");
        }
        assert_eq!(Str { raw: vec![0xff] }.clean(), None);
        assert_eq!(
            Str {
                raw: b"a\x01".to_vec()
            }
            .display(),
            "a\\u{1}"
        );
    }

    #[test]
    fn toml_maps_types_and_refuses_floats_and_datetimes() {
        let doc = read_toml(b"a = 1\nb = \"x\"\nc = [true]\n[d]\ne = -2\n").unwrap();
        let Json::Object(members) = doc else { panic!() };
        assert_eq!(members.len(), 4);
        let float = read_toml(b"[a]\nb = 1.5\n").unwrap_err();
        assert_eq!(
            (float.field.as_str(), float.reason),
            ("a.b", Reason::TomlTypeNotMapped)
        );
        let date = read_toml(b"a = [1979-05-27]\n").unwrap_err();
        assert_eq!(
            (date.field.as_str(), date.reason),
            ("a[0]", Reason::TomlTypeNotMapped)
        );
        assert_eq!(
            read_toml(b"a = 1\na = 2\n").unwrap_err().reason,
            Reason::Malformed
        );
        assert_eq!(read_toml(b"\xff").unwrap_err().reason, Reason::Malformed);
        let deep = format!(
            "a = {}{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        );
        assert_eq!(
            read_toml(deep.as_bytes()).unwrap_err().reason,
            Reason::Malformed
        );
        let deep_tables: String = (0..MAX_DEPTH + 1)
            .map(|i| format!("[{}]\n", vec!["t"; i + 1].join(".")))
            .collect();
        assert_eq!(
            read_toml(deep_tables.as_bytes()).unwrap_err().reason,
            Reason::Malformed
        );
    }
}
