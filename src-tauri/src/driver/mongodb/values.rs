//! BSON values as grid cells: text, one-letter codes, parsing an edit, and the preview syntax.

use mongodb::bson::oid::ObjectId;
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{Bson, DateTime, Decimal128};

use crate::model::CellType;

/// First instant of year 1 and last millisecond of year 9999, in epoch milliseconds.
const MIN_RFC3339_MS: i64 = -62_135_596_800_000;
const MAX_RFC3339_MS: i64 = 253_402_300_799_999;

/// A cell's text and code. `None` (a missing field) and `Bson::Null` are both NULL.
pub fn cell(v: Option<&Bson>) -> (Option<String>, CellType) {
    let Some(v) = v else {
        return (None, CellType::Null);
    };
    match v {
        Bson::Null => (None, CellType::Null),
        Bson::Int32(n) => (Some(n.to_string()), CellType::Integer),
        Bson::Int64(n) => (Some(n.to_string()), CellType::Long),
        Bson::Double(f) => (Some(double_text(*f)), CellType::Real),
        Bson::Decimal128(d) => (Some(d.to_string()), CellType::Decimal),
        Bson::String(s) => (Some(s.clone()), CellType::Text),
        Bson::Boolean(b) => (Some(b.to_string()), CellType::Bool),
        Bson::ObjectId(o) => (Some(o.to_hex()), CellType::ObjectId),
        Bson::DateTime(d) => (Some(date_text(*d)), CellType::Date),
        Bson::Document(_) | Bson::Array(_) => (Some(ext_json(v)), CellType::Json),
        other => (Some(canonical(other)), CellType::Other),
    }
}

/// serde_json's number form (`1.0`, `1e+300`, `-0.0`), or `NaN` / `Infinity` / `-Infinity`.
fn double_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        serde_json::to_string(&f).unwrap_or_else(|_| f.to_string())
    }
}

