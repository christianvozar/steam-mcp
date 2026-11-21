//! A small, dependency-free JSON implementation.
//!
//! Provides an order-preserving [`Value`] model, a strict RFC 8259 parser with
//! depth limits, and compact / pretty serializers. Integers are kept exact
//! (`i64` / `u64`) so 64-bit Steam identifiers survive round-trips.

use std::fmt::{self, Write as _};

/// Maximum nesting depth accepted by the parser.
pub const MAX_DEPTH: usize = 128;

/// A JSON number that preserves integer precision.
#[derive(Clone, Copy, Debug)]
pub enum Number {
    Int(i64),
    UInt(u64),
    Float(f64),
}

impl Number {
    pub fn as_f64(&self) -> f64 {
        match *self {
            Number::Int(i) => i as f64,
            Number::UInt(u) => u as f64,
            Number::Float(f) => f,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Number::Int(i) => Some(i),
            Number::UInt(u) => i64::try_from(u).ok(),
            Number::Float(f) => {
                if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                    Some(f as i64)
                } else {
                    None
                }
            }
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Number::Int(i) => u64::try_from(i).ok(),
            Number::UInt(u) => Some(u),
            Number::Float(f) => {
                if f.fract() == 0.0 && f >= 0.0 && f <= u64::MAX as f64 {
                    Some(f as u64)
                } else {
                    None
                }
            }
        }
    }

    pub fn is_integer(&self) -> bool {
        match *self {
            Number::Int(_) | Number::UInt(_) => true,
            Number::Float(f) => f.is_finite() && f.fract() == 0.0,
        }
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => a == b,
            (Number::UInt(a), Number::UInt(b)) => a == b,
            (Number::Int(a), Number::UInt(b)) | (Number::UInt(b), Number::Int(a)) => {
                *a >= 0 && *a as u64 == *b
            }
            _ => self.as_f64() == other.as_f64(),
        }
    }
}

/// An insertion-ordered JSON object.
#[derive(Clone, Debug, Default)]
pub struct Map {
    entries: Vec<(String, Value)>,
}

impl Map {
    pub fn new() -> Self {
        Map { entries: Vec::new() }
    }

    pub fn with_capacity(n: usize) -> Self {
        Map { entries: Vec::with_capacity(n) }
    }

    /// Inserts or replaces a key, returning the previous value.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let key = key.into();
        let value = value.into();
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            return Some(std::mem::replace(&mut slot.1, value));
        }
        self.entries.push((key, value));
        None
    }

    /// Appends without a duplicate check (parser fast path; last wins on lookup).
    fn push_unchecked(&mut self, key: String, value: Value) {
        self.entries.push((key, value));
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.entries.iter_mut().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let mut removed = None;
        self.entries.retain_mut(|(k, v)| {
            if k == key {
                removed = Some(std::mem::replace(v, Value::Null));
                false
            } else {
                true
            }
        });
        removed
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|(_, v)| v)
    }

    /// Returns a copy with keys sorted (recursively), used for canonical hashing.
    pub fn sorted(&self) -> Map {
        let mut entries: Vec<(String, Value)> =
            self.entries.iter().map(|(k, v)| (k.clone(), v.canonical())).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        Map { entries }
    }
}

impl PartialEq for Map {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl IntoIterator for Map {
    type Item = (String, Value);
    type IntoIter = std::vec::IntoIter<(String, Value)>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl FromIterator<(String, Value)> for Map {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        let mut m = Map::new();
        for (k, v) in iter {
            m.insert(k, v);
        }
        m
    }
}

/// A JSON value.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Value>),
    Object(Map),
}

static NULL: Value = Value::Null;

impl Value {
    pub fn object() -> Value {
        Value::Object(Map::new())
    }

