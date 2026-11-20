//! Minimal strict JSON value model for `.dxo` indexes.
//!
//! The parser rejects duplicate keys at every object level and enforces a
//! nesting depth limit, both required by the container contract. The
//! canonical writer follows RFC 8785 ordering (keys sorted by their UTF-8
//! encoding) with the restricted number space used by v1 indexes: every
//! `u64` is written as a decimal string, so no float formatting exists.

use crate::error::DxoError;

/// Maximum nesting depth accepted for index JSON.
pub(crate) const MAX_DEPTH: usize = 64;

/// A parsed JSON value preserving object key order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json {
    Null,
    Bool(bool),
    /// Raw number text as it appeared; the typed layer only accepts numbers
    /// where the contract allows them, and v1 stores all `u64` as strings.
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn as_obj(&self) -> Result<&[(String, Json)], DxoError> {
        match self {
            Json::Obj(pairs) => Ok(pairs),
            _ => Err(DxoError::BadIndex("expected object")),
        }
    }

    pub(crate) fn as_arr(&self) -> Result<&[Json], DxoError> {
        match self {
            Json::Arr(items) => Ok(items),
            _ => Err(DxoError::BadIndex("expected array")),
        }
    }

    pub(crate) fn as_str(&self) -> Result<&str, DxoError> {
        match self {
            Json::Str(s) => Ok(s),
            _ => Err(DxoError::BadIndex("expected string")),
        }
    }

    /// A decimal string without leading zeros, decoded to `u64`.
    pub(crate) fn as_u64_str(&self) -> Result<u64, DxoError> {
        let s = self.as_str()?;
        if s.is_empty() || (s.len() > 1 && s.starts_with('0')) || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DxoError::BadIndex("u64 must be a plain decimal string"));
        }
        s.parse::<u64>().map_err(|_| DxoError::BadIndex("u64 out of range"))
    }

    /// A 64-character lowercase hex SHA-256.
    pub(crate) fn as_hex32(&self) -> Result<[u8; 32], DxoError> {
        let s = self.as_str()?;
        decode_hex32(s).ok_or(DxoError::BadIndex("expected 64 lowercase hex chars"))
    }

    pub(crate) fn get<'a>(&'a self, key: &str) -> Result<&'a Json, DxoError> {
        self.as_obj()?.iter().find(|(k, _)| k == key).map(|(_, v)| v).ok_or(DxoError::BadIndex("required field missing"))
    }

    pub(crate) fn get_opt<'a>(&'a self, key: &str) -> Result<Option<&'a Json>, DxoError> {
        Ok(self.as_obj()?.iter().find(|(k, _)| k == key).map(|(_, v)| v))
    }

    /// Reject any key outside `allowed`.
    pub(crate) fn deny_extra(&self, allowed: &[&str]) -> Result<(), DxoError> {
        for (k, _) in self.as_obj()? {
            if !allowed.contains(&k.as_str()) {
                return Err(DxoError::BadIndex("unknown field"));
            }
        }
        Ok(())
    }
}

/// Decode 64 lowercase hex chars into 32 bytes.
pub(crate) fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = (s.as_bytes()[i * 2] as char).to_digit(16)? as u8;
        let lo = (s.as_bytes()[i * 2 + 1] as char).to_digit(16)? as u8;
        *byte = hi << 4 | lo;
    }
    Some(out)
}

/// Encode 32 bytes as lowercase hex.
pub(crate) fn encode_hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xF) as u32, 16).unwrap());
    }
    s
}

