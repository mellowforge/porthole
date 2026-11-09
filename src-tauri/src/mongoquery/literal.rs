//! Literal values of the mongosh subset, built straight into BSON so
//! key order and int32-ness are what was written.

use mongodb::bson::oid::ObjectId;
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{Binary, Bson, DateTime, Decimal128, Document, Regex, Timestamp, Uuid};

use super::ParseError;
use super::lex::{self, Tok, Token};
use crate::model::ParamValues;

/// The largest magnitude a double holds exactly for every integer up to it.
const MAX_SAFE_INTEGER: u128 = 1 << 53;

/// The keys of canonical extended-JSON wrappers, read by `bson` itself.
const WRAPPER_KEYS: [&str; 16] = [
    "$oid",
    "$date",
    "$numberLong",
    "$numberInt",
    "$numberDouble",
    "$numberDecimal",
    "$binary",
    "$uuid",
    "$timestamp",
    "$regularExpression",
    "$minKey",
    "$maxKey",
    "$undefined",
    "$code",
    "$symbol",
    "$dbPointer",
];

const CONSTRUCTORS: [&str; 17] = [
    "NumberLong",
    "Long",
    "NumberInt",
    "Int32",
    "Double",
    "NumberDecimal",
    "Decimal128",
    "ISODate",
    "Date",
    "ObjectId",
    "UUID",
    "BinData",
    "RegExp",
    "Timestamp",
    "MinKey",
    "MaxKey",
    "Code",
];

/// One whole value; anything after it is an error.
pub fn parse_value(text: &str, params: &ParamValues) -> Result<Bson, ParseError> {
    let tokens = lex::tokenize(text)?;
    let mut parser = Parser::new(&tokens, params);
    let value = parser.value()?;
    match parser.peek() {
        Some(extra) => Err(ParseError::new(
            "Unexpected text after the value.",
            extra.start,
        )),
        None => Ok(value),
    }
}

/// A parameter's value: its text as one literal when the whole text parses, else the text as a
/// string. NULL (`None`) is `Null`.
pub fn param_value(text: Option<&str>) -> Bson {
    match text {
        None => Bson::Null,
        Some(t) => parse_value(t, &ParamValues::new()).unwrap_or_else(|_| Bson::String(t.into())),
    }
}

/// Reads values from tokens, skipping `Newline`s.
pub struct Parser<'a> {
    tokens: &'a [Token],
    params: &'a ParamValues,
    pos: usize,
}

/// A constructor's argument. A number keeps its text, so `NumberLong(9007199254740993)` is exact.
enum ArgValue {
    Num(String),
    Value(Bson),
}

struct Arg {
    value: ArgValue,
    at: usize,
}

impl<'a> Parser<'a> {
    pub fn new(tokens: &'a [Token], params: &'a ParamValues) -> Self {
        Self {
            tokens,
            params,
            pos: 0,
        }
    }

