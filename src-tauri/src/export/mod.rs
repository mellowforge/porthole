use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::model::CellType;

pub mod run;

#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Csv,
    Json,
}

impl ExportFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportColumn {
    pub name: String,
    pub data_type: String,
}

/// The error a stream stops with when its export is canceled.
pub const EXPORT_CANCELED: &str = "Export canceled";

/// Receives rows as a table or a query streams them.
pub trait RowSink: Send {
    /// Called once, before the first row, with the columns.
    fn begin(&mut self, columns: &[ExportColumn]) -> AppResult<()>;
    /// Returning an error stops the stream and becomes the export's error.
    fn row(&mut self, row: Vec<Option<String>>) -> AppResult<()>;
    /// A row with one `CellType` code per cell (`model::row_types`), from a database whose cells
    /// carry their own type. Sinks that don't use the types take it as a plain row.
    fn typed_row(&mut self, row: Vec<Option<String>>, _types: Option<&str>) -> AppResult<()> {
        self.row(row)
    }
    /// True when the sink takes whole documents (`document`) instead of cell rows: a
    /// document database streams them to a JSON file as they are.
    fn wants_documents(&self) -> bool {
        false
    }
    /// One document as JSON text, only if `wants_documents`.
    fn document(&mut self, _json: &str) -> AppResult<()> {
        Err(AppError::Unsupported("This export takes rows, not documents.".into()))
    }
}

fn io_err(e: std::io::Error) -> AppError {
    AppError::Query(format!("Couldn't write the file: {e}"))
}

/// Adds `_2`, `_3`, … to repeated names. A generated name that collides with a
/// real column name is skipped.
pub fn unique_names(names: &[String]) -> Vec<String> {
    let real: HashSet<&str> = names.iter().map(String::as_str).collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut counters: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if used.insert(name.clone()) {
            out.push(name.clone());
            continue;
        }
        let n = counters.entry(name.as_str()).or_insert(1);
        loop {
            *n += 1;
            let candidate = format!("{name}_{n}");
            if !real.contains(candidate.as_str()) && used.insert(candidate.clone()) {
                out.push(candidate);
                break;
            }
        }
    }
    out
}

/// End offset of the JSON number starting at `start`, per the grammar
/// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`.
fn scan_number(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    match b.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        _ => return None,
    }
    let digits = |i: &mut usize| {
        let s = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > s
    };
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return None;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return None;
        }
    }
    Some(i)
}

fn is_json_number(s: &str) -> bool {
    scan_number(s.as_bytes(), 0) == Some(s.len())
}

