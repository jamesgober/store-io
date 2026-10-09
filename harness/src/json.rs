//! A minimal JSON value and serializer (no third-party crates).
//!
//! Non-finite numbers serialise as `null`, so a missing measurement can never
//! be read back as a number.

use std::fmt::Write as _;

/// A JSON value. Objects keep insertion order.
#[derive(Debug, Clone)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number.
    Num(f64),
    /// A string.
    Str(String),
    /// An array.
    Arr(Vec<Json>),
    /// An object.
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// A string value.
    pub fn str(s: impl Into<String>) -> Self {
        Self::Str(s.into())
    }

    /// A number, or `null` for `None`.
    #[must_use]
    pub fn opt(x: Option<f64>) -> Self {
        x.map_or(Self::Null, Self::Num)
    }

    /// An object from `(key, value)` pairs.
    pub fn obj<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Json)>) -> Self {
        Self::Obj(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Serialises with two-space indentation.
    #[must_use]
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String, indent: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Num(x) => {
                if x.is_finite() {
                    if x.fract() == 0.0 && x.abs() < 1e15 {
                        let _ = write!(out, "{x:.0}");
                    } else {
                        let _ = write!(out, "{x}");
                    }
                } else {
                    out.push_str("null");
                }
            }
            Self::Str(s) => escape(out, s),
            Self::Arr(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (i, v) in items.iter().enumerate() {
                    pad(out, indent + 1);
                    v.write(out, indent + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                pad(out, indent);
                out.push(']');
            }
            Self::Obj(pairs) => {
                if pairs.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (i, (k, v)) in pairs.iter().enumerate() {
                    pad(out, indent + 1);
                    escape(out, k);
                    out.push_str(": ");
                    v.write(out, indent + 1);
                    if i + 1 < pairs.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                pad(out, indent);
                out.push('}');
            }
        }
    }
}

fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn escape(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialises_every_kind_and_escapes() {
        let v = Json::obj([
            ("a", Json::Num(1.0)),
            ("b", Json::Num(0.5)),
            ("c", Json::Num(f64::NAN)),
            ("d", Json::str("q\"\\\n\u{1}")),
            ("e", Json::Arr(vec![Json::Bool(true), Json::Null])),
            ("f", Json::Obj(Vec::new())),
        ]);
        let s = v.pretty();
        assert!(s.contains("\"a\": 1,"));
        assert!(s.contains("\"b\": 0.5,"));
        assert!(s.contains("\"c\": null,"));
        assert!(s.contains(r#""d": "q\"\\\n\u0001""#));
        assert!(s.contains("true,\n"));
        assert!(s.contains("\"f\": {}"));
    }
}
