//! The work of an export after the save dialog: shared by the Tauri commands and the tests.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;

use super::{ExportColumn, ExportFormat, ExportWriter, RowSink, EXPORT_CANCELED};
use crate::driver::Driver;
use crate::error::{AppError, AppResult};
use crate::model::TableQuery;
use crate::mongoquery::MongoCall;
use crate::query::{QuerySession, RawOutcome, ResultColumn, CANCELED_CODE, EXPORT_SAVEPOINT};
use crate::sqltext::{classify_in, SqlDialect, StatementKind};

/// How often progress is reported, in rows.
const PROGRESS_EVERY: u64 = 1000;
/// How often a waiting export looks at its cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(100);

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ExportOutcome {
    Done { rows: u64, path: String, file_name: String },
    Canceled,
    /// The save dialog was closed without choosing a file.
    Dismissed,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ExportProgress {
    pub rows: u64,
}

fn done(rows: u64, target: &Path) -> ExportOutcome {
    ExportOutcome::Done {
        rows,
        path: target.to_string_lossy().into_owned(),
        file_name: target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

struct WriterSink<'a> {
    target: &'a Path,
    format: ExportFormat,
    cancel: &'a AtomicBool,
    progress: &'a (dyn Fn(u64) + Sync),
    writer: Option<ExportWriter>,
    canceled: bool,
    /// False when the stream is stopped on the server instead (a query tab's session): on
    /// cancel the temporary file is removed at once and rows that still arrive are dropped.
    stop_on_cancel: bool,
}

impl WriterSink<'_> {
    fn stop_for_cancel(&mut self) -> AppError {
        self.canceled = true;
        if let Some(w) = self.writer.take() {
            w.abort();
        }
        AppError::Query(EXPORT_CANCELED.into())
    }
}

impl RowSink for WriterSink<'_> {
    fn begin(&mut self, columns: &[ExportColumn]) -> AppResult<()> {
        if self.cancel.load(Ordering::SeqCst) {
            let e = self.stop_for_cancel();
            return if self.stop_on_cancel { Err(e) } else { Ok(()) };
        }
        self.writer = Some(ExportWriter::create(self.target, self.format, columns)?);
        Ok(())
    }

    fn row(&mut self, row: Vec<Option<String>>) -> AppResult<()> {
        self.typed_row(row, None)
    }

    fn typed_row(&mut self, row: Vec<Option<String>>, types: Option<&str>) -> AppResult<()> {
        if self.cancel.load(Ordering::SeqCst) {
            let e = self.stop_for_cancel();
            return if self.stop_on_cancel { Err(e) } else { Ok(()) };
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| AppError::Query("Export got a row before its columns".into()))?;
        writer.write_typed_row(&row, types)?;
        if writer.rows() % PROGRESS_EVERY == 0 {
            (self.progress)(writer.rows());
        }
        Ok(())
    }

    fn wants_documents(&self) -> bool {
        self.format == ExportFormat::Json
    }

    fn document(&mut self, json: &str) -> AppResult<()> {
        if self.format != ExportFormat::Json {
            return Err(AppError::Unsupported("Documents can only be written to a JSON export.".into()));
        }
        if self.cancel.load(Ordering::SeqCst) {
            let e = self.stop_for_cancel();
            return if self.stop_on_cancel { Err(e) } else { Ok(()) };
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| AppError::Query("Export got a document before its columns".into()))?;
        writer.write_document(json)?;
        if writer.rows() % PROGRESS_EVERY == 0 {
            (self.progress)(writer.rows());
        }
        Ok(())
    }
}