/// End offset (after the closing quote) of the JSON string starting at `start`.
fn scan_string(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    loop {
        match *b.get(i)? {
            b'"' => return Some(i + 1),
            b'\\' => match *b.get(i + 1)? {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => i += 2,
                b'u' if b.get(i + 2..i + 6)?.iter().all(u8::is_ascii_hexdigit) => i += 6,
                _ => return None,
            },
            0..=0x1f => return None,
            _ => i += 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Expect {
    Value,
    ValueOrArrayEnd,
    KeyOrObjectEnd,
    Key,
    Colon,
    After,
}

/// Validates `text` as JSON and returns it with the whitespace outside strings
/// removed. Keys, duplicate keys, number text and string contents are kept
/// exactly. `None` when the text is not valid JSON. Iterative, so nesting depth
/// is not limited.
fn compact_json(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut stack: Vec<u8> = Vec::new();
    let mut state = Expect::Value;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
            continue;
        }
        match state {
            Expect::Value | Expect::ValueOrArrayEnd => {
                if c == b']' && state == Expect::ValueOrArrayEnd {
                    stack.pop();
                    out.push(']');
                    state = Expect::After;
                    i += 1;
                    continue;
                }
                match c {
                    b'{' | b'[' => {
                        stack.push(c);
                        out.push(c as char);
                        state = if c == b'{' { Expect::KeyOrObjectEnd } else { Expect::ValueOrArrayEnd };
                        i += 1;
                    }
                    b'"' => {
                        let end = scan_string(b, i)?;
                        out.push_str(&text[i..end]);
                        i = end;
                        state = Expect::After;
                    }
                    b'-' | b'0'..=b'9' => {
                        let end = scan_number(b, i)?;
                        out.push_str(&text[i..end]);
                        i = end;
                        state = Expect::After;
                    }
                    _ => {
                        let lit = ["true", "false", "null"]
                            .into_iter()
                            .find(|l| text[i..].starts_with(l))?;
                        out.push_str(lit);
                        i += lit.len();
                        state = Expect::After;
                    }
                }
            }
            Expect::KeyOrObjectEnd | Expect::Key => {
                if c == b'}' && state == Expect::KeyOrObjectEnd {
                    stack.pop();
                    out.push('}');
                    state = Expect::After;
                    i += 1;
                } else if c == b'"' {
                    let end = scan_string(b, i)?;
                    out.push_str(&text[i..end]);
                    i = end;
                    state = Expect::Colon;
                } else {
                    return None;
                }
            }
            Expect::Colon => {
                if c != b':' {
                    return None;
                }
                out.push(':');
                i += 1;
                state = Expect::Value;
            }
            Expect::After => match (stack.last(), c) {
                (Some(b'['), b',') => {
                    out.push(',');
                    i += 1;
                    state = Expect::Value;
                }
                (Some(b'{'), b',') => {
                    out.push(',');
                    i += 1;
                    state = Expect::Key;
                }
                (Some(b'['), b']') | (Some(b'{'), b'}') => {
                    stack.pop();
                    out.push(c as char);
                    i += 1;
                }
                _ => return None,
            },
        }
    }
    (state == Expect::After && stack.is_empty()).then_some(out)
}

fn csv_field(out: &mut String, value: &str) {
    let needs_quotes = value.is_empty()
        || value.contains([',', '"', '\r', '\n'])
        || value.starts_with(' ')
        || value.ends_with(' ');
    if needs_quotes {
        out.push('"');
        out.push_str(&value.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(value);
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serializes")
}

fn json_value(out: &mut String, data_type: &str, value: &str) {
    match data_type {
        "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "oid" => {
            if is_json_number(value) {
                out.push_str(value);
            } else {
                out.push_str(&json_string(value));
            }
        }
        "bool" => match value {
            "true" | "t" => out.push_str("true"),
            "false" | "f" => out.push_str("false"),
            _ => out.push_str(&json_string(value)),
        },
        "json" | "jsonb" => match compact_json(value) {
            Some(compact) => out.push_str(&compact),
            None => out.push_str(&json_string(value)),
        },
        _ => out.push_str(&json_string(value)),
    }
}

/// A cell's own type decides its JSON form; `None` for a code that isn't one.
fn json_typed_value(out: &mut String, code: u8, value: &str) -> Option<()> {
    match CellType::from_code(code as char)? {
        CellType::Null => out.push_str("null"),
        CellType::Integer | CellType::Real if is_json_number(value) => out.push_str(value),
        CellType::Long | CellType::Decimal if is_json_number(value) => out.push_str(value),
        CellType::Bool => match value {
            "true" => out.push_str("true"),
            "false" => out.push_str("false"),
            _ => out.push_str(&json_string(value)),
        },
        CellType::Json | CellType::Other => match compact_json(value) {
            Some(compact) => out.push_str(&compact),
            None => out.push_str(&json_string(value)),
        },
        CellType::Integer
        | CellType::Real
        | CellType::Text
        | CellType::Blob
        | CellType::Long
        | CellType::Decimal
        | CellType::ObjectId
        | CellType::Date => out.push_str(&json_string(value)),
    }
    Some(())
}

pub struct ExportWriter {
    out: Option<BufWriter<File>>,
    tmp: PathBuf,
    target: PathBuf,
    format: ExportFormat,
    /// `"name":` for each column, escaped once.
    keys: Vec<String>,
    types: Vec<String>,
    rows: u64,
}

impl ExportWriter {
    pub fn create(target: &Path, format: ExportFormat, columns: &[ExportColumn]) -> AppResult<Self> {
        let mut file_name = target
            .file_name()
            .map(|n| n.to_os_string())
            .ok_or_else(|| AppError::Query("Couldn't write the file: no file name".into()))?;
        file_name.push(format!(".{}.part", uuid::Uuid::new_v4().simple()));
        let tmp = target.with_file_name(file_name);
        let file = File::create(&tmp).map_err(io_err)?;
        let raw_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let names = unique_names(&raw_names);
        let mut w = ExportWriter {
            out: Some(BufWriter::new(file)),
            tmp,
            target: target.to_path_buf(),
            format,
            keys: names.iter().map(|n| format!("{}:", json_string(n))).collect(),
            types: columns.iter().map(|c| c.data_type.clone()).collect(),
            rows: 0,
        };
        let head = match format {
            ExportFormat::Csv => {
                let mut line = String::new();
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    csv_field(&mut line, n);
                }
                line.push_str("\r\n");
                line
            }
            ExportFormat::Json => "[".to_string(),
        };
        // On failure `w` is dropped here, which removes the temp file.
        w.write_str(&head)?;
        Ok(w)
    }

    fn write_str(&mut self, s: &str) -> AppResult<()> {
        match self.out.as_mut() {
            Some(out) => out.write_all(s.as_bytes()).map_err(io_err),
            None => Err(AppError::Query("Couldn't write the file: already closed".into())),
        }
    }

    pub fn write_row(&mut self, row: &[Option<String>]) -> AppResult<()> {
        self.write_typed_row(row, None)
    }

    /// `types`: one `CellType` code per cell. In JSON a cell with a code follows it (`n` null;
    /// `i`, `r` a number when the text is one; `t`, `b` a string); without one, its column's
    /// data type decides. CSV ignores the codes.
    pub fn write_typed_row(&mut self, row: &[Option<String>], types: Option<&str>) -> AppResult<()> {
        let codes = types.map_or(&[][..], str::as_bytes);
        let mut line = String::new();
        match self.format {
            ExportFormat::Csv => {
                for (i, cell) in row.iter().enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    if let Some(v) = cell {
                        csv_field(&mut line, v);
                    }
                }
                line.push_str("\r\n");
            }
            ExportFormat::Json => {
                line.push_str(if self.rows == 0 { "\n{" } else { ",\n{" });
                for (i, cell) in row.iter().enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    line.push_str(self.keys.get(i).map_or("\"\":", String::as_str));
                    match cell {
                        None => line.push_str("null"),
                        Some(v) => {
                            let typed = codes.get(i).and_then(|&c| json_typed_value(&mut line, c, v));
                            if typed.is_none() {
                                json_value(&mut line, self.types.get(i).map_or("", String::as_str), v)
                            }
                        }
                    }
                }
                line.push('}');
            }
        }
        self.write_str(&line)?;
        self.rows += 1;
        Ok(())
    }

    /// One JSON document as an array element, in the layout of `write_typed_row`'s objects.
    /// JSON files only; `json` must be valid JSON.
    pub fn write_document(&mut self, json: &str) -> AppResult<()> {
        if self.format != ExportFormat::Json {
            return Err(AppError::Unsupported("Documents can only be written to a JSON export.".into()));
        }
        let compact = compact_json(json)
            .ok_or_else(|| AppError::Query("Export got a document that isn't valid JSON".into()))?;
        let line = format!("{}{compact}", if self.rows == 0 { "\n" } else { ",\n" });
        self.write_str(&line)?;
        self.rows += 1;
        Ok(())
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn finish(mut self) -> AppResult<u64> {
        if self.format == ExportFormat::Json {
            let tail = if self.rows == 0 { "]\n" } else { "\n]\n" };
            self.write_str(tail)?;
        }
        let out = self.out.take().expect("writer is open until finish");
        let file = out.into_inner().map_err(|e| io_err(e.into_error()))?;
        file.sync_all().map_err(io_err)?;
        drop(file);
        fs::rename(&self.tmp, &self.target).map_err(io_err)?;
        // Renamed: nothing left for Drop to remove.
        self.tmp = PathBuf::new();
        Ok(self.rows)
    }

    pub fn abort(self) {
        // Drop removes the temp file.
    }
}