    /// Builder-style insert for objects. No-op on non-objects.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Value {
        if let Value::Object(m) = &mut self {
            m.insert(key, value);
        }
        self
    }

    /// Inserts a key into an object value.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        if let Value::Object(m) = self {
            m.insert(key, value);
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(m) => m.get(key),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Value::Object(m) => m.get_mut(key),
            _ => None,
        }
    }

    /// Path lookup: `v.at(&["response", "players"])`.
    pub fn at(&self, path: &[&str]) -> &Value {
        let mut cur = self;
        for p in path {
            cur = match cur {
                Value::Object(m) => m.get(p).unwrap_or(&NULL),
                Value::Array(a) => p.parse::<usize>().ok().and_then(|i| a.get(i)).unwrap_or(&NULL),
                _ => &NULL,
            };
        }
        cur
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }
    pub fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }
    pub fn is_number(&self) -> bool {
        matches!(self, Value::Number(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(n.as_f64()),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) => n.as_i64(),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    /// Lenient numeric read: accepts numbers and numeric strings (Steam often
    /// encodes 64-bit numbers as strings).
    pub fn as_f64_lenient(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(n.as_f64()),
            Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
            _ => None,
        }
    }

    pub fn as_u64_lenient(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim().parse::<u64>().ok(),
            _ => None,
        }
    }

    /// Renders a scalar as a string suitable for a query parameter.
    pub fn to_param_string(&self) -> String {
        match self {
            Value::String(s) => s.clone(),
            Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }

    /// The JSON type name as used by JSON Schema.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(n) if n.is_integer() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    /// Recursively sorted representation, for stable hashing / cache keys.
    pub fn canonical(&self) -> Value {
        match self {
            Value::Object(m) => Value::Object(m.sorted()),
            Value::Array(a) => Value::Array(a.iter().map(|v| v.canonical()).collect()),
            other => other.clone(),
        }
    }

    pub fn to_pretty_string(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, Some(0));
        out
    }

    /// Approximate in-memory size, used for cache accounting.
    pub fn approx_size(&self) -> usize {
        match self {
            Value::Null | Value::Bool(_) | Value::Number(_) => 16,
            Value::String(s) => 24 + s.len(),
            Value::Array(a) => 24 + a.iter().map(|v| v.approx_size()).sum::<usize>(),
            Value::Object(m) => {
                24 + m.iter().map(|(k, v)| 24 + k.len() + v.approx_size()).sum::<usize>()
            }
        }
    }
}

impl std::ops::Index<&str> for Value {
    type Output = Value;
    fn index(&self, key: &str) -> &Value {
        self.get(key).unwrap_or(&NULL)
    }
}

impl std::ops::Index<usize> for Value {
    type Output = Value;
    fn index(&self, idx: usize) -> &Value {
        match self {
            Value::Array(a) => a.get(idx).unwrap_or(&NULL),
            _ => &NULL,
        }
    }
}

macro_rules! from_int {
    ($($t:ty => $variant:ident as $cast:ty),*) => {$(
        impl From<$t> for Value {
            fn from(v: $t) -> Value { Value::Number(Number::$variant(v as $cast)) }
        }
    )*};
}
from_int!(i8 => Int as i64, i16 => Int as i64, i32 => Int as i64, i64 => Int as i64,
          u8 => Int as i64, u16 => Int as i64, u32 => Int as i64, u64 => UInt as u64,
          usize => UInt as u64, isize => Int as i64);

