//! Small JSON value adapter for configuration and the local API.
//!
//! Parsing delegates to serde_json. This module supplies typed accessors and the
//! existing value representation used by configuration, storage and API callers.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    pub fn num(v: impl Into<f64>) -> Json {
        Json::Num(v.into())
    }

    pub fn str(v: impl Into<String>) -> Json {
        Json::Str(v.into())
    }

    /// Insert or replace a key in an object (no-op on other variants).
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Obj(entries) = self {
            if let Some(slot) = entries.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                entries.push((key.to_string(), value));
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn idx(&self, i: usize) -> Option<&Json> {
        match self {
            Json::Arr(items) => items.get(i),
            _ => None,
        }
    }

    /// Dotted path lookup: `get_path("policy.thermal.enter_c")`.
    pub fn get_path(&self, path: &str) -> Option<&Json> {
        let mut cur = self;
        for part in path.split('.') {
            cur = cur.get(part)?;
        }
        Some(cur)
    }

    pub fn as_f64(&self) -> Option<f64> {
        let value = match self {
            Json::Num(v) => Some(*v),
            Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Json::Str(s) => s.parse().ok(),
            _ => None,
        };
        value.filter(|v| v.is_finite())
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64().map(|v| v.max(0.0) as u64)
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64().map(|v| v as i64)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            Json::Num(v) => Some(*v != 0.0),
            Json::Str(s) => match s.as_str() {
                "true" | "1" | "yes" | "on" => Some(true),
                "false" | "0" | "no" | "off" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> &[Json] {
        match self {
            Json::Arr(items) => items,
            _ => &[],
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn to_string_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(v) => write_num(out, *v),
            Json::Str(s) => write_str(out, s),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Obj(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(out, k);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    fn write_pretty(&self, out: &mut String, indent: usize) {
        let pad = "  ".repeat(indent);
        let pad_in = "  ".repeat(indent + 1);
        match self {
            Json::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&pad_in);
                    item.write_pretty(out, indent + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad);
                out.push(']');
            }
            Json::Obj(entries) if !entries.is_empty() => {
                out.push_str("{\n");
                for (i, (k, v)) in entries.iter().enumerate() {
                    out.push_str(&pad_in);
                    write_str(out, k);
                    out.push_str(": ");
                    v.write_pretty(out, indent + 1);
                    if i + 1 < entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad);
                out.push('}');
            }
            other => other.write(out),
        }
    }
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        self.write(&mut out);
        f.write_str(&out)
    }
}

fn write_num(out: &mut String, v: f64) {
    if !v.is_finite() {
        out.push_str("null");
    } else if (v.fract()).abs() < f64::EPSILON && v.abs() < 9.0e15 {
        let _ = write!(out, "{}", v as i64);
    } else {
        let _ = write!(out, "{:.3}", v);
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parses JSON with `serde_json` and converts into this crate's `Json` type.
///
/// The parser is the part of a JSON implementation where malformed input, deep
/// nesting and unicode edge cases turn into security bugs, so it is delegated to a
/// mature crate (MIT/Apache-2.0). Serialisation stays local: it is a straight
/// forward escaper and is covered by round-trip tests through `serde_json`.
pub fn parse(input: &str) -> Result<Json, String> {
    let value: serde_json::Value = serde_json::from_str(input).map_err(|e| e.to_string())?;
    Ok(from_value(value))
}

fn from_value(v: serde_json::Value) -> Json {
    match v {
        serde_json::Value::Null => Json::Null,
        serde_json::Value::Bool(b) => Json::Bool(b),
        serde_json::Value::Number(n) => Json::Num(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Json::Str(s),
        serde_json::Value::Array(items) => Json::Arr(items.into_iter().map(from_value).collect()),
        serde_json::Value::Object(map) => {
            Json::Obj(map.into_iter().map(|(k, v)| (k, from_value(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_object() {
        let src = r#"{"a":1,"b":[1,2,3],"c":{"d":"x"},"e":true,"f":null}"#;
        let v = parse(src).expect("parse");
        assert_eq!(v.get("a").and_then(|x| x.as_u64()), Some(1));
        assert_eq!(v.get("b").map(|x| x.as_arr().len()), Some(3));
        assert_eq!(v.get_path("c.d").and_then(|x| x.as_str()), Some("x"));
        assert_eq!(v.get("e").and_then(|x| x.as_bool()), Some(true));
        assert!(v.get("f").unwrap().is_null());
        // Re-parsing our own output must succeed and preserve values.
        let again = parse(&v.to_string()).expect("reparse");
        assert_eq!(again, v);
    }

    #[test]
    fn handles_escapes_and_unicode() {
        let v = parse(r#"{"s":"a\"b\\c\n中"}"#).expect("parse");
        assert_eq!(v.get("s").and_then(|x| x.as_str()), Some("a\"b\\c\n中"));
        let out = v.to_string();
        assert!(parse(&out).is_ok());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("{").is_err());
        assert!(parse("{}x").is_err());
        assert!(parse("nul").is_err());
    }

    #[test]
    fn pretty_output_is_parseable() {
        let v = parse(r#"{"a":[1,{"b":2}]}"#).unwrap();
        assert!(parse(&v.to_string_pretty()).is_ok());
    }

    #[test]
    fn numbers_render_without_trailing_zero() {
        assert_eq!(Json::Num(12.0).to_string(), "12");
        assert_eq!(Json::Num(12.5).to_string(), "12.500");
    }

    #[test]
    fn numeric_accessors_reject_non_finite_values() {
        for value in [
            Json::Num(f64::NAN),
            Json::Num(f64::INFINITY),
            Json::Num(f64::NEG_INFINITY),
            Json::str("NaN"),
            Json::str("inf"),
            Json::str("-inf"),
        ] {
            assert_eq!(value.as_f64(), None);
            assert_eq!(value.as_u64(), None);
            assert_eq!(value.as_i64(), None);
        }
        assert_eq!(Json::str("42.5").as_f64(), Some(42.5));
    }
}