impl Drop for ExportWriter {
    fn drop(&mut self) {
        if self.tmp.as_os_str().is_empty() {
            return;
        }
        self.out.take();
        let _ = fs::remove_file(&self.tmp);
    }
}

struct Job {
    window: String,
    cancel: Arc<AtomicBool>,
}

/// Cancel flags of the exports in flight, by export id, with the window each runs for.
#[derive(Default)]
pub struct ExportJobs {
    flags: Mutex<HashMap<String, Job>>,
}

impl ExportJobs {
    fn jobs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Job>> {
        self.flags.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn start(&self, window: &str, id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.jobs().insert(id.to_string(), Job { window: window.to_string(), cancel: flag.clone() });
        flag
    }

    pub fn cancel(&self, id: &str) {
        if let Some(job) = self.jobs().get(id) {
            job.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Cancels every export of `window`: its session was replaced or removed, or it closed.
    pub fn cancel_window(&self, window: &str) {
        for job in self.jobs().values().filter(|j| j.window == window) {
            job.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Cancels every export (the app is quitting) and returns how many were running.
    pub fn cancel_all(&self) -> usize {
        let jobs = self.jobs();
        for job in jobs.values() {
            job.cancel.store(true, Ordering::SeqCst);
        }
        jobs.len()
    }

    pub fn finish(&self, id: &str) {
        self.jobs().remove(id);
    }

    /// True when no export is running.
    pub fn is_idle(&self) -> bool {
        self.jobs().is_empty()
    }

    /// Waits until no export is left (each has removed its temporary file by then), at most
    /// `limit`. Returns whether they all finished.
    pub async fn wait_idle(&self, limit: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if self.jobs().is_empty() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20).min(limit)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ExportColumn {
        ExportColumn { name: name.into(), data_type: ty.into() }
    }

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    fn write_all(
        format: ExportFormat,
        cols: &[ExportColumn],
        rows: &[Vec<Option<String>>],
    ) -> String {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out");
        let mut w = ExportWriter::create(&target, format, cols).unwrap();
        for r in rows {
            w.write_row(r).unwrap();
        }
        assert_eq!(w.finish().unwrap(), rows.len() as u64);
        fs::read_to_string(&target).unwrap()
    }

    #[test]
    fn json_export_of_new_codes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out");
        let cols = [col("a", ""), col("b", ""), col("c", ""), col("d", ""), col("e", ""), col("f", ""), col("g", ""), col("h", "")];
        let mut w = ExportWriter::create(&target, ExportFormat::Json, &cols).unwrap();
        let row = vec![
            s("9007199254740993"), s("NaN"), s("true"), s("{\"a\":1}"), s("507f1f77bcf86cd799439011"),
            s("2024-01-02T03:04:05Z"), s("{\"$binary\":{}}"), s("1.10"),
        ];
        w.write_typed_row(&row, Some("lmyjodxm")).unwrap();
        w.finish().unwrap();
        let text = fs::read_to_string(&target).unwrap();
        assert!(text.contains("\"a\":9007199254740993,\"b\":\"NaN\",\"c\":true,\"d\":{\"a\":1},"), "{text}");
        assert!(text.contains("\"h\":1.10}"), "{text}");
        let got: serde_json::Value = serde_json::from_str(&text).unwrap();
        let o = &got[0];
        assert_eq!(o["b"], "NaN");
        assert_eq!(o["c"], true);
        assert_eq!(o["d"], serde_json::json!({"a": 1}));
        assert_eq!(o["e"], "507f1f77bcf86cd799439011");
        assert_eq!(o["f"], "2024-01-02T03:04:05Z");
        assert_eq!(o["g"], serde_json::json!({"$binary": {}}));
    }

    #[test]
    fn documents_are_refused_by_a_csv_export() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ExportWriter::create(&dir.path().join("out.csv"), ExportFormat::Csv, &[]).unwrap();
        assert!(matches!(w.write_document("{}"), Err(AppError::Unsupported(_))));
    }

    #[test]
    fn documents_are_array_elements_in_the_row_layout() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out");
        let mut w = ExportWriter::create(&target, ExportFormat::Json, &[]).unwrap();
        w.write_document("{ \"a\": 1 }").unwrap();
        w.write_document("{\"b\":[1,2]}").unwrap();
        assert!(w.write_document("{oops").is_err());
        assert_eq!(w.finish().unwrap(), 2);
        assert_eq!(fs::read_to_string(&target).unwrap(), "[\n{\"a\":1},\n{\"b\":[1,2]}\n]\n");
    }

    #[test]
    fn csv_quotes_only_when_needed() {
        let got = write_all(
            ExportFormat::Csv,
            &[col("a", "text"), col("b", "text")],
            &[
                vec![s("x"), None],
                vec![s(""), s("he said \"hi\", ok")],
                vec![s(" lead"), s("two\nlines")],
            ],
        );
        assert_eq!(
            got,
            "a,b\r\nx,\r\n\"\",\"he said \"\"hi\"\", ok\"\r\n\" lead\",\"two\nlines\"\r\n"
        );
    }

    #[test]
    fn csv_keeps_unicode() {
        let got = write_all(ExportFormat::Csv, &[col("a", "text")], &[vec![s("Zoë 🐘")]]);
        assert_eq!(got.as_bytes(), "a\r\nZoë 🐘\r\n".as_bytes());
        assert!(!got.starts_with('\u{feff}'));
    }

    #[test]
    fn json_types() {
        let cols = [
            col("i", "int4"),
            col("n", "numeric"),
            col("f", "float8"),
            col("b", "bool"),
            col("j", "jsonb"),
            col("t", "text"),
            col("x", "numeric"),
        ];
        let got = write_all(
            ExportFormat::Json,
            &cols,
            &[vec![
                s("42"),
                s("1.50"),
                s("NaN"),
                s("true"),
                s("{\"a\":[1]}"),
                s("007"),
                s("1e400"),
            ]],
        );
        assert_eq!(
            got,
            "[\n{\"i\":42,\"n\":1.50,\"f\":\"NaN\",\"b\":true,\"j\":{\"a\":[1]},\"t\":\"007\",\"x\":1e400}\n]\n"
        );
    }

    fn write_typed(cols: &[ExportColumn], rows: &[(Vec<Option<String>>, Option<&str>)]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out");
        let mut w = ExportWriter::create(&target, ExportFormat::Json, cols).unwrap();
        for (r, types) in rows {
            w.write_typed_row(r, *types).unwrap();
        }
        w.finish().unwrap();
        fs::read_to_string(&target).unwrap()
    }

    #[test]
    fn json_export_uses_cell_types() {
        let cols: Vec<ExportColumn> = ["a", "b", "c", "d", "e"].iter().map(|n| col(n, "text")).collect();
        let row = vec![s("5"), s("5"), None, s("1.5"), s("\\xdead")];
        assert_eq!(
            write_typed(&cols, &[(row.clone(), Some("itnrb"))]),
            "[\n{\"a\":5,\"b\":\"5\",\"c\":null,\"d\":1.5,\"e\":\"\\\\xdead\"}\n]\n"
        );
        assert_eq!(
            write_typed(&cols, &[(row, None)]),
            "[\n{\"a\":\"5\",\"b\":\"5\",\"c\":null,\"d\":\"1.5\",\"e\":\"\\\\xdead\"}\n]\n"
        );
    }

    #[test]
    fn json_export_falls_back_to_column_types_without_a_code() {
        let cols = [col("a", "int4"), col("b", "text"), col("c", "int4")];
        let row = vec![s("5"), s("5"), s("7")];
        // A code that isn't one: the column's type decides.
        assert_eq!(
            write_typed(&cols, &[(row.clone(), Some("xzq"))]),
            "[\n{\"a\":5,\"b\":\"5\",\"c\":7}\n]\n"
        );
        // Fewer codes than cells: the cells past the end follow their column's type.
        assert_eq!(
            write_typed(&cols, &[(row, Some("t"))]),
            "[\n{\"a\":\"5\",\"b\":\"5\",\"c\":7}\n]\n"
        );
    }

    #[test]
    fn json_export_real_inf_is_a_string() {
        assert_eq!(write_typed(&[col("r", "float8")], &[(vec![s("Inf")], Some("r"))]), "[\n{\"r\":\"Inf\"}\n]\n");
    }

    #[test]
    fn json_null_and_bad_json() {
        let got = write_all(
            ExportFormat::Json,
            &[col("a", "int4"), col("j", "jsonb")],
            &[vec![None, s("{oops")]],
        );
        assert_eq!(got, "[\n{\"a\":null,\"j\":\"{oops\"}\n]\n");
    }

    #[test]
    fn json_multiline_json_value_stays_on_one_line() {
        let got = write_all(
            ExportFormat::Json,
            &[col("j", "json")],
            &[vec![s("{\n  \"a\": 1\n}")]],
        );
        assert_eq!(got, "[\n{\"j\":{\"a\":1}}\n]\n");
    }

    #[test]
    fn json_values_are_embedded_losslessly() {
        let got = write_all(
            ExportFormat::Json,
            &[col("j", "jsonb")],
            &[
                vec![s("{\"b\":1,\"a\":2,\"b\":3}")],
                vec![s("{\"x\": 1.50, \"y\": [1e400, -0]}")],
                vec![s("{\n  \"s\": \"a  b \\\" c\\n\",\n  \"n\": 2.50\n}")],
            ],
        );
        assert_eq!(
            got,
            "[\n{\"j\":{\"b\":1,\"a\":2,\"b\":3}},\n{\"j\":{\"x\":1.50,\"y\":[1e400,-0]}},\n{\"j\":{\"s\":\"a  b \\\" c\\n\",\"n\":2.50}}\n]\n"
        );
    }

    #[test]
    fn json_deep_nesting_still_embeds() {
        let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        let got = write_all(ExportFormat::Json, &[col("j", "json")], &[vec![s(&deep)]]);
        assert_eq!(got, format!("[\n{{\"j\":{deep}}}\n]\n"));
    }

    #[test]
    fn compact_json_rejects_invalid() {
        for bad in ["", "{", "{\"a\"}", "[1,]", "{\"a\":1,}", "[1 2]", "{oops", "01", "\"a", "[1]]", "tru", "{\"a\":\"\\x\"}", "[\"\t\"]"] {
            assert!(compact_json(bad).is_none(), "{bad:?} should be invalid");
        }
        for ok in ["null", " 12 ", "[]", "{}", "\"\\u00e9\""] {
            assert!(compact_json(ok).is_some(), "{ok:?} should be valid");
        }
    }

    #[test]
    fn json_empty_is_brackets() {
        assert_eq!(write_all(ExportFormat::Json, &[col("a", "int4")], &[]), "[]\n");
        let got = write_all(
            ExportFormat::Json,
            &[col("a", "int4")],
            &[vec![s("1")], vec![s("2")]],
        );
        assert_eq!(got, "[\n{\"a\":1},\n{\"a\":2}\n]\n");
    }

    #[test]
    fn json_duplicate_columns_are_renamed() {
        let got = write_all(
            ExportFormat::Json,
            &[col("id", "int4"), col("id", "int4")],
            &[vec![s("1"), s("2")]],
        );
        assert_eq!(got, "[\n{\"id\":1,\"id_2\":2}\n]\n");
        let csv = write_all(
            ExportFormat::Csv,
            &[col("id", "int4"), col("id", "int4")],
            &[],
        );
        assert_eq!(csv, "id,id_2\r\n");
    }

    #[test]
    fn json_number_checker() {
        for ok in ["0", "-0", "1", "-12", "1.5", "1e+5", "-1.5E-3", "0.0", "1e400"] {
            assert!(is_json_number(ok), "{ok} should be valid");
        }
        for bad in ["", "-", "01", "1.", ".5", "1e", "1e+", "+1", "NaN", "Infinity", "1 ", "0x1"] {
            assert!(!is_json_number(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn unique_names_suffixes() {
        let v = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            unique_names(&v(&["id", "name", "id", "id"])),
            v(&["id", "name", "id_2", "id_3"])
        );
        assert_eq!(unique_names(&v(&["a", "a", "a_2"])), v(&["a", "a_3", "a_2"]));
    }

    fn part_files(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".part"))
            .count()
    }

    #[test]
    fn writer_abort_leaves_target_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.csv");
        fs::write(&target, "old").unwrap();
        let mut w = ExportWriter::create(&target, ExportFormat::Csv, &[col("a", "text")]).unwrap();
        w.write_row(&[s("x")]).unwrap();
        assert_eq!(part_files(dir.path()), 1);
        w.abort();
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        assert_eq!(part_files(dir.path()), 0);

        let w = ExportWriter::create(&target, ExportFormat::Json, &[col("a", "text")]).unwrap();
        drop(w);
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        assert_eq!(part_files(dir.path()), 0);
    }

    #[test]
    fn writer_finish_replaces_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.csv");
        fs::write(&target, "old").unwrap();
        let mut w = ExportWriter::create(&target, ExportFormat::Csv, &[col("a", "text")]).unwrap();
        w.write_row(&[s("x")]).unwrap();
        assert_eq!(w.rows(), 1);
        w.finish().unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "a\r\nx\r\n");
        assert_eq!(part_files(dir.path()), 0);
    }