    pub(crate) fn peek(&self) -> Option<&'a Token> {
        self.tokens[self.pos..]
            .iter()
            .find(|t| t.kind != Tok::Newline)
    }

    pub(crate) fn bump(&mut self) -> Option<&'a Token> {
        while let Some(t) = self.tokens.get(self.pos) {
            self.pos += 1;
            if t.kind != Tok::Newline {
                return Some(t);
            }
        }
        None
    }

    /// The next token's offset, or the end of the last one.
    pub(crate) fn here(&self) -> usize {
        match self.peek() {
            Some(t) => t.start,
            None => self.tokens.last().map_or(0, |t| t.end),
        }
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError::new(message, self.here())
    }

    pub(crate) fn eat(&mut self, kind: &Tok) -> bool {
        let found = self.peek().is_some_and(|t| &t.kind == kind);
        if found {
            self.bump();
        }
        found
    }

    pub fn value(&mut self) -> Result<Bson, ParseError> {
        let Some(tok) = self.peek() else {
            return Err(self.error("Expected a value."));
        };
        let at = tok.start;
        let value = match &tok.kind {
            Tok::LBrace => {
                self.bump();
                return self.object(at);
            }
            Tok::LBracket => {
                self.bump();
                return self.array();
            }
            Tok::Ident(name) => {
                self.bump();
                return self.word(name, at);
            }
            Tok::Str(s) => Bson::String(s.clone()),
            Tok::Num(n) => number(n, at)?,
            Tok::Regex { pattern, flags } => regex(pattern, flags, at)?,
            Tok::Param(name) => match self.params.get(name) {
                None => return Err(ParseError::new(format!("No value for :{name}"), at)),
                Some(text) => param_value(text.as_deref()),
            },
            _ => return Err(ParseError::new("Expected a value.", at)),
        };
        self.bump();
        Ok(value)
    }

    /// A value that must be a document (not an ext-JSON wrapper).
    pub fn document(&mut self) -> Result<Document, ParseError> {
        let at = self.here();
        let expected = || ParseError::new("Expected a document.", at);
        if !self
            .peek()
            .is_some_and(|t| matches!(t.kind, Tok::LBrace | Tok::Param(_)))
        {
            return Err(expected());
        }
        match self.value()? {
            Bson::Document(d) => Ok(d),
            _ => Err(expected()),
        }
    }

    /// After `{`.
    fn object(&mut self, open: usize) -> Result<Bson, ParseError> {
        let mut doc = Document::new();
        let mut key_at = Vec::new();
        loop {
            let Some(tok) = self.peek() else {
                return Err(self.error("Expected a key or }."));
            };
            let key = match &tok.kind {
                Tok::RBrace => {
                    self.bump();
                    break;
                }
                Tok::Ident(k) | Tok::Str(k) => k,
                _ => return Err(ParseError::new("Expected a key or }.", tok.start)),
            };
            if doc.contains_key(key) {
                return Err(ParseError::new(
                    format!("Duplicate key {}.", quoted(key)),
                    tok.start,
                ));
            }
            self.bump();
            if !self.eat(&Tok::Colon) {
                return Err(self.error("Expected : after the key."));
            }
            let value = self.value()?;
            doc.insert(key.clone(), value);
            key_at.push(tok.start);
            if self.eat(&Tok::Comma) {
                continue;
            }
            if self.eat(&Tok::RBrace) {
                break;
            }
            return Err(self.error("Expected , or }."));
        }
        unwrap_ext_json(doc, &key_at, open)
    }

    /// After `[`.
    fn array(&mut self) -> Result<Bson, ParseError> {
        let mut items = Vec::new();
        loop {
            if self.eat(&Tok::RBracket) {
                return Ok(Bson::Array(items));
            }
            items.push(self.value()?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            if self.eat(&Tok::RBracket) {
                return Ok(Bson::Array(items));
            }
            return Err(self.error("Expected , or ]."));
        }
    }

    /// A name in a value position: a keyword or a constructor call.
    fn word(&mut self, name: &str, at: usize) -> Result<Bson, ParseError> {
        if name == "new" {
            let ctor_at = self.here();
            return match self.bump() {
                Some(Token {
                    kind: Tok::Ident(ctor),
                    start,
                    ..
                }) => {
                    if !self.eat(&Tok::LParen) {
                        return Err(self.error(format!("Expected ( after {ctor}.")));
                    }
                    self.call(ctor, *start, true)
                }
                _ => Err(ParseError::new(
                    "Expected a constructor after new.",
                    ctor_at,
                )),
            };
        }
        if self.eat(&Tok::LParen) {
            return self.call(name, at, false);
        }
        Ok(match name {
            "true" => Bson::Boolean(true),
            "false" => Bson::Boolean(false),
            "null" | "undefined" => Bson::Null,
            "NaN" => Bson::Double(f64::NAN),
            "Infinity" => Bson::Double(f64::INFINITY),
            _ => {
                return Err(ParseError::new(
                    format!("Unknown name {name}. Put text in quotes."),
                    at,
                ));
            }
        })
    }

    /// After `Name(`.
    fn call(&mut self, name: &str, at: usize, with_new: bool) -> Result<Bson, ParseError> {
        if !CONSTRUCTORS.contains(&name) {
            return Err(ParseError::new(format!("Unknown constructor {name}."), at));
        }
        if name == "Date" && !with_new {
            return Err(ParseError::new(
                "Date() makes a string in mongosh. Use new Date() or ISODate().",
                at,
            ));
        }
        let args = self.args()?;
        construct(name, at, &args)
    }

    /// After `(`, through `)`.
    fn args(&mut self) -> Result<Vec<Arg>, ParseError> {
        let mut args = Vec::new();
        loop {
            if self.eat(&Tok::RParen) {
                return Ok(args);
            }
            let at = self.here();
            let value = match self.peek() {
                Some(Token {
                    kind: Tok::Num(n), ..
                }) => {
                    self.bump();
                    ArgValue::Num(n.clone())
                }
                _ => ArgValue::Value(self.value()?),
            };
            args.push(Arg { value, at });
            if self.eat(&Tok::Comma) {
                continue;
            }
            if self.eat(&Tok::RParen) {
                return Ok(args);
            }
            return Err(self.error("Expected , or )."));
        }
    }
}

fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

/// A whole number written as decimal or `0x` hex, with an optional `-`.
fn int_text(text: &str) -> Option<i128> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let magnitude = match body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
            u128::from_str_radix(hex, 16).ok()?
        }
        Some(_) => return None,
        None if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit()) => {
            body.parse::<u128>().ok()?
        }
        None => return None,
    };
    let magnitude = i128::try_from(magnitude).ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// A number token: int32 when it fits, a double up to 2^53, an error beyond; a fraction or an
/// exponent is a double.
fn number(raw: &str, at: usize) -> Result<Bson, ParseError> {
    let negative = raw.starts_with('-');
    if raw.trim_start_matches('-') == "Infinity" {
        return Ok(Bson::Double(if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        }));
    }
    let hex = raw.contains(['x', 'X']);
    if !hex && raw.contains(['.', 'e', 'E']) {
        return raw
            .parse::<f64>()
            .map(Bson::Double)
            .map_err(|_| ParseError::new("Invalid number.", at));
    }
    let inexact = || {
        ParseError::new(
            format!(r#"{raw} can't be stored exactly. Use NumberLong("{raw}")."#),
            at,
        )
    };
    let n = int_text(raw).ok_or_else(inexact)?;
    if n == 0 && negative {
        return Ok(Bson::Double(-0.0));
    }
    if let Ok(n) = i32::try_from(n) {
        return Ok(Bson::Int32(n));
    }
    if n.unsigned_abs() <= MAX_SAFE_INTEGER {
        Ok(Bson::Double(n as f64))
    } else {
        Err(inexact())
    }
}