/// RFC 3339 UTC for years 1 to 9999, otherwise the canonical `$date` wrapper.
fn date_text(d: DateTime) -> String {
    let ms = d.timestamp_millis();
    if (MIN_RFC3339_MS..=MAX_RFC3339_MS).contains(&ms)
        && let Ok(s) = d.try_to_rfc3339_string()
    {
        // The library trims trailing zeros of the fraction; the cell shows exactly `.mmm` or nothing.
        let whole = s.split('.').next().unwrap_or(&s).trim_end_matches('Z');
        let frac = ms.rem_euclid(1000);
        return if frac == 0 {
            format!("{whole}Z")
        } else {
            format!("{whole}.{frac:03}Z")
        };
    }
    format!(r#"{{"$date":{{"$numberLong":"{ms}"}}}}"#)
}

/// Canonical extended JSON, compact (key order inside the `$` wrappers is fixed by the spec).
fn canonical(v: &Bson) -> String {
    serde_json::to_string(&v.clone().into_canonical_extjson()).unwrap_or_else(|_| "null".into())
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

/// Compact extended JSON from our own writer (key order kept).
pub fn ext_json(v: &Bson) -> String {
    let mut out = String::new();
    write_ext(v, &mut out);
    out
}

fn write_ext(v: &Bson, out: &mut String) {
    match v {
        Bson::Null => out.push_str("null"),
        Bson::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Bson::Int32(n) => out.push_str(&n.to_string()),
        Bson::Int64(n) => out.push_str(&format!(r#"{{"$numberLong":"{n}"}}"#)),
        Bson::Double(f) if f.is_finite() => out.push_str(&double_text(*f)),
        Bson::Double(f) => out.push_str(&format!(r#"{{"$numberDouble":"{}"}}"#, double_text(*f))),
        Bson::Decimal128(d) => out.push_str(&format!(r#"{{"$numberDecimal":"{d}"}}"#)),
        Bson::String(s) => out.push_str(&json_str(s)),
        Bson::ObjectId(o) => out.push_str(&format!(r#"{{"$oid":"{}"}}"#, o.to_hex())),
        Bson::DateTime(d) => {
            let t = date_text(*d);
            if t.starts_with('{') {
                out.push_str(&t);
            } else {
                out.push_str(&format!(r#"{{"$date":"{t}"}}"#));
            }
        }
        Bson::Document(doc) => {
            out.push('{');
            for (i, (k, val)) in doc.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_str(k));
                out.push(':');
                write_ext(val, out);
            }
            out.push('}');
        }
        Bson::Array(items) => {
            out.push('[');
            for (i, val) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_ext(val, out);
            }
            out.push(']');
        }
        other => out.push_str(&canonical(other)),
    }
}

fn type_label(code: CellType) -> Option<&'static str> {
    Some(match code {
        CellType::Integer => "a whole number (int32)",
        CellType::Long => "a whole number (int64)",
        CellType::Real => "a number",
        CellType::Decimal => "a decimal number",
        CellType::Bool => "true or false",
        CellType::ObjectId => "an ObjectId (24 hex digits)",
        CellType::Date => "a date (e.g. 2026-01-02T03:04:05Z)",
        CellType::Json | CellType::Other => "a JSON value",
        _ => return None,
    })
}

pub(crate) fn parse_date(text: &str) -> Option<Bson> {
    let t = text.trim();
    if t.starts_with('{') {
        return match serde_json::from_str::<Bson>(t) {
            Ok(b @ Bson::DateTime(_)) => Some(b),
            _ => None,
        };
    }
    let bare = t.len() == 10
        && t.as_bytes().iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    let full = if bare {
        format!("{t}T00:00:00Z")
    } else {
        t.to_string()
    };
    DateTime::parse_rfc3339_str(full).ok().map(Bson::DateTime)
}

/// Parses an edited cell by the cell's own code. A cell whose code is `n` (null or missing) and
/// every inserted value must be parsed by the caller with `parse_new(text, column_type)`; the
/// `Null` branch here is only the no-type fallback.
pub fn parse_cell(text: &str, code: CellType, field: &str) -> Result<Bson, String> {
    let t = text.trim();
    let parsed = match code {
        CellType::Text => return Ok(Bson::String(text.to_string())),
        CellType::Null => return Ok(parse_new(text, None)),
        CellType::Integer => t.parse::<i32>().ok().map(Bson::Int32),
        CellType::Long => t.parse::<i64>().ok().map(Bson::Int64),
        CellType::Real => t.parse::<f64>().ok().map(Bson::Double),
        CellType::Decimal => t.parse::<Decimal128>().ok().map(Bson::Decimal128),
        CellType::Bool => t.parse::<bool>().ok().map(Bson::Boolean),
        CellType::ObjectId => ObjectId::parse_str(t).ok().map(Bson::ObjectId),
        CellType::Date => parse_date(text),
        CellType::Json | CellType::Other => parse_json(t),
        CellType::Blob => None,
    };
    parsed.ok_or_else(|| match type_label(code) {
        Some(name) => format!(r#"{field}: expected {name}, got "{text}"."#),
        None => format!("{field}: this value can't be edited."),
    })
}

/// A number written with digits (not the words `nan` / `inf` that `f64::from_str` accepts).
fn numeric_f64(t: &str) -> Option<f64> {
    if t.bytes().any(|b| b.is_ascii_digit())
        && !t
            .bytes()
            .any(|b| b.is_ascii_alphabetic() && b != b'e' && b != b'E')
    {
        t.parse::<f64>().ok().filter(|f| f.is_finite())
    } else {
        None
    }
}

/// Parses a value for a cell that was NULL or for an inserted document, by the column's dominant type.
pub fn parse_new(text: &str, column_type: Option<&str>) -> Bson {
    let t = text.trim();
    let Some(ty) = column_type else {
        if let Ok(n) = t.parse::<i32>() {
            return Bson::Int32(n);
        }
        if let Ok(n) = t.parse::<i64>() {
            return Bson::Int64(n);
        }
        if let Some(f) = numeric_f64(t) {
            return Bson::Double(f);
        }
        return match t {
            "true" => Bson::Boolean(true),
            "false" => Bson::Boolean(false),
            _ => Bson::String(text.to_string()),
        };
    };
    let typed = match ty {
        "int32" => t.parse::<i32>().ok().map(Bson::Int32),
        "int64" => t.parse::<i64>().ok().map(Bson::Int64),
        "double" => t.parse::<f64>().ok().map(Bson::Double),
        "decimal" => t.parse::<Decimal128>().ok().map(Bson::Decimal128),
        "bool" => t.parse::<bool>().ok().map(Bson::Boolean),
        "objectId" => ObjectId::parse_str(t).ok().map(Bson::ObjectId),
        "date" => parse_date(text),
        "object" => parse_json(t).filter(|b| matches!(b, Bson::Document(_))),
        "array" => parse_json(t).filter(|b| matches!(b, Bson::Array(_))),
        _ => None,
    };
    if let Some(b) = typed {
        return b;
    }
    if matches!(ty, "int32" | "int64" | "double" | "decimal")
        && let Some(f) = numeric_f64(t)
    {
        return Bson::Double(f);
    }
    Bson::String(text.to_string())
}

/// The keys that make a JSON object an extended-JSON value for `bson` (its `Bson` visitor reads
/// the whole object as that value as soon as it meets one of them).
const EXT_JSON_KEYS: [&str; 18] = [
    "$oid",
    "$symbol",
    "$numberInt",
    "$numberLong",
    "$numberDouble",
    "$numberDecimal",
    "$numberDecimalBytes",
    "$binary",
    "$uuid",
    "$code",
    "$scope",
    "$timestamp",
    "$regularExpression",
    "$dbPointer",
    "$date",
    "$minKey",
    "$maxKey",
    "$undefined",
];

/// A JSON value as written: an integer keeps its sign's range, which `serde_json` reports as
/// `i64` for every negative integer and `u64` for every other one.
enum Json {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl<'de> serde::Deserialize<'de> for Json {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Json;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_none<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<Json, E> {
                Ok(Json::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Json, E> {
                Ok(Json::Signed(n))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Json, E> {
                Ok(Json::Unsigned(n))
            }
            fn visit_f64<E>(self, f: f64) -> Result<Json, E> {
                Ok(Json::Float(f))
            }
            fn visit_str<E>(self, s: &str) -> Result<Json, E> {
                Ok(Json::Str(s.to_string()))
            }
            fn visit_string<E>(self, s: String) -> Result<Json, E> {
                Ok(Json::Str(s))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Json::Array(items))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut pairs = Vec::new();
                while let Some(pair) = map.next_entry::<String, Json>()? {
                    pairs.push(pair);
                }
                Ok(Json::Object(pairs))
            }
        }
        d.deserialize_any(V)
    }
}

impl Json {
    /// The value as BSON: a bare integer is an int32 when it fits (negative ones too), else an
    /// int64; a fraction or exponent is a double. An object with an extended-JSON key is read by
    /// `bson` itself, from the same text, so `$numberLong`, `$date` and the rest are unchanged.
    fn into_bson(self) -> Option<Bson> {
        Some(match self {
            Json::Null => Bson::Null,
            Json::Bool(b) => Bson::Boolean(b),
            Json::Signed(n) => i32::try_from(n).map_or(Bson::Int64(n), Bson::Int32),
            Json::Unsigned(n) => match i32::try_from(n) {
                Ok(n) => Bson::Int32(n),
                Err(_) => Bson::Int64(i64::try_from(n).ok()?),
            },
            Json::Float(f) => Bson::Double(f),
            Json::Str(s) => Bson::String(s),
            Json::Array(items) => Bson::Array(items.into_iter().map(Json::into_bson).collect::<Option<_>>()?),
            Json::Object(pairs) if pairs.iter().any(|(k, _)| EXT_JSON_KEYS.contains(&k.as_str())) => {
                let mut text = String::new();
                Json::Object(pairs).write(&mut text);
                serde_json::from_str::<Bson>(&text).ok()?
            }
            Json::Object(pairs) => {
                let mut doc = mongodb::bson::Document::new();
                for (k, v) in pairs {
                    doc.insert(k, v.into_bson()?);
                }
                Bson::Document(doc)
            }
        })
    }

    /// The value as JSON text again (numbers as `serde_json` writes them, which reads back exactly).
    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Signed(n) => out.push_str(&n.to_string()),
            Json::Unsigned(n) => out.push_str(&n.to_string()),
            Json::Float(f) => out.push_str(&serde_json::to_string(f).unwrap_or_else(|_| f.to_string())),
            Json::Str(s) => out.push_str(&json_str(s)),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&json_str(k));
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// A JSON cell's text (extended JSON allowed) as BSON, key order kept. Unlike
/// `serde_json::from_str::<Bson>`, a bare negative integer that fits is an int32, as our writer
/// wrote it: an int64 is always written as `{"$numberLong": …}`.
pub fn parse_json(text: &str) -> Option<Bson> {
    serde_json::from_str::<Json>(text).ok()?.into_bson()
}

/// The preview syntax (mongosh-like).
pub fn shell(v: &Bson) -> String {
    match v {
        Bson::Int64(n) => format!(r#"NumberLong("{n}")"#),
        Bson::Decimal128(d) => format!(r#"NumberDecimal("{d}")"#),
        Bson::ObjectId(o) => format!(r#"ObjectId("{}")"#, o.to_hex()),
        Bson::DateTime(d) => {
            let t = date_text(*d);
            if t.starts_with('{') {
                t
            } else {
                format!(r#"ISODate("{t}")"#)
            }
        }
        Bson::Document(doc) if doc.is_empty() => "{}".into(),
        Bson::Document(doc) => {
            let parts: Vec<String> = doc
                .iter()
                .map(|(k, val)| {
                    let key = if is_identifier(k) {
                        k.to_string()
                    } else {
                        json_str(k)
                    };
                    format!("{key}: {}", shell(val))
                })
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
        Bson::Array(items) => {
            let parts: Vec<String> = items.iter().map(shell).collect();
            format!("[{}]", parts.join(", "))
        }
        Bson::Double(f) => double_text(*f),
        Bson::Int32(_) | Bson::String(_) | Bson::Boolean(_) | Bson::Null => ext_json(v),
        other => canonical(other),
    }
}

fn is_identifier(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The column type name.
pub fn type_name(v: &Bson) -> &'static str {
    match v {
        Bson::Int32(_) => "int32",
        Bson::Int64(_) => "int64",
        Bson::Double(_) => "double",
        Bson::Decimal128(_) => "decimal",
        Bson::String(_) => "string",
        Bson::Boolean(_) => "bool",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid => "uuid",
        Bson::Binary(_) => "binary",
        Bson::Timestamp(_) => "timestamp",
        Bson::RegularExpression(_) => "regex",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::spec::BinarySubtype;
    use mongodb::bson::{
        Binary, DateTime, Decimal128, Document, Regex, Timestamp, doc, oid::ObjectId,
    };

    fn bytes(v: &Bson) -> Vec<u8> {
        let mut d = Document::new();
        d.insert("v", v.clone());
        d.to_vec().unwrap()
    }

    fn sample_values() -> Vec<Bson> {
        vec![
            Bson::Int32(42),
            Bson::Int32(-7),
            Bson::Int64(42),
            Bson::Int64(9007199254740993),
            Bson::Double(1.0),
            Bson::Double(0.1),
            Bson::Double(1e300),
            Bson::Double(1e21),
            Bson::Double(12345678.0),
            Bson::Double(f64::NAN),
            Bson::Double(f64::INFINITY),
            Bson::Double(f64::NEG_INFINITY),
            Bson::Double(-0.0),
            Bson::Decimal128("1.10".parse::<Decimal128>().unwrap()),
            Bson::String("héllo \"q\"".into()),
            Bson::String(String::new()),
            Bson::String("  padded ".into()),
            Bson::Boolean(true),
            Bson::Boolean(false),
            Bson::Null,
            Bson::ObjectId(ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap()),
            Bson::DateTime(DateTime::from_millis(1704164645678)),
            Bson::DateTime(DateTime::from_millis(-1000)),
            Bson::DateTime(DateTime::from_millis(253402300800000)),
            Bson::DateTime(DateTime::from_millis(-62135596800001)),
            Bson::Document(doc! {"z": 1, "a": Bson::Int64(2)}),
            Bson::Document(
                doc! {"a": 1, "b": Bson::Int64(2), "c": {"d": [1.0, "x", Bson::Double(f64::NAN)]}},
            ),
            Bson::Document(doc! {}),
            // Negative int32s stay int32 (a bare negative JSON integer isn't an int64), next to
            // an explicit int64.
            Bson::Document(doc! {
                "a": -7,
                "b": [-1, 2],
                "c": i32::MIN,
                "d": Bson::Int64(-5),
            }),
            Bson::Array(vec![]),
            Bson::Array(vec![
                Bson::Int32(1),
                Bson::Int64(1),
                Bson::Double(1.0),
                Bson::Null,
            ]),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![1, 2, 3, 255],
            }),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: vec![7; 16],
            }),
            Bson::Timestamp(Timestamp {
                time: 1700000000,
                increment: 7,
            }),
            Bson::RegularExpression(Regex {
                pattern: "^ab+c".try_into().unwrap(),
                options: "i".try_into().unwrap(),
            }),
            Bson::MinKey,
            Bson::MaxKey,
            Bson::Undefined,
            Bson::JavaScriptCode("return 1".into()),
            Bson::Symbol("sym".into()),
        ]
    }

    #[test]
    fn values_round_trip_every_type() {
        for v in sample_values() {
            let (text, code) = cell(Some(&v));
            let back = match &text {
                Some(t) => {
                    parse_cell(t, code, "f").unwrap_or_else(|e| panic!("{v:?} -> {t:?}: {e}"))
                }
                None => Bson::Null,
            };
            assert_eq!(bytes(&back), bytes(&v), "{v:?} via {text:?} {code:?}");
        }
    }

    #[test]
    fn nested_keeps_order_and_int64() {
        let v = Bson::Document(doc! {"z": 1, "a": Bson::Int64(2)});
        let (text, code) = cell(Some(&v));
        assert_eq!(code, CellType::Json);
        let back = parse_cell(&text.unwrap(), code, "f").unwrap();
        let d = back.as_document().unwrap();
        assert_eq!(d.keys().collect::<Vec<_>>(), ["z", "a"]);
        assert_eq!(d.get("a"), Some(&Bson::Int64(2)));
        assert_eq!(d.get("z"), Some(&Bson::Int32(1)));
    }

    #[test]
    fn nested_negative_integers_are_int32_when_they_fit() {
        let parsed = |t: &str| parse_cell(t, CellType::Json, "f").unwrap();
        assert_eq!(parsed(r#"{"a":-7}"#), Bson::Document(doc! { "a": Bson::Int32(-7) }));
        assert_eq!(
            parsed(r#"[-1, 2, -2147483648, -2147483649, 3000000000, -1.5, 1e2]"#),
            Bson::Array(vec![
                Bson::Int32(-1),
                Bson::Int32(2),
                Bson::Int32(i32::MIN),
                Bson::Int64(-2147483649),
                Bson::Int64(3000000000),
                Bson::Double(-1.5),
                Bson::Double(100.0),
            ])
        );
        // Explicit wrappers keep their type; keys keep their order; strings are left alone.
        let d = parsed(r#"{"z":{"$numberInt":"-3"},"a":{"$numberLong":"-3"},"s":"-3","n":{"x":[{"y":-4}]}}"#);
        let d = d.as_document().unwrap();
        assert_eq!(d.keys().collect::<Vec<_>>(), ["z", "a", "s", "n"]);
        assert_eq!(d.get("z"), Some(&Bson::Int32(-3)));
        assert_eq!(d.get("a"), Some(&Bson::Int64(-3)));
        assert_eq!(d.get("s"), Some(&Bson::String("-3".into())));
        assert_eq!(d.get("n"), Some(&Bson::Document(doc! { "x": [{ "y": -4 }] })));
        // A new value of an object or array column too.
        assert_eq!(parse_new(r#"{"a":-7}"#, Some("object")), Bson::Document(doc! { "a": -7 }));
        assert_eq!(parse_new("[-7]", Some("array")), Bson::Array(vec![Bson::Int32(-7)]));
        // Extended JSON wrappers are read as before.
        assert_eq!(
            parsed(r#"{"$timestamp":{"t":1700000000,"i":7}}"#),
            Bson::Timestamp(Timestamp { time: 1700000000, increment: 7 })
        );
        assert_eq!(parsed(r#"{"$date":{"$numberLong":"-1000"}}"#), Bson::DateTime(DateTime::from_millis(-1000)));
        assert!(parse_cell("{\"a\":", CellType::Json, "f").is_err());
    }

    fn c(v: Bson) -> (String, CellType) {
        let (t, k) = cell(Some(&v));
        (t.unwrap(), k)
    }

    #[test]
    fn cell_text_matches_the_table() {
        assert_eq!(cell(None), (None, CellType::Null));
        assert_eq!(cell(Some(&Bson::Null)), (None, CellType::Null));
        assert_eq!(c(Bson::Int32(42)), ("42".into(), CellType::Integer));
        assert_eq!(c(Bson::Int64(-9)), ("-9".into(), CellType::Long));
        assert_eq!(c(Bson::Double(1.0)), ("1.0".into(), CellType::Real));
        assert_eq!(c(Bson::Double(0.1)).0, "0.1");
        assert_eq!(c(Bson::Double(1e300)).0, "1e+300");
        assert_eq!(c(Bson::Double(-0.0)).0, "-0.0");
        assert_eq!(c(Bson::Double(12345678.0)).0, "12345678.0");
        assert_eq!(c(Bson::Double(f64::NAN)).0, "NaN");
        assert_eq!(c(Bson::Double(f64::INFINITY)).0, "Infinity");
        assert_eq!(c(Bson::Double(f64::NEG_INFINITY)).0, "-Infinity");
        assert_eq!(
            c(Bson::Decimal128("1.10".parse().unwrap())),
            ("1.10".into(), CellType::Decimal)
        );
        assert_eq!(
            c(Bson::String("a\"b".into())),
            ("a\"b".into(), CellType::Text)
        );
        assert_eq!(c(Bson::Boolean(true)), ("true".into(), CellType::Bool));
        assert_eq!(
            c(Bson::ObjectId(
                ObjectId::parse_str("65A1B2C3D4E5F60718293A4B").unwrap()
            )),
            ("65a1b2c3d4e5f60718293a4b".into(), CellType::ObjectId)
        );
        assert_eq!(
            c(Bson::DateTime(DateTime::from_millis(1704164645678))),
            ("2024-01-02T03:04:05.678Z".into(), CellType::Date)
        );
        assert_eq!(
            c(Bson::DateTime(DateTime::from_millis(-1000))).0,
            "1969-12-31T23:59:59Z"
        );
        assert_eq!(
            c(Bson::DateTime(DateTime::from_millis(253402300800000))),
            (
                r#"{"$date":{"$numberLong":"253402300800000"}}"#.into(),
                CellType::Date
            )
        );
        assert_eq!(
            c(Bson::Document(doc! {"z": 1, "a": Bson::Int64(2)})),
            (r#"{"z":1,"a":{"$numberLong":"2"}}"#.into(), CellType::Json)
        );
        assert_eq!(
            c(Bson::Array(vec![
                Bson::Double(1.0),
                Bson::Double(f64::NAN),
                Bson::Int32(2)
            ]))
            .0,
            r#"[1.0,{"$numberDouble":"NaN"},2]"#
        );
        assert_eq!(
            ext_json(&Bson::Document(doc! {
                "d": DateTime::from_millis(1704164645678),
                "o": ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap(),
                "m": "1.10".parse::<Decimal128>().unwrap(),
                "s": "x\ny",
                "n": Bson::Null,
            })),
            r#"{"d":{"$date":"2024-01-02T03:04:05.678Z"},"o":{"$oid":"65a1b2c3d4e5f60718293a4b"},"m":{"$numberDecimal":"1.10"},"s":"x\ny","n":null}"#
        );
        assert_eq!(
            c(Bson::Timestamp(Timestamp {
                time: 1700000000,
                increment: 7
            })),
            (
                r#"{"$timestamp":{"i":7,"t":1700000000}}"#.into(),
                CellType::Other
            )
        );
        assert_eq!(
            c(Bson::MinKey),
            (r#"{"$minKey":1}"#.into(), CellType::Other)
        );
    }

    #[test]
    fn parse_errors_name_the_type() {
        assert_eq!(
            parse_cell("abc", CellType::Integer, "age").unwrap_err(),
            r#"age: expected a whole number (int32), got "abc"."#
        );
        assert!(parse_cell("3000000000", CellType::Integer, "age").is_err());
        assert_eq!(
            parse_cell("3000000000", CellType::Long, "age").unwrap(),
            Bson::Int64(3000000000)
        );
        assert_eq!(
            parse_cell(" 42 ", CellType::Integer, "age").unwrap(),
            Bson::Int32(42)
        );
        assert_eq!(
            parse_cell("2026-01-02", CellType::Date, "d").unwrap(),
            Bson::DateTime(DateTime::parse_rfc3339_str("2026-01-02T00:00:00Z").unwrap())
        );
        assert_eq!(
            parse_cell("x", CellType::Date, "d").unwrap_err(),
            r#"d: expected a date (e.g. 2026-01-02T03:04:05Z), got "x"."#
        );
        assert_eq!(
            parse_cell("x", CellType::Real, "f").unwrap_err(),
            r#"f: expected a number, got "x"."#
        );
        assert_eq!(
            parse_cell("x", CellType::Decimal, "f").unwrap_err(),
            r#"f: expected a decimal number, got "x"."#
        );
        assert_eq!(
            parse_cell("True", CellType::Bool, "f").unwrap_err(),
            r#"f: expected true or false, got "True"."#
        );
        assert_eq!(
            parse_cell("12", CellType::ObjectId, "f").unwrap_err(),
            r#"f: expected an ObjectId (24 hex digits), got "12"."#
        );
        assert_eq!(
            parse_cell("{a: 1}", CellType::Json, "f").unwrap_err(),
            r#"f: expected a JSON value, got "{a: 1}"."#
        );
        assert_eq!(
            parse_cell("  keep ", CellType::Text, "f").unwrap(),
            Bson::String("  keep ".into())
        );
    }

    #[test]
    fn new_values_follow_the_column() {
        assert_eq!(parse_new("7", Some("int32")), Bson::Int32(7));
        assert_eq!(parse_new("1.5", Some("int32")), Bson::Double(1.5));
        assert_eq!(parse_new("abc", Some("int32")), Bson::String("abc".into()));
        assert_eq!(parse_new("nan", Some("int32")), Bson::String("nan".into()));
        assert_eq!(parse_new("7", Some("string")), Bson::String("7".into()));
        assert_eq!(parse_new("7", Some("int64")), Bson::Int64(7));
        assert_eq!(parse_new("7", Some("double")), Bson::Double(7.0));
        assert_eq!(parse_new("yes", Some("bool")), Bson::String("yes".into()));
        assert_eq!(parse_new("true", Some("bool")), Bson::Boolean(true));
        assert_eq!(
            parse_new("65a1b2c3d4e5f60718293a4b", Some("objectId")),
            Bson::ObjectId(ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap())
        );
        assert_eq!(
            parse_new("65a1", Some("objectId")),
            Bson::String("65a1".into())
        );
        assert!(matches!(
            parse_new("2026-01-02", Some("date")),
            Bson::DateTime(_)
        ));
        assert!(matches!(
            parse_new("{\"a\":1}", Some("object")),
            Bson::Document(_)
        ));
        assert_eq!(parse_new("[1]", Some("object")), Bson::String("[1]".into()));
        assert_eq!(parse_new("true", None), Bson::Boolean(true));
        assert_eq!(parse_new("12", None), Bson::Int32(12));
        assert_eq!(parse_new("3000000000", None), Bson::Int64(3000000000));
        assert_eq!(parse_new("1.5", None), Bson::Double(1.5));
        assert_eq!(parse_new("nan", None), Bson::String("nan".into()));
        assert_eq!(parse_new("hello", None), Bson::String("hello".into()));
        assert_eq!(parse_new("", None), Bson::String(String::new()));
    }

    #[test]
    fn shell_preview_forms() {
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        assert_eq!(
            shell(&Bson::ObjectId(oid)),
            r#"ObjectId("65a1b2c3d4e5f60718293a4b")"#
        );
        assert_eq!(
            shell(&Bson::Int64(9007199254740993)),
            r#"NumberLong("9007199254740993")"#
        );
        assert_eq!(shell(&Bson::Int32(5)), "5");
        assert_eq!(shell(&Bson::Double(1.0)), "1.0");
        assert_eq!(
            shell(&Bson::Decimal128("1.10".parse().unwrap())),
            r#"NumberDecimal("1.10")"#
        );
        assert_eq!(
            shell(&Bson::DateTime(DateTime::from_millis(1704164645678))),
            r#"ISODate("2024-01-02T03:04:05.678Z")"#
        );
        assert_eq!(shell(&Bson::String("a\"b".into())), r#""a\"b""#);
        assert_eq!(
            shell(&Bson::Document(doc! {"name": "Ann", "a b": 1})),
            r#"{ name: "Ann", "a b": 1 }"#
        );
        assert_eq!(shell(&Bson::Document(doc! {})), "{}");
        assert_eq!(
            shell(&Bson::Array(vec![Bson::Int32(1), Bson::Null])),
            "[1, null]"
        );
        assert_eq!(shell(&Bson::Array(vec![])), "[]");
        assert_eq!(shell(&Bson::MinKey), r#"{"$minKey":1}"#);
        assert_eq!(shell(&Bson::Boolean(false)), "false");
    }

    #[test]
    fn date_text_is_three_digit_milliseconds() {
        let d = |ms| c(Bson::DateTime(DateTime::from_millis(ms))).0;
        assert_eq!(d(1704164645500), "2024-01-02T03:04:05.500Z");
        assert_eq!(d(1704164645120), "2024-01-02T03:04:05.120Z");
        assert_eq!(d(1704164645678), "2024-01-02T03:04:05.678Z");
        assert_eq!(d(1704164645007), "2024-01-02T03:04:05.007Z");
        assert_eq!(d(-1500), "1969-12-31T23:59:58.500Z");
        assert_eq!(d(MIN_RFC3339_MS), "0001-01-01T00:00:00Z");
        assert_eq!(d(MAX_RFC3339_MS), "9999-12-31T23:59:59.999Z");
        assert_eq!(
            d(MIN_RFC3339_MS - 1),
            r#"{"$date":{"$numberLong":"-62135596800001"}}"#
        );
        let v = Bson::DateTime(DateTime::from_millis(1704164645500));
        assert_eq!(shell(&v), r#"ISODate("2024-01-02T03:04:05.500Z")"#);
        assert_eq!(ext_json(&v), r#"{"$date":"2024-01-02T03:04:05.500Z"}"#);
        for ms in [1704164645500, -1500, MIN_RFC3339_MS, MAX_RFC3339_MS] {
            let v = Bson::DateTime(DateTime::from_millis(ms));
            let (t, k) = cell(Some(&v));
            assert_eq!(bytes(&parse_cell(&t.unwrap(), k, "f").unwrap()), bytes(&v));
        }
    }

    #[test]
    fn shell_shows_non_finite_doubles_plainly() {
        assert_eq!(shell(&Bson::Double(f64::NAN)), "NaN");
        assert_eq!(shell(&Bson::Double(f64::NEG_INFINITY)), "-Infinity");
        assert_eq!(
            shell(&Bson::Document(doc! {"x": f64::INFINITY})),
            "{ x: Infinity }"
        );
        assert_eq!(
            shell(&Bson::Array(vec![
                Bson::Double(f64::NAN),
                Bson::Double(1.0)
            ])),
            "[NaN, 1.0]"
        );
    }

    #[test]
    fn type_names() {
        assert_eq!(type_name(&Bson::Int32(1)), "int32");
        assert_eq!(type_name(&Bson::Int64(1)), "int64");
        assert_eq!(type_name(&Bson::Double(1.0)), "double");
        assert_eq!(
            type_name(&Bson::Decimal128("1".parse().unwrap())),
            "decimal"
        );
        assert_eq!(type_name(&Bson::String("".into())), "string");
        assert_eq!(type_name(&Bson::Boolean(true)), "bool");
        assert_eq!(type_name(&Bson::ObjectId(ObjectId::new())), "objectId");
        assert_eq!(type_name(&Bson::DateTime(DateTime::now())), "date");
        assert_eq!(type_name(&Bson::Document(doc! {})), "object");
        assert_eq!(type_name(&Bson::Array(vec![])), "array");
        assert_eq!(
            type_name(&Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: vec![0; 16]
            })),
            "uuid"
        );
        assert_eq!(
            type_name(&Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![]
            })),
            "binary"
        );
        assert_eq!(
            type_name(&Bson::Timestamp(Timestamp {
                time: 1,
                increment: 1
            })),
            "timestamp"
        );
        assert_eq!(
            type_name(&Bson::RegularExpression(Regex {
                pattern: "a".try_into().unwrap(),
                options: "".try_into().unwrap()
            })),
            "regex"
        );
        assert_eq!(type_name(&Bson::MinKey), "other");
    }
}