    #[test]
    fn jobs_cancel_window_sets_only_that_windows_flags() {
        let jobs = ExportJobs::default();
        let a1 = jobs.start("w1", "a1");
        let a2 = jobs.start("w1", "a2");
        let b = jobs.start("w2", "b");
        jobs.cancel_window("nobody");
        assert!(![&a1, &a2, &b].iter().any(|f| f.load(Ordering::SeqCst)));
        jobs.cancel_window("w1");
        assert!(a1.load(Ordering::SeqCst) && a2.load(Ordering::SeqCst));
        assert!(!b.load(Ordering::SeqCst));
        assert_eq!(jobs.cancel_all(), 3);
        assert!(b.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn jobs_wait_idle_returns_when_the_jobs_finish_or_at_the_limit() {
        use std::time::{Duration, Instant};
        let jobs = Arc::new(ExportJobs::default());
        assert_eq!(jobs.cancel_all(), 0);
        jobs.start("w1", "a");
        let finisher = {
            let jobs = jobs.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                jobs.finish("a");
            })
        };
        let started = Instant::now();
        assert!(jobs.wait_idle(Duration::from_secs(5)).await);
        assert!(started.elapsed() < Duration::from_secs(2));
        finisher.await.unwrap();

        jobs.start("w1", "stuck");
        let started = Instant::now();
        assert!(!jobs.wait_idle(Duration::from_millis(150)).await);
        assert!(started.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn jobs_cancel_sets_the_flag() {
        let jobs = ExportJobs::default();
        let flag = jobs.start("main", "e1");
        assert!(!flag.load(Ordering::SeqCst));
        jobs.cancel("nope");
        assert!(!flag.load(Ordering::SeqCst));
        jobs.cancel("e1");
        assert!(flag.load(Ordering::SeqCst));
        jobs.finish("e1");
        jobs.cancel("e1");
    }
}