/// The end of an export whose rows came through `sink`: `result` is the stream's, `None` when
/// the cancel flag won. A cancel (or the sink's own cancel error) is `Canceled`, and dropping
/// `sink` removes the temporary file.
fn finish_streamed(
    sink: WriterSink<'_>,
    result: Option<AppResult<()>>,
    target: &Path,
    progress: &(dyn Fn(u64) + Sync),
) -> AppResult<ExportOutcome> {
    match result {
        None => return Ok(ExportOutcome::Canceled),
        Some(Err(_)) if sink.canceled => return Ok(ExportOutcome::Canceled),
        Some(Err(e)) => return Err(e),
        Some(Ok(())) => {}
    }
    finish_writer(sink, target, progress)
}

/// Closes the sink's file at `target` and reports the rows it holds.
fn finish_writer(mut sink: WriterSink<'_>, target: &Path, progress: &(dyn Fn(u64) + Sync)) -> AppResult<ExportOutcome> {
    let writer = sink
        .writer
        .take()
        .ok_or_else(|| AppError::Query("Export produced no output".into()))?;
    let rows = writer.finish()?;
    progress(rows);
    Ok(done(rows, target))
}

/// What a statement that doesn't read rows is told when its export is asked for.
pub const EXPORT_NOT_A_READ: &str = "Only a statement that reads rows can be run again for an export.";

/// Streams the table to `target` through a temporary file. The `.part` file is
/// removed on cancel or failure, and an existing `target` is left alone.
pub async fn run_table_export(
    driver: &dyn Driver,
    q: &TableQuery,
    format: ExportFormat,
    target: &Path,
    cancel: &AtomicBool,
    progress: &(dyn Fn(u64) + Sync),
) -> AppResult<ExportOutcome> {
    let mut sink = WriterSink { target, format, cancel, progress, writer: None, canceled: false, stop_on_cancel: true };
    let wait_for_cancel = async {
        while !cancel.load(Ordering::SeqCst) {
            tokio::time::sleep(CANCEL_POLL).await;
        }
    };
    let result = tokio::select! {
        r = driver.export_table(q, &mut sink) => Some(r),
        _ = wait_for_cancel => None,
    };
    finish_streamed(sink, result, target, progress)
}

/// How long a canceled or failed query export keeps stopping its statement before it gives up.
const STOP_LIMIT: Duration = Duration::from_secs(10);
const SAVEPOINT: &str = EXPORT_SAVEPOINT;

/// Runs a query result's statement again on the tab's session and streams every row to
/// `target`, with the same file guarantees as `run_table_export`. See `stream_query`.
#[allow(clippy::too_many_arguments)] // the session's dialect joins the export's own arguments
pub async fn run_query_export(
    session: &dyn QuerySession,
    dialect: SqlDialect,
    sql: &str,
    params: &[Option<String>],
    format: ExportFormat,
    target: &Path,
    cancel: &AtomicBool,
    progress: &(dyn Fn(u64) + Sync),
) -> AppResult<ExportOutcome> {
    let mut sink =
        WriterSink { target, format, cancel, progress, writer: None, canceled: false, stop_on_cancel: false };
    let result = stream_query(session, dialect, sql, params, &mut sink, cancel).await;
    if cancel.load(Ordering::SeqCst) {
        // A failure to put the transaction back still wins over the cancel.
        return match result {
            Err(AppError::Query(m)) if m.starts_with(TX_NOT_RESTORED) => Err(AppError::Query(m)),
            _ => Ok(ExportOutcome::Canceled), // dropping `sink` removes the temp file
        };
    }
    result?;
    finish_writer(sink, target, progress)
}