/// `/pattern/flags` or `RegExp(p, f)`: the flags BSON knows, each once, stored sorted.
fn regex(pattern: &str, flags: &str, at: usize) -> Result<Bson, ParseError> {
    let mut sorted: Vec<char> = flags.chars().collect();
    sorted.sort_unstable();
    let valid =
        sorted.iter().all(|c| "ilmsux".contains(*c)) && sorted.windows(2).all(|w| w[0] != w[1]);
    if !valid {
        return Err(ParseError::new(
            format!(r#"Invalid regex flags "{flags}". Use any of i, m, s, u, x and l."#),
            at,
        ));
    }
    let options: String = sorted.into_iter().collect();
    let nul = || ParseError::new("A regex can't contain a NUL character.", at);
    Ok(Bson::RegularExpression(Regex {
        pattern: pattern.try_into().map_err(|_| nul())?,
        options: options.try_into().map_err(|_| nul())?,
    }))
}

/// A document with a canonical wrapper key becomes that value, read by `bson`.
fn unwrap_ext_json(doc: Document, key_at: &[usize], open: usize) -> Result<Bson, ParseError> {
    let Some(key) = doc
        .keys()
        .find(|k| WRAPPER_KEYS.contains(&k.as_str()))
        .cloned()
    else {
        return Ok(Bson::Document(doc));
    };
    // Code with scope is the one two-key wrapper.
    let belongs = |k: &str| k == key || (key == "$code" && k == "$scope");
    if let Some((_, at)) = doc.keys().zip(key_at).find(|(k, _)| !belongs(k)) {
        return Err(ParseError::new(
            format!(r#"{{"{key}": …}} can't have other keys."#),
            *at,
        ));
    }
    // `$timestamp`'s `t`/`i` and `$date`'s epoch ms must be JSON integers; elsewhere (a
    // `$scope`) a double stays a double.
    let whole_as_int = key == "$timestamp" || key == "$date";
    let json = serde_json::Value::Object(
        doc.into_iter()
            .map(|(k, v)| (k, json_value(v, whole_as_int)))
            .collect(),
    );
    Bson::try_from(json).map_err(|e| {
        ParseError::new(
            format!(
                r#"Invalid {{"{key}": …}}: {}."#,
                e.to_string().trim_end_matches('.')
            ),
            open,
        )
    })
}

/// A parsed value back as the JSON `bson` expects inside a wrapper, inner wrappers as canonical
/// ext JSON again. A whole double is a JSON integer when `whole_as_int`, else `$numberDouble`.
fn json_value(v: Bson, whole_as_int: bool) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Bson::Null => Value::Null,
        Bson::Boolean(b) => Value::Bool(b),
        Bson::String(s) => Value::String(s),
        Bson::Int32(n) => n.into(),
        Bson::Double(f)
            if whole_as_int
                && f.fract() == 0.0
                && f.abs() <= MAX_SAFE_INTEGER as f64
                && !(f == 0.0 && f.is_sign_negative()) =>
        {
            (f as i64).into()
        }
        Bson::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| json_value(v, whole_as_int))
                .collect(),
        ),
        Bson::Document(d) => Value::Object(
            d.into_iter()
                .map(|(k, v)| (k, json_value(v, whole_as_int)))
                .collect(),
        ),
        other => other.into_canonical_extjson(),
    }
}

/// `YYYY-MM-DD`, optionally `T` (or a space) and `HH:MM[:SS[.fff]]`, optionally `Z` or `±HH[:MM]`.
/// A date alone is midnight UTC; a time without a zone is UTC.
fn iso_date(text: &str) -> Option<DateTime> {
    let s = text.trim();
    if !s.is_ascii() || s.len() < 10 {
        return None;
    }
    let (day, rest) = s.split_at(10);
    let day_ok = day.bytes().enumerate().all(|(i, b)| {
        if i == 4 || i == 7 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        }
    });
    if !day_ok {
        return None;
    }
    if rest.is_empty() {
        return DateTime::parse_rfc3339_str(format!("{day}T00:00:00Z")).ok();
    }
    let time = rest.strip_prefix(['T', 't', ' '])?;
    let (clock, zone) = time.split_at(time.find(['Z', 'z', '+', '-']).unwrap_or(time.len()));
    let seconds = if clock.len() == 5 { ":00" } else { "" };
    let zone = match zone {
        "" | "Z" | "z" => "Z".to_string(),
        z if z.len() == 3 => format!("{z}:00"),
        z if z.len() == 5 && !z.contains(':') => format!("{}:{}", &z[..3], &z[3..]),
        z => z.to_string(),
    };
    DateTime::parse_rfc3339_str(format!("{day}T{clock}{seconds}{zone}")).ok()
}

fn bson_int(v: &Bson) -> Option<i128> {
    match v {
        Bson::Int32(n) => Some((*n).into()),
        Bson::Int64(n) => Some((*n).into()),
        Bson::Double(f) if f.fract() == 0.0 && f.abs() < 1e30 => Some(*f as i128),
        _ => None,
    }
}

impl Arg {
    fn int(&self) -> Option<i128> {
        match &self.value {
            ArgValue::Num(n) => int_text(n),
            ArgValue::Value(Bson::String(s)) => int_text(s.trim()),
            ArgValue::Value(v) => bson_int(v),
        }
    }

    fn float(&self) -> Option<f64> {
        match &self.value {
            ArgValue::Num(n) => match number(n, self.at) {
                Ok(Bson::Int32(i)) => Some(i.into()),
                Ok(Bson::Double(f)) => Some(f),
                _ => n.parse::<f64>().ok(),
            },
            ArgValue::Value(Bson::Int32(i)) => Some((*i).into()),
            ArgValue::Value(Bson::Int64(i)) => Some(*i as f64),
            ArgValue::Value(Bson::Double(f)) => Some(*f),
            ArgValue::Value(Bson::String(s)) => s.trim().parse::<f64>().ok(),
            ArgValue::Value(_) => None,
        }
    }