impl From<f64> for Value {
    fn from(v: f64) -> Value {
        if v.is_finite() { Value::Number(Number::Float(v)) } else { Value::Null }
    }
}
impl From<f32> for Value {
    fn from(v: f32) -> Value {
        Value::from(v as f64)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Bool(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::String(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::String(v)
    }
}
impl From<&String> for Value {
    fn from(v: &String) -> Value {
        Value::String(v.clone())
    }
}
impl From<Map> for Value {
    fn from(v: Map) -> Value {
        Value::Object(v)
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Value {
        Value::Array(v)
    }
}
impl From<Vec<String>> for Value {
    fn from(v: Vec<String>) -> Value {
        Value::Array(v.into_iter().map(Value::String).collect())
    }
}
impl From<Vec<&str>> for Value {
    fn from(v: Vec<&str>) -> Value {
        Value::Array(v.into_iter().map(Value::from).collect())
    }
}
impl From<&Value> for Value {
    fn from(v: &Value) -> Value {
        v.clone()
    }
}
impl From<Number> for Value {
    fn from(n: Number) -> Value {
        Value::Number(n)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        match v {
            Some(x) => x.into(),
            None => Value::Null,
        }
    }
}

/// Builds a JSON object: `obj! { "a" => 1, "b" => "x" }`.
#[macro_export]
macro_rules! obj {
    () => { $crate::json::Value::Object($crate::json::Map::new()) };
    ($($k:expr => $v:expr),+ $(,)?) => {{
        let mut m = $crate::json::Map::new();
        $( m.insert($k, $crate::json::Value::from($v)); )+
        $crate::json::Value::Object(m)
    }};
}

/// Builds a JSON array: `arr![1, "x", obj!{}]`.
#[macro_export]
macro_rules! arr {
    () => { $crate::json::Value::Array(Vec::new()) };
    ($($v:expr),+ $(,)?) => { $crate::json::Value::Array(vec![$($crate::json::Value::from($v)),+]) };
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        write_value(&mut out, self, None);
        f.write_str(&out)
    }
}

fn write_indent(out: &mut String, n: usize) {
    for _ in 0..n {
        out.push_str("  ");
    }
}

fn write_value(out: &mut String, v: &Value, indent: Option<usize>) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_string(out, s),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if let Some(level) = indent {
                    out.push('\n');
                    write_indent(out, level + 1);
                    write_value(out, item, Some(level + 1));
                } else {
                    write_value(out, item, None);
                }
            }
            if let Some(level) = indent {
                out.push('\n');
                write_indent(out, level);
            }
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if let Some(level) = indent {
                    out.push('\n');
                    write_indent(out, level + 1);
                    write_string(out, k);
                    out.push_str(": ");
                    write_value(out, item, Some(level + 1));
                } else {
                    write_string(out, k);
                    out.push(':');
                    write_value(out, item, None);
                }
            }
            if let Some(level) = indent {
                out.push('\n');
                write_indent(out, level);
            }
            out.push('}');
        }
    }
}

fn write_number(out: &mut String, n: &Number) {
    match *n {
        Number::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Number::UInt(u) => {
            let _ = write!(out, "{u}");
        }
        Number::Float(f) => {
            if !f.is_finite() {
                out.push_str("null");
            } else if f.fract() == 0.0 && f.abs() < 1e15 {
                let _ = write!(out, "{}", f as i64);
            } else {
                let _ = write!(out, "{f}");
            }
        }
    }
}

/// Writes a JSON string literal with escaping.
pub fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // U+2028/U+2029 are valid JSON but break JS consumers and SSE framing tools.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// A parse failure with byte offset.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON parse error at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parses a complete JSON document from a string.
pub fn parse(input: &str) -> Result<Value, ParseError> {
    parse_bytes(input.as_bytes())
}