/// Parse strict JSON: duplicate keys and over-deep nesting are rejected.
pub(crate) fn parse(input: &str) -> Result<Json, DxoError> {
    let bytes = input.as_bytes();
    let mut p = Parser { b: bytes, i: 0 };
    p.skip_ws();
    let value = p.value(0)?;
    p.skip_ws();
    if p.i != bytes.len() {
        return Err(DxoError::BadJson("trailing content"));
    }
    Ok(value)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), DxoError> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        }
        else {
            Err(DxoError::BadJson("unexpected token"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, DxoError> {
        if depth > MAX_DEPTH {
            return Err(DxoError::TooDeep);
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(DxoError::BadJson("unexpected character")),
        }
    }

    fn lit(&mut self, word: &str, out: Json) -> Result<Json, DxoError> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(out)
        }
        else {
            Err(DxoError::BadJson("bad literal"))
        }
    }

    fn number(&mut self) -> Result<Json, DxoError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(DxoError::BadJson("floats are not allowed in indexes"));
        }
        Ok(Json::Num(String::from_utf8_lossy(&self.b[start..self.i]).into_owned()))
    }

    fn string(&mut self) -> Result<String, DxoError> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or(DxoError::BadJson("unterminated string"))?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek().ok_or(DxoError::BadJson("bad escape"))?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(DxoError::BadJson("bad escape")),
                    }
                }
                _ => {
                    // Re-read the UTF-8 sequence starting at the byte we just consumed.
                    let start = self.i - 1;
                    let len = utf8_len(c).ok_or(DxoError::BadJson("invalid utf-8"))?;
                    if start + len > self.b.len() {
                        return Err(DxoError::BadJson("truncated utf-8"));
                    }
                    let s = std::str::from_utf8(&self.b[start..start + len]).map_err(|_| DxoError::BadJson("invalid utf-8"))?;
                    out.push_str(s);
                    self.i = start + len;
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, DxoError> {
        let hi = self.hex4()?;
        if (0xDC00..0xE000).contains(&hi) {
            return Err(DxoError::BadJson("lone low surrogate"));
        }
        let cp = if (0xD800..0xDC00).contains(&hi) {
            if self.peek() != Some(b'\\') || self.b.get(self.i + 1) != Some(&b'u') {
                return Err(DxoError::BadJson("lone high surrogate"));
            }
            self.i += 2;
            let lo = self.hex4()?;
            if !(0xDC00..0xE000).contains(&lo) {
                return Err(DxoError::BadJson("bad surrogate pair"));
            }
            0x1_0000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
        }
        else {
            hi
        };
        char::from_u32(cp).ok_or(DxoError::BadJson("bad code point"))
    }

    fn hex4(&mut self) -> Result<u32, DxoError> {
        if self.i + 4 > self.b.len() {
            return Err(DxoError::BadJson("truncated \\u escape"));
        }
        let s = std::str::from_utf8(&self.b[self.i..self.i + 4]).map_err(|_| DxoError::BadJson("bad \\u escape"))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| DxoError::BadJson("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn object(&mut self, depth: usize) -> Result<Json, DxoError> {
        self.eat(b'{')?;
        let mut pairs: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            if pairs.iter().any(|(k, _)| *k == key) {
                return Err(DxoError::DuplicateKey(key));
            }
            self.skip_ws();
            self.eat(b':')?;
            self.skip_ws();
            let value = self.value(depth + 1)?;
            pairs.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(pairs));
                }
                _ => return Err(DxoError::BadJson("expected , or }")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, DxoError> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(DxoError::BadJson("expected , or ]")),
            }
        }
    }
}

fn utf8_len(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

/// RFC 8785 canonical encoding of a `Json` value.
pub(crate) fn canonical(value: &Json, out: &mut Vec<u8>) {
    match value {
        Json::Null => out.extend_from_slice(b"null"),
        Json::Bool(true) => out.extend_from_slice(b"true"),
        Json::Bool(false) => out.extend_from_slice(b"false"),
        Json::Num(n) => out.extend_from_slice(n.as_bytes()),
        Json::Str(s) => encode_string(s, out),
        Json::Arr(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical(item, out);
            }
            out.push(b']');
        }
        Json::Obj(pairs) => {
            let mut sorted: Vec<&(String, Json)> = pairs.iter().collect();
            sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push(b'{');
            for (i, (k, v)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                encode_string(k, out);
                out.push(b':');
                canonical(v, out);
            }
            out.push(b'}');
        }
    }
}

/// Canonical bytes of a value.
pub(crate) fn canonical_bytes(value: &Json) -> Vec<u8> {
    let mut out = Vec::new();
    canonical(value, &mut out);
    out
}

fn encode_string(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// Helper to build an object value from ordered (key, value) pairs.
pub(crate) fn obj<const N: usize>(pairs: [(&str, Json); N]) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Helper to build a string value.
pub(crate) fn jstr(s: impl Into<String>) -> Json {
    Json::Str(s.into())
}

/// Helper to build a u64-as-decimal-string value.
pub(crate) fn ju64(v: u64) -> Json {
    Json::Str(v.to_string())
}

/// Helper to build an array value.
pub(crate) fn jarr(items: Vec<Json>) -> Json {
    Json::Arr(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_keys() {
        assert!(matches!(parse(r#"{"a":1,"a":2}"#), Err(DxoError::DuplicateKey(_))));
    }

    #[test]
    fn rejects_floats() {
        assert!(matches!(parse(r#"{"a":1.5}"#), Err(DxoError::BadJson(_))));
    }

    #[test]
    fn rejects_deep_nesting() {
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 2), "]".repeat(MAX_DEPTH + 2));
        assert!(matches!(parse(&deep), Err(DxoError::TooDeep)));
    }

    #[test]
    fn canonical_sorts_keys_and_preserves_arrays() {
        let v = parse(r#"{"b":"x","a":[3,1,2],"c":{"z":1,"y":2}}"#).unwrap();
        let c = String::from_utf8(canonical_bytes(&v)).unwrap();
        assert_eq!(c, r#"{"a":[3,1,2],"b":"x","c":{"y":2,"z":1}}"#);
    }

    #[test]
    fn u64_string_rules() {
        assert_eq!(jstr("42").as_u64_str().unwrap(), 42);
        assert!(jstr("0").as_u64_str().unwrap() == 0);
        assert!(jstr("01").as_u64_str().is_err());
        assert!(jstr("1_0").as_u64_str().is_err());
    }

    #[test]
    fn hex32_roundtrip() {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8 * 3;
        }
        let hex = encode_hex32(&bytes);
        assert_eq!(decode_hex32(&hex).unwrap(), bytes);
        assert!(decode_hex32(&hex.to_uppercase()).is_none());
    }

    #[test]
    fn surrogate_pairs_decode() {
        let v = parse(r#""😀""#).unwrap();
        assert_eq!(v.as_str().unwrap(), "😀");
    }
}