/// As `run_query_export`, for a parsed MongoDB `find`, `distinct` or read `aggregate`: no cap, no savepoint
/// (a tab has no transaction) and nothing to settle. Cancel drops the stream, which drops the
/// server cursor; the temporary file goes with the sink.
pub async fn run_mongo_export(
    session: &dyn QuerySession,
    call: &MongoCall,
    format: ExportFormat,
    target: &Path,
    cancel: &AtomicBool,
    progress: &(dyn Fn(u64) + Sync),
) -> AppResult<ExportOutcome> {
    if !call.is_exportable() {
        return Err(AppError::Query(EXPORT_NOT_A_READ.into()));
    }
    if cancel.load(Ordering::SeqCst) {
        return Ok(ExportOutcome::Canceled);
    }
    let mut sink =
        WriterSink { target, format, cancel, progress, writer: None, canceled: false, stop_on_cancel: true };
    let wait_for_cancel = async {
        while !cancel.load(Ordering::SeqCst) {
            tokio::time::sleep(CANCEL_POLL).await;
        }
    };
    let result = tokio::select! {
        r = session.stream_mongo(call, &mut sink) => Some(r),
        _ = wait_for_cancel => None,
    };
    finish_streamed(sink, result, target, progress)
}

const TX_NOT_RESTORED: &str = "The export couldn't put the tab's transaction back as it was";

/// Streams a read statement into `sink` on a query tab's session. Only read statements run:
/// running anything else again would repeat its changes. `dialect` is the session's: it decides
/// what counts as a read (a SQLite `PRAGMA table_info(t)` does).
///
/// The session is left as it was found:
/// - Inside the tab's transaction the statement runs under a savepoint, so a cancel or an
///   error doesn't abort that transaction.
/// - Cancel stops the statement on the server (the session stays open). The server ignores
///   a cancel that arrives between two messages, so it is sent again until the stream ends.
/// - A statement left running (a cancel, or the sink failing mid-stream) is stopped, and
///   the session is idle again before this returns, so a late cancel can't hit the next
///   statement.
pub async fn stream_query(
    session: &dyn QuerySession,
    dialect: SqlDialect,
    sql: &str,
    params: &[Option<String>],
    sink: &mut dyn RowSink,
    cancel: &AtomicBool,
) -> AppResult<()> {
    if classify_in(dialect, sql).0 != StatementKind::Read {
        return Err(AppError::Query(EXPORT_NOT_A_READ.into()));
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(AppError::Query(EXPORT_CANCELED.into()));
    }
    let savepoint = session.export_savepoint().await?;
    let mut tracked = TrackedSink { inner: sink, failed: false };
    let (result, interrupted) = {
        let export = session.stream_rows(sql, params, &mut tracked);
        tokio::pin!(export);
        let wait_for_cancel = async {
            while !cancel.load(Ordering::SeqCst) {
                tokio::time::sleep(CANCEL_POLL).await;
            }
        };
        tokio::select! {
            r = &mut export => (r, false),
            _ = wait_for_cancel => {
                let deadline = tokio::time::Instant::now() + STOP_LIMIT;
                let r = loop {
                    if let Err(e) = session.cancel().await {
                        eprintln!("could not cancel the export query: {e}");
                    }
                    tokio::select! {
                        r = &mut export => break r,
                        _ = tokio::time::sleep(CANCEL_POLL) => {}
                    }
                    if tokio::time::Instant::now() >= deadline {
                        break Err(AppError::Query(EXPORT_CANCELED.into()));
                    }
                };
                (r, true)
            }
        }
    };
    // The sink failing mid-stream drops the stream while the server still runs the statement.
    if (interrupted || tracked.failed) && !settle(session).await && savepoint {
        // Ending the savepoint would wait behind the statement for as long as it runs.
        return Err(AppError::Query(format!(
            "{TX_NOT_RESTORED}: the query is still running on the server. Roll back before going on."
        )));
    }
    if savepoint {
        end_savepoint(session, result.is_ok()).await?;
    }
    result
}

/// Notes whether the sink failed, which leaves the statement running on the server.
struct TrackedSink<'a> {
    inner: &'a mut dyn RowSink,
    failed: bool,
}

impl RowSink for TrackedSink<'_> {
    fn begin(&mut self, columns: &[ExportColumn]) -> AppResult<()> {
        self.inner.begin(columns).inspect_err(|_| self.failed = true)
    }

    fn row(&mut self, row: Vec<Option<String>>) -> AppResult<()> {
        self.inner.row(row).inspect_err(|_| self.failed = true)
    }

    fn typed_row(&mut self, row: Vec<Option<String>>, types: Option<&str>) -> AppResult<()> {
        self.inner.typed_row(row, types).inspect_err(|_| self.failed = true)
    }
}