/// Parses a complete JSON document from UTF-8 bytes.
pub fn parse_bytes(input: &[u8]) -> Result<Value, ParseError> {
    let mut p = Parser { s: input, i: 0 };
    // Tolerate a UTF-8 BOM.
    if p.s.starts_with(&[0xEF, 0xBB, 0xBF]) {
        p.i = 3;
    }
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return Err(p.err("trailing characters after JSON value"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> ParseError {
        ParseError { offset: self.i, message: msg.to_string() }
    }

    fn ws(&mut self) {
        while let Some(&c) = self.s.get(self.i) {
            if c == b' ' || c == b'\n' || c == b'\r' || c == b'\t' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn expect_lit(&mut self, lit: &[u8], v: Value) -> Result<Value, ParseError> {
        if self.s[self.i..].starts_with(lit) {
            self.i += lit.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.err("maximum nesting depth exceeded"));
        }
        match self.peek() {
            None => Err(self.err("unexpected end of input")),
            Some(b'n') => self.expect_lit(b"null", Value::Null),
            Some(b't') => self.expect_lit(b"true", Value::Bool(true)),
            Some(b'f') => self.expect_lit(b"false", Value::Bool(false)),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Array(out));
        }
        loop {
            self.ws();
            out.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(out));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.i += 1;
        let mut m = Map::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Object(m));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected string key in object"));
            }
            let k = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after object key"));
            }
            self.i += 1;
            self.ws();
            let v = self.value(depth + 1)?;
            m.push_unchecked(k, v);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(m));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        if self.i + 4 > self.s.len() {
            return Err(self.err("truncated unicode escape"));
        }
        let mut v = 0u32;
        for k in 0..4 {
            let c = self.s[self.i + k];
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(self.err("invalid hex digit in unicode escape")),
            };
            v = v * 16 + d as u32;
        }
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.i += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            // Fast path: copy a run of plain bytes.
            let start = self.i;
            while let Some(&c) = self.s.get(self.i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            out.extend_from_slice(&self.s[start..self.i]);
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    return String::from_utf8(out).map_err(|_| self.err("invalid UTF-8 in string"));
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.i += 1;
                    match c {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                // High surrogate: must be followed by a low surrogate.
                                if self.s.get(self.i) == Some(&b'\\')
                                    && self.s.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err(self.err("invalid low surrogate"));
                                    }
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    return Err(self.err("unpaired high surrogate"));
                                }
                            } else if (0xDC00..0xE000).contains(&cp) {
                                return Err(self.err("unpaired low surrogate"));
                            }
                            let ch = char::from_u32(cp).ok_or_else(|| self.err("bad code point"))?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(self.err("invalid escape sequence")),
                    }
                }
                Some(_) => return Err(self.err("control character in string")),
            }
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        let mut is_float = false;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.i += 1;
                if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    return Err(self.err("leading zeros are not allowed"));
                }
            }
            Some(c) if c.is_ascii_digit() => {
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            _ => return Err(self.err("invalid number")),
        }
        if self.peek() == Some(b'.') {
            is_float = true;
            self.i += 1;
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(self.err("expected digit after decimal point"));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(self.err("expected digit in exponent"));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        // Safe: the slice contains only ASCII characters validated above.
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("0");
        if !is_float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Number(Number::Int(i)));
            }
            if let Ok(u) = text.parse::<u64>() {
                return Ok(Value::Number(Number::UInt(u)));
            }
        }
        text.parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(|f| Value::Number(Number::Float(f)))
            .ok_or_else(|| self.err("number out of range"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let src = r#"{"a":1,"b":[true,false,null],"c":"x\ny","d":-2.5,"e":{"f":18446744073709551615}}"#;
        let v = parse(src).unwrap();
        assert_eq!(v["a"].as_i64(), Some(1));
        assert_eq!(v["e"]["f"].as_u64(), Some(u64::MAX));
        assert_eq!(v["c"].as_str(), Some("x\ny"));
        let again = parse(&v.to_string()).unwrap();
        assert_eq!(v, again);
        let pretty = parse(&v.to_pretty_string()).unwrap();
        assert_eq!(v, pretty);
    }

    #[test]
    fn steamid_precision() {
        let v = parse(r#"{"steamid":76561197960287930}"#).unwrap();
        assert_eq!(v["steamid"].as_u64(), Some(76561197960287930));
        assert_eq!(v.to_string(), r#"{"steamid":76561197960287930}"#);
    }

    #[test]
    fn unicode_and_surrogates() {
        let v = parse(r#""é😀""#).unwrap();
        assert_eq!(v.as_str(), Some("é😀"));
        assert!(parse(r#""\ud83d""#).is_err());
        assert!(parse("\"a\u{1}\"").is_err());
    }

    #[test]
    fn rejects_invalid() {
        for bad in ["", "01", "[1,]", "{\"a\" 1}", "tru", "1 2", "[", "{\"a\":}", "-", "1.", "1e"] {
            assert!(parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn depth_limit() {
        let deep = "[".repeat(MAX_DEPTH + 5) + &"]".repeat(MAX_DEPTH + 5);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn macros_and_canonical() {
        let v = obj! { "z" => 1, "a" => arr![1, "two", obj!{ "k" => true }] };
        assert_eq!(v.to_string(), r#"{"z":1,"a":[1,"two",{"k":true}]}"#);
        assert_eq!(v.canonical().to_string(), r#"{"a":[1,"two",{"k":true}],"z":1}"#);
    }

    #[test]
    fn escapes_control_chars() {
        let v = Value::from("a\u{0}b\u{2028}");
        assert_eq!(v.to_string(), "\"a\\u0000b\\u2028\"");
    }

    #[test]
    fn number_equality() {
        assert_eq!(parse("42").unwrap(), parse("42.0").unwrap());
        assert_eq!(Value::from(5u64), Value::from(5i64));
    }
}