    fn text(&self) -> Option<&str> {
        match &self.value {
            ArgValue::Value(Bson::String(s)) => Some(s),
            _ => None,
        }
    }

    /// A decimal's digits: a string, or a number as written.
    fn decimal_text(&self) -> Option<String> {
        match &self.value {
            ArgValue::Num(n) => Some(n.clone()),
            ArgValue::Value(Bson::String(s)) => Some(s.trim().to_string()),
            ArgValue::Value(Bson::Int32(n)) => Some(n.to_string()),
            ArgValue::Value(Bson::Int64(n)) => Some(n.to_string()),
            ArgValue::Value(_) => None,
        }
    }
}

fn usage(name: &str) -> String {
    match name {
        "NumberLong" | "Long" => {
            format!(r#"{name} needs one whole number in the int64 range, as in {name}("5")."#)
        }
        "NumberInt" | "Int32" => {
            format!("{name} needs one whole number in the int32 range, as in {name}(5).")
        }
        "Double" => "Double needs one number, as in Double(5).".into(),
        "NumberDecimal" | "Decimal128" => {
            format!(r#"{name} needs one decimal string, as in {name}("1.10")."#)
        }
        "ISODate" | "Date" => {
            let call = if name == "Date" { "new Date" } else { name };
            format!(
                r#"{call} needs a date string, as in {call}("2026-01-01T00:00:00Z"), or nothing for now."#
            )
        }
        "ObjectId" => r#"ObjectId needs 24 hex digits, as in ObjectId("650000000000000000000001"), or nothing for a new one."#.into(),
        "UUID" => r#"UUID needs a UUID string, as in UUID("6c1e5d2a-9b3f-4c8e-8a7d-1f2e3d4c5b6a"), or nothing for a new one."#.into(),
        "BinData" => r#"BinData needs a subtype from 0 to 255 and a base64 string, as in BinData(0, "AQID")."#.into(),
        "RegExp" => r#"RegExp needs a pattern string and optional flags, as in new RegExp("^a", "i")."#.into(),
        "Timestamp" => "Timestamp needs two whole numbers from 0 to 4294967295, as in Timestamp(1, 2) or Timestamp({ t: 1, i: 2 }).".into(),
        "Code" => r#"Code needs one string, as in Code("return 1")."#.into(),
        _ => format!("{name} takes no arguments."),
    }
}

/// A known constructor applied to its arguments.
fn construct(name: &str, at: usize, args: &[Arg]) -> Result<Bson, ParseError> {
    let u32_of = |n: Option<i128>| n.and_then(|n| u32::try_from(n).ok());
    let value = match (name, args) {
        ("NumberLong" | "Long", [a]) => {
            a.int().and_then(|n| i64::try_from(n).ok()).map(Bson::Int64)
        }
        ("NumberInt" | "Int32", [a]) => {
            a.int().and_then(|n| i32::try_from(n).ok()).map(Bson::Int32)
        }
        ("Double", [a]) => a.float().map(Bson::Double),
        ("NumberDecimal" | "Decimal128", [a]) => a
            .decimal_text()
            .and_then(|t| t.parse::<Decimal128>().ok())
            .map(Bson::Decimal128),
        ("ISODate" | "Date", []) => Some(Bson::DateTime(DateTime::now())),
        ("ISODate" | "Date", [a]) => match a.text() {
            Some(t) => {
                return iso_date(t).map(Bson::DateTime).ok_or_else(|| {
                    ParseError::new(
                        format!(r#"Invalid date "{t}". Write it like 2026-01-01T00:00:00Z."#),
                        a.at,
                    )
                });
            }
            None => None,
        },
        ("ObjectId", []) => Some(Bson::ObjectId(ObjectId::new())),
        ("ObjectId", [a]) => a
            .text()
            .and_then(|t| ObjectId::parse_str(t).ok())
            .map(Bson::ObjectId),
        ("UUID", []) => Some(Bson::Binary(Binary::from_uuid(Uuid::new()))),
        ("UUID", [a]) => a
            .text()
            .and_then(|t| Uuid::parse_str(t).ok())
            .map(|u| Bson::Binary(Binary::from_uuid(u))),
        ("BinData", [subtype, data]) => {
            match (
                subtype.int().and_then(|n| u8::try_from(n).ok()),
                data.text(),
            ) {
                (Some(s), Some(b64)) => Binary::from_base64(b64, BinarySubtype::from(s))
                    .ok()
                    .map(Bson::Binary),
                _ => None,
            }
        }
        ("RegExp", [pattern]) => match pattern.text() {
            Some(p) => return regex(p, "", pattern.at),
            None => None,
        },
        ("RegExp", [pattern, flags]) => match (pattern.text(), flags.text()) {
            (Some(p), Some(f)) => return regex(p, f, flags.at),
            _ => None,
        },
        ("Timestamp", [t, i]) => match (u32_of(t.int()), u32_of(i.int())) {
            (Some(time), Some(increment)) => Some(Bson::Timestamp(Timestamp { time, increment })),
            _ => None,
        },
        ("Timestamp", [a]) => match &a.value {
            ArgValue::Value(Bson::Document(d)) if d.len() == 2 => {
                let part = |k: &str| u32_of(d.get(k).and_then(bson_int));
                match (part("t"), part("i")) {
                    (Some(time), Some(increment)) => {
                        Some(Bson::Timestamp(Timestamp { time, increment }))
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        ("MinKey", []) => Some(Bson::MinKey),
        ("MaxKey", []) => Some(Bson::MaxKey),
        ("Code", [a]) => a.text().map(|t| Bson::JavaScriptCode(t.into())),
        _ => None,
    };
    value.ok_or_else(|| ParseError::new(usage(name), args.first().map_or(at, |a| a.at)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::mongodb::values;
    use mongodb::bson::spec::BinarySubtype;
    use mongodb::bson::{
        Binary, DateTime, Decimal128, JavaScriptCodeWithScope, Regex, Timestamp, doc, oid::ObjectId,
    };

    fn none() -> ParamValues {
        ParamValues::new()
    }

    fn ok(text: &str) -> Bson {
        parse_value(text, &none()).unwrap_or_else(|e| panic!("{text}: {e:?}"))
    }

    fn err(text: &str) -> String {
        match parse_value(text, &none()) {
            Ok(v) => panic!("{text} parsed as {v:?}"),
            Err(e) => e.message,
        }
    }

    /// Equal values, with doubles compared by bit pattern (so `-0.0` isn't `0.0`), NaN equal
    /// to NaN, and document keys compared in order.
    fn same(a: &Bson, b: &Bson) -> bool {
        match (a, b) {
            (Bson::Double(x), Bson::Double(y)) => {
                (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits()
            }
            (Bson::Document(x), Bson::Document(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .zip(y.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && same(va, vb))
            }
            (Bson::Array(x), Bson::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
            }
            _ => a == b,
        }
    }

    fn assert_same(text: &str, want: Bson) {
        let got = ok(text);
        assert!(same(&got, &want), "{text}: got {got:?}, want {want:?}");
    }

    fn date(ms: i64) -> Bson {
        Bson::DateTime(DateTime::from_millis(ms))
    }

    fn near_now(v: Bson) {
        let Bson::DateTime(d) = v else {
            panic!("not a date: {v:?}")
        };
        let diff = (d.timestamp_millis() - DateTime::now().timestamp_millis()).abs();
        assert!(diff < 5_000, "{d:?} isn't now");
    }

    const JAN_1_2026: i64 = 1_767_225_600_000;

    #[test]
    fn literal_table() {
        // Integers in int32 range, decimal or hex, optional sign.
        assert_same("5", Bson::Int32(5));
        assert_same("-5", Bson::Int32(-5));
        assert_same("0x10", Bson::Int32(16));
        assert_same("-0x10", Bson::Int32(-16));
        assert_same("2147483647", Bson::Int32(i32::MAX));
        assert_same("-2147483648", Bson::Int32(i32::MIN));
        // Outside int32, up to 2^53: Double.
        assert_same("2147483648", Bson::Double(2147483648.0));
        assert_same("-2147483649", Bson::Double(-2147483649.0));
        assert_same("9007199254740992", Bson::Double(9007199254740992.0));
        // Beyond 2^53: an error.
        assert_eq!(
            err("9007199254740993"),
            r#"9007199254740993 can't be stored exactly. Use NumberLong("9007199254740993")."#
        );
        assert_eq!(
            err("-9007199254740993"),
            r#"-9007199254740993 can't be stored exactly. Use NumberLong("-9007199254740993")."#
        );
        // A fraction or an exponent: Double.
        assert_same("5.0", Bson::Double(5.0));
        assert_same("5.5", Bson::Double(5.5));
        assert_same("1e3", Bson::Double(1000.0));
        assert_same("1e21", Bson::Double(1e21));
        // NaN, Infinity, -Infinity.
        assert!(matches!(ok("NaN"), Bson::Double(f) if f.is_nan()));
        assert_same("Infinity", Bson::Double(f64::INFINITY));
        assert_same("-Infinity", Bson::Double(f64::NEG_INFINITY));
        // Strings with JS escapes, either quote.
        assert_same(r#""a\nb\t\\\"""#, Bson::String("a\nb\t\\\"".into()));
        assert_same(r"'it\'s é'", Bson::String("it's é".into()));
        // Keywords.
        assert_same("true", Bson::Boolean(true));
        assert_same("false", Bson::Boolean(false));
        assert_same("null", Bson::Null);
        assert_same("undefined", Bson::Null);
        // Int64.
        assert_same(r#"NumberLong("5")"#, Bson::Int64(5));
        assert_same("NumberLong(5)", Bson::Int64(5));
        assert_same("NumberLong(-5)", Bson::Int64(-5));
        assert_same(r#"Long("9223372036854775807")"#, Bson::Int64(i64::MAX));
        assert_same("Long(9007199254740993)", Bson::Int64(9007199254740993));
        assert_same(r#"new NumberLong("-1")"#, Bson::Int64(-1));
        // Int32.
        assert_same("NumberInt(5)", Bson::Int32(5));
        assert_same(r#"NumberInt("-5")"#, Bson::Int32(-5));
        assert_same("Int32(5)", Bson::Int32(5));
        // Double.
        assert_same("Double(5)", Bson::Double(5.0));
        assert_same("Double(2.5)", Bson::Double(2.5));
        // Decimal128, scale kept.
        let dec = ok(r#"NumberDecimal("1.10")"#);
        assert_eq!(dec, Bson::Decimal128("1.10".parse::<Decimal128>().unwrap()));
        assert_eq!(values::shell(&dec), r#"NumberDecimal("1.10")"#);
        assert_same(
            r#"Decimal128("1.5")"#,
            Bson::Decimal128("1.5".parse::<Decimal128>().unwrap()),
        );
        // Dates: date-only is midnight UTC, no zone is UTC.
        assert_same(r#"ISODate("2026-01-01T00:00:00Z")"#, date(JAN_1_2026));
        assert_same(r#"ISODate("2026-01-01")"#, date(JAN_1_2026));
        assert_same(r#"ISODate("2026-01-01T00:00:00")"#, date(JAN_1_2026));
        assert_same(r#"ISODate("2026-01-01T00:00")"#, date(JAN_1_2026));
        assert_same(r#"ISODate("2026-01-01T02:00:00+02:00")"#, date(JAN_1_2026));
        assert_same(
            r#"ISODate("2026-01-01T00:00:00.250Z")"#,
            date(JAN_1_2026 + 250),
        );
        assert_same(r#"new Date("2026-01-01T00:00:00Z")"#, date(JAN_1_2026));
        near_now(ok("ISODate()"));
        near_now(ok("new Date()"));
        assert_eq!(
            err("Date()"),
            "Date() makes a string in mongosh. Use new Date() or ISODate()."
        );
        // ObjectId.
        let oid = ObjectId::parse_str("650000000000000000000001").unwrap();
        assert_same(
            r#"ObjectId("650000000000000000000001")"#,
            Bson::ObjectId(oid),
        );
        assert_same(
            r#"new ObjectId("650000000000000000000001")"#,
            Bson::ObjectId(oid),
        );
        assert!(matches!(ok("ObjectId()"), Bson::ObjectId(_)));
        // UUID: Binary subtype 4.
        let uuid = ok(r#"UUID("6c1e5d2a-9b3f-4c8e-8a7d-1f2e3d4c5b6a")"#);
        assert_eq!(
            uuid,
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: vec![
                    0x6c, 0x1e, 0x5d, 0x2a, 0x9b, 0x3f, 0x4c, 0x8e, 0x8a, 0x7d, 0x1f, 0x2e, 0x3d,
                    0x4c, 0x5b, 0x6a
                ],
            })
        );
        assert!(matches!(ok("UUID()"),
            Bson::Binary(Binary { subtype: BinarySubtype::Uuid, bytes }) if bytes.len() == 16));
        // BinData.
        assert_same(
            r#"BinData(0, "AQID")"#,
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![1, 2, 3],
            }),
        );
        // Regex, flags sorted.
        let regex = |pattern: &str, options: &str| {
            Bson::RegularExpression(Regex {
                pattern: pattern.try_into().unwrap(),
                options: options.try_into().unwrap(),
            })
        };
        assert_same("/re/i", regex("re", "i"));
        assert_same("/a/mi", regex("a", "im"));
        assert_same(r#"new RegExp("a+b", "m")"#, regex("a+b", "m"));
        assert_same(r#"RegExp("a+b")"#, regex("a+b", ""));
        // Timestamp.
        let ts = Bson::Timestamp(Timestamp {
            time: 1,
            increment: 2,
        });
        assert_same("Timestamp(1, 2)", ts.clone());
        assert_same("Timestamp({ t: 1, i: 2 })", ts);
        // MinKey, MaxKey, Code.
        assert_same("MinKey()", Bson::MinKey);
        assert_same("MaxKey()", Bson::MaxKey);
        assert_same(r#"Code("x")"#, Bson::JavaScriptCode("x".into()));
        // Arrays and documents.
        assert_same(
            r#"[1, 2.5, "s"]"#,
            Bson::Array(vec![
                Bson::Int32(1),
                Bson::Double(2.5),
                Bson::String("s".into()),
            ]),
        );
        assert_same(
            r#"{ z: 1, "a b": 2, 'c': 3, $d: 4 }"#,
            Bson::Document(doc! {"z": 1, "a b": 2, "c": 3, "$d": 4}),
        );
        assert_eq!(err("{ a: 1, a: 2 }"), r#"Duplicate key "a"."#);
        // Canonical ext-JSON wrappers.
        assert_same(
            r#"{"$oid": "650000000000000000000001"}"#,
            Bson::ObjectId(oid),
        );
        assert_same(r#"{ $numberLong: "5" }"#, Bson::Int64(5));
        assert_same(
            r#"{"$date":{"$numberLong":"1767225600000"}}"#,
            date(JAN_1_2026),
        );
        // Relaxed epoch ms: the number parses as a Double, and the wrapper needs an integer.
        assert_same(r#"{"$date": 1767225600000}"#, date(JAN_1_2026));
        assert_eq!(
            err(r#"{"$oid": "650000000000000000000001", "x": 1}"#),
            r#"{"$oid": …} can't have other keys."#
        );
        // Unknown names.
        assert_eq!(err("Foo(1)"), "Unknown constructor Foo.");
        assert_eq!(err("new Foo()"), "Unknown constructor Foo.");
    }

    #[test]
    fn numbers_follow_mongosh() {
        assert_same("-5", Bson::Int32(-5));
        assert_same("2147483648", Bson::Double(2147483648.0));
        assert_eq!(
            err("9007199254740993"),
            r#"9007199254740993 can't be stored exactly. Use NumberLong("9007199254740993")."#
        );
        assert_same("0x10", Bson::Int32(16));
        assert_same("1e3", Bson::Double(1000.0));
        assert_same("-0", Bson::Double(-0.0));
        assert_same("-0.0", Bson::Double(-0.0));
        assert_same(".5", Bson::Double(0.5));
        assert_same("3000000000.0", Bson::Double(3000000000.0));
    }

    #[test]
    fn document_keeps_key_order() {
        let Bson::Document(d) = ok("{ z: 1, a: -1, $set: { b: 2 } }") else {
            panic!("not a document")
        };
        assert_eq!(d.keys().collect::<Vec<_>>(), ["z", "a", "$set"]);
        assert_eq!(d.get("a"), Some(&Bson::Int32(-1)));
        assert_eq!(d.get("$set"), Some(&Bson::Document(doc! {"b": 2})));
    }

    #[test]
    fn duplicate_keys_are_an_error() {
        let e = parse_value("{ a: 1, b: { c: 1 }, a: 2 }", &none()).unwrap_err();
        assert_eq!(e, ParseError::new(r#"Duplicate key "a"."#, 21));
        assert_eq!(err(r#"{ "a": 1, a: 2 }"#), r#"Duplicate key "a"."#);
    }

    #[test]
    fn wrapper_with_extra_keys_is_an_error() {
        let e = parse_value(r#"{ x: 1, $oid: "650000000000000000000001" }"#, &none()).unwrap_err();
        assert_eq!(
            e,
            ParseError::new(r#"{"$oid": …} can't have other keys."#, 2)
        );
        assert_eq!(
            err(r#"{ $date: "2026-01-01T00:00:00Z", $lt: 1 }"#),
            r#"{"$date": …} can't have other keys."#
        );
        // Query operators and the `$regex` form aren't wrappers.
        assert_same(
            r#"{ $regex: "^a", $options: "i" }"#,
            Bson::Document(doc! {"$regex": "^a", "$options": "i"}),
        );
        // A wrapper with a bad value names it.
        assert!(err(r#"{ $oid: "zz" }"#).starts_with(r#"Invalid {"$oid": …}: "#));
    }

    #[test]
    fn trailing_commas_are_allowed() {
        assert_same(
            "{ a: [1, 2,], b: { c: 1, }, }",
            Bson::Document(doc! {"a": [1, 2], "b": {"c": 1}}),
        );
        assert_same(
            "Timestamp(1, 2,)",
            Bson::Timestamp(Timestamp {
                time: 1,
                increment: 2,
            }),
        );
        assert_eq!(err("[1,,2]"), "Expected a value.");
    }

    #[test]
    fn multi_line_values() {
        assert_same(
            "{\n  a: 1, // one\n  b: [\n    2,\n  ]\n}\n",
            Bson::Document(doc! {"a": 1, "b": [2]}),
        );
    }

    /// One value of every kind `values::cell` handles, as the grid holds them.
    fn cell_values() -> Vec<Bson> {
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let db_pointer = Bson::try_from(serde_json::json!({
            "$dbPointer": {"$ref": "users", "$id": {"$oid": "65a1b2c3d4e5f60718293a4b"}}
        }))
        .unwrap();
        let scalars = vec![
            Bson::Null,
            Bson::Int32(42),
            Bson::Int32(-7),
            Bson::Int32(i32::MIN),
            Bson::Int64(42),
            Bson::Int64(-5),
            Bson::Int64(i64::MIN),
            Bson::Int64(9007199254740993),
            Bson::Double(1.0),
            Bson::Double(5.0),
            Bson::Double(0.1),
            Bson::Double(-2.5),
            Bson::Double(1e300),
            Bson::Double(1e21),
            Bson::Double(1e-7),
            Bson::Double(3000000000.0),
            Bson::Double(12345678.0),
            Bson::Double(f64::MAX),
            Bson::Double(f64::MIN_POSITIVE),
            Bson::Double(f64::NAN),
            Bson::Double(f64::INFINITY),
            Bson::Double(f64::NEG_INFINITY),
            Bson::Double(-0.0),
            Bson::Decimal128("1.10".parse().unwrap()),
            Bson::Decimal128("-0.00".parse().unwrap()),
            Bson::Decimal128("1E+3".parse().unwrap()),
            Bson::Decimal128("NaN".parse().unwrap()),
            Bson::String("héllo \"q\" 'single' \\ /".into()),
            Bson::String("\n\t\r\u{8}\u{c}\u{1}\u{7f}\u{2028}😀".into()),
            Bson::String(String::new()),
            Bson::String("  padded ".into()),
            Bson::String(":x".into()),
            Bson::Boolean(true),
            Bson::Boolean(false),
            Bson::ObjectId(oid),
            date(1704164645678),
            date(1704164645000),
            date(-1000),
            date(-1500),
            date(-62_135_596_800_000),
            date(253_402_300_799_999),
            date(253_402_300_800_000),
            date(-62_135_596_800_001),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![1, 2, 3, 255],
            }),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![],
            }),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: vec![7; 16],
            }),
            Bson::Binary(Binary {
                subtype: BinarySubtype::UserDefined(0x80),
                bytes: vec![9],
            }),
            Bson::Timestamp(Timestamp {
                time: 1700000000,
                increment: 7,
            }),
            Bson::Timestamp(Timestamp {
                time: u32::MAX,
                increment: u32::MAX,
            }),
            Bson::RegularExpression(Regex {
                pattern: "^ab+c/\\d".try_into().unwrap(),
                options: "imsx".try_into().unwrap(),
            }),
            Bson::MinKey,
            Bson::MaxKey,
            Bson::Undefined,
            Bson::JavaScriptCode("return 1".into()),
            Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope {
                code: "return x".into(),
                scope: doc! {"x": 1, "y": 1.0, "z": -3.0, "w": 2.5, "l": Bson::Int64(4)},
            }),
            Bson::Symbol("sym".into()),
            db_pointer,
        ];
        let mut all = scalars.clone();
        all.push(Bson::Document(doc! {}));
        all.push(Bson::Array(vec![]));
        all.push(Bson::Array(scalars.clone()));
        let mut every = mongodb::bson::Document::new();
        for (i, v) in scalars.iter().enumerate() {
            every.insert(format!("k{i}"), v.clone());
        }
        all.push(Bson::Document(doc! {
            "z": 1,
            "a b": Bson::Int64(2),
            "$set": {"é": -1, "_id": oid},
            "nested": {"list": [every.clone(), [1, [2.0, Bson::Double(f64::NAN)]]]},
        }));
        all.push(Bson::Document(every));
        all
    }

    #[test]
    fn shell_output_round_trips_for_every_cell_type() {
        for v in cell_values() {
            let text = values::shell(&v);
            let back = parse_value(&text, &none())
                .unwrap_or_else(|e| panic!("{v:?} printed as {text}: {e:?}"));
            assert!(same(&back, &v), "{text}: got {back:?}, want {v:?}");
        }
    }

    #[test]
    fn param_value_rule() {
        assert_eq!(param_value(Some("30")), Bson::Int32(30));
        assert_eq!(param_value(Some("\"30\"")), Bson::String("30".into()));
        assert_eq!(
            param_value(Some("ObjectId(\"650000000000000000000001\")")),
            Bson::ObjectId(ObjectId::parse_str("650000000000000000000001").unwrap())
        );
        assert_eq!(param_value(Some("abc")), Bson::String("abc".into()));
        assert_eq!(param_value(Some("")), Bson::String(String::new()));
        assert_eq!(param_value(None), Bson::Null);
        assert_eq!(param_value(Some(" 5 ")), Bson::Int32(5));
        assert_eq!(param_value(Some("1 2")), Bson::String("1 2".into()));
    }

    #[test]
    fn params_in_values() {
        let mut params = ParamValues::new();
        params.insert("age".into(), Some("30".into()));
        params.insert("name".into(), Some("Ann".into()));
        params.insert("gone".into(), None);
        params.insert("id".into(), Some("650000000000000000000001".into()));
        assert_eq!(
            parse_value(
                "{ age: :age, name: :name, gone: :gone, id: ObjectId(:id) }",
                &params
            )
            .unwrap(),
            Bson::Document(doc! {
                "age": 30,
                "name": "Ann",
                "gone": Bson::Null,
                "id": ObjectId::parse_str("650000000000000000000001").unwrap(),
            })
        );
        let e = parse_value("[1, :missing]", &params).unwrap_err();
        assert_eq!(e, ParseError::new("No value for :missing", 4));
    }

    #[test]
    fn errors_point_at_the_token() {
        let e = parse_value("{ a: 1, b: }", &none()).unwrap_err();
        assert_eq!(e, ParseError::new("Expected a value.", 11));
        // After multi-byte text, the offset counts chars.
        let e = parse_value("{ name: \"Zoë 😀\", age: }", &none()).unwrap_err();
        assert_eq!(e.at, 22);
        // An extra token after the value.
        let e = parse_value("{ a: 1 } 2", &none()).unwrap_err();
        assert_eq!(e, ParseError::new("Unexpected text after the value.", 9));
        // Running out of tokens points just past the last one.
        let e = parse_value("{ a: 1, ", &none()).unwrap_err();
        assert_eq!(e, ParseError::new("Expected a key or }.", 7));
        assert_eq!(
            parse_value("", &none()).unwrap_err(),
            ParseError::new("Expected a value.", 0)
        );
        let e = parse_value(r#"{ a: NumberLong("x") }"#, &none()).unwrap_err();
        assert_eq!(e.at, 16);
    }

    #[test]
    fn bad_constructor_arguments() {
        assert!(err(r#"ObjectId("abc")"#).starts_with("ObjectId needs 24 hex digits"));
        assert!(err("NumberInt(2147483648)").starts_with("NumberInt needs"));
        assert!(err(r#"NumberLong("1.5")"#).starts_with("NumberLong needs"));
        assert!(err(r#"ISODate("tomorrow")"#).starts_with(r#"Invalid date "tomorrow""#));
        assert!(err(r#"UUID("nope")"#).starts_with("UUID needs"));
        assert!(err(r#"BinData(256, "AQID")"#).starts_with("BinData needs"));
        assert!(err(r#"BinData(0, "***")"#).starts_with("BinData needs"));
        assert!(err("Timestamp(-1, 2)").starts_with("Timestamp needs"));
        assert!(err("MinKey(1)").starts_with("MinKey takes no arguments"));
        assert!(err("/a/g").starts_with(r#"Invalid regex flags "g""#));
        assert_eq!(err("abc"), "Unknown name abc. Put text in quotes.");
        assert_eq!(
            err("Date(\"2026-01-01\")"),
            "Date() makes a string in mongosh. Use new Date() or ISODate()."
        );
    }

    #[test]
    fn document_requires_a_document() {
        let tokens = crate::mongoquery::lex::tokenize("{ a: 1 } [1]").unwrap();
        let params = none();
        let mut p = Parser::new(&tokens, &params);
        assert_eq!(p.document().unwrap(), doc! {"a": 1});
        assert_eq!(
            p.document().unwrap_err(),
            ParseError::new("Expected a document.", 9)
        );
        let tokens =
            crate::mongoquery::lex::tokenize(r#"{"$oid": "650000000000000000000001"}"#).unwrap();
        let mut p = Parser::new(&tokens, &params);
        assert_eq!(
            p.document().unwrap_err(),
            ParseError::new("Expected a document.", 0)
        );
    }
}