/// Waits until the session answers a probe, canceling whatever still runs on it. Cancels
/// sent earlier get time to land first; a probe one of them hits is simply sent again.
/// False when the session is still busy (or gone) after `STOP_LIMIT`.
async fn settle(session: &dyn QuerySession) -> bool {
    let deadline = tokio::time::Instant::now() + STOP_LIMIT;
    loop {
        tokio::time::sleep(CANCEL_POLL).await;
        match tokio::time::timeout(CANCEL_POLL * 5, session.run_simple("SELECT 1")).await {
            Ok(RawOutcome::Error { code: Some(code), .. }) if code == CANCELED_CODE => {}
            // An aborted transaction rejects the probe, but it answered: nothing runs.
            Ok(_) => return true,
            // Still busy with the rest of the statement.
            Err(_) => {
                if let Err(e) = session.cancel().await {
                    eprintln!("could not cancel the export query: {e}");
                }
            }
        }
        if session.is_closed() || tokio::time::Instant::now() >= deadline {
            eprintln!("the export query did not stop in time; the tab may stay busy until it ends");
            return false;
        }
    }
}

/// Releases the export's savepoint, or rolls back to it when the export failed. A late
/// cancel can make either fail with 57014, which aborts the transaction; rolling back to
/// the savepoint (which is still there) then restores it.
async fn end_savepoint(session: &dyn QuerySession, ok: bool) -> AppResult<()> {
    let release = format!("RELEASE SAVEPOINT {SAVEPOINT}");
    if ok && !matches!(session.run_simple(&release).await, RawOutcome::Error { .. }) {
        return Ok(());
    }
    let rollback = format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}");
    let mut last = String::new();
    for _ in 0..3 {
        match session.run_simple(&rollback).await {
            RawOutcome::Error { message, .. } => last = message,
            _ => match session.run_simple(&release).await {
                RawOutcome::Error { message, .. } => last = message,
                _ => return Ok(()),
            },
        }
    }
    Err(AppError::Query(format!(
        "{TX_NOT_RESTORED}, so it may have failed: {last}. Roll back before going on."
    )))
}

/// Writes rows the UI already holds, with their `cell_types` when the result had them. Same
/// guarantees as `run_table_export`.
pub fn run_rows_export(
    columns: &[ResultColumn],
    rows: &[Vec<Option<String>>],
    cell_types: Option<&[String]>,
    format: ExportFormat,
    target: &Path,
    cancel: &AtomicBool,
    progress: &dyn Fn(u64),
) -> AppResult<ExportOutcome> {
    let cols: Vec<ExportColumn> = columns
        .iter()
        .map(|c| ExportColumn { name: c.name.clone(), data_type: c.data_type.clone() })
        .collect();
    let mut writer = ExportWriter::create(target, format, &cols)?;
    for (i, row) in rows.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            writer.abort();
            return Ok(ExportOutcome::Canceled);
        }
        let types = cell_types.and_then(|t| t.get(i)).map(String::as_str);
        writer.write_typed_row(row, types)?;
        if writer.rows() % PROGRESS_EVERY == 0 {
            progress(writer.rows());
        }
    }
    let n = writer.finish()?;
    progress(n);
    Ok(done(n, target))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::query::{StatementRun, TxStatus};

    /// Decides the export savepoint itself and records every `run_simple`.
    struct SavepointFake {
        decision: bool,
        sent: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl QuerySession for SavepointFake {
        async fn run(&self, _: &str, _: &[Option<String>], _: usize) -> StatementRun {
            unreachable!()
        }
        async fn run_simple(&self, sql: &str) -> RawOutcome {
            self.sent.lock().unwrap().push(sql.to_string());
            RawOutcome::Command { rows_affected: 0 }
        }
        async fn export_savepoint(&self) -> AppResult<bool> {
            Ok(self.decision)
        }
        async fn stream_rows(&self, _: &str, _: &[Option<String>], _: &mut dyn RowSink) -> AppResult<()> {
            Ok(())
        }
        async fn cancel(&self) -> AppResult<()> {
            Ok(())
        }
        async fn tx_status(&self) -> TxStatus {
            TxStatus::Unknown
        }
        fn is_closed(&self) -> bool {
            false
        }
        async fn set_search_path(&self, _: Option<&str>) -> AppResult<()> {
            Ok(())
        }
        async fn close(&self) {}
    }

    struct NullSink;
    impl RowSink for NullSink {
        fn begin(&mut self, _: &[ExportColumn]) -> AppResult<()> {
            Ok(())
        }
        fn row(&mut self, _: Vec<Option<String>>) -> AppResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn stream_query_uses_the_sessions_savepoint_decision() {
        let cancel = AtomicBool::new(false);
        for (decision, expected) in [(false, vec![]), (true, vec!["RELEASE SAVEPOINT ts_export".to_string()])] {
            let fake = SavepointFake { decision, sent: Mutex::new(vec![]) };
            stream_query(&fake, SqlDialect::Postgres, "select 1", &[], &mut NullSink, &cancel).await.unwrap();
            assert_eq!(*fake.sent.lock().unwrap(), expected, "decision {decision}");
        }
    }

    /// Records the types each row arrives with.
    #[derive(Default)]
    struct TypesSink(Vec<Option<String>>);
    impl RowSink for TypesSink {
        fn begin(&mut self, _: &[ExportColumn]) -> AppResult<()> {
            Ok(())
        }
        fn row(&mut self, _: Vec<Option<String>>) -> AppResult<()> {
            self.0.push(None);
            Ok(())
        }
        fn typed_row(&mut self, _: Vec<Option<String>>, types: Option<&str>) -> AppResult<()> {
            self.0.push(types.map(str::to_string));
            Ok(())
        }
    }

    #[test]
    fn tracked_sink_passes_the_types_on() {
        let mut inner = TypesSink::default();
        let mut tracked = TrackedSink { inner: &mut inner, failed: false };
        tracked.typed_row(vec![None], Some("n")).unwrap();
        tracked.row(vec![None]).unwrap();
        assert_eq!(inner.0, vec![Some("n".to_string()), None]);
    }

    #[test]
    fn rows_export_uses_the_rows_cell_types() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("r.json");
        let cols = [ResultColumn { name: "v".into(), data_type: "text".into() }];
        let rows = vec![vec![Some("5".to_string())], vec![Some("5".to_string())]];
        let types = vec!["i".to_string(), "t".to_string()];
        let cancel = AtomicBool::new(false);
        run_rows_export(&cols, &rows, Some(&types), ExportFormat::Json, &target, &cancel, &|_| {}).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "[\n{\"v\":5},\n{\"v\":\"5\"}\n]\n");
    }

    #[test]
    fn outcome_and_progress_serialize_as_the_ui_expects() {
        let done = ExportOutcome::Done { rows: 17, path: "/tmp/users.csv".into(), file_name: "users.csv".into() };
        assert_eq!(
            serde_json::to_value(&done).unwrap(),
            json!({ "kind": "done", "rows": 17, "path": "/tmp/users.csv", "fileName": "users.csv" })
        );
        assert_eq!(serde_json::to_value(ExportOutcome::Canceled).unwrap(), json!({ "kind": "canceled" }));
        assert_eq!(serde_json::to_value(ExportOutcome::Dismissed).unwrap(), json!({ "kind": "dismissed" }));
        assert_eq!(serde_json::to_value(ExportProgress { rows: 1000 }).unwrap(), json!({ "rows": 1000 }));
    }
}
