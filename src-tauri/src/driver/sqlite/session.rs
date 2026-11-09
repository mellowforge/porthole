//! A query tab's own SQLite connection: one `SqliteWorker`, so the tab can hold a transaction
//! and be stopped without touching the browse connections.
//!
//! - Statements already use `?1, ?2, …` (`sqltext::rewrite_params_in`). Values bind as INTEGER
//!   when they are an integer's canonical text, otherwise as TEXT; `None` binds NULL.
//! - Stop (`cancel`) interrupts whatever runs on the connection; the statement fails with
//!   `SQLITE_INTERRUPT`, reported as `CANCELED_CODE`.
//! - SQLite has no aborted transaction state: most failed statements leave the transaction open
//!   and usable. Some failures roll the whole transaction back by themselves (an interrupted
//!   write, SQLITE_FULL / IOERR / BUSY / NOMEM, `INSERT OR ROLLBACK`, a trigger's
//!   `RAISE(ROLLBACK, …)`); `run` then adds `LOST_TRANSACTION` as a warning notice.

use std::future::Future;
use std::time::Instant;

use async_trait::async_trait;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, ErrorCode, Statement};
use tokio::sync::mpsc;

use super::{read_row, SqliteConfig, SqliteWorker, CLOSE_WAIT};
use crate::error::{AppError, AppResult};
use crate::export::{ExportColumn, RowSink};
use crate::query::bind::BindValue;
use crate::query::{
    Notice, QuerySession, RawOutcome, ResultColumn, StatementRun, TxStatus, CANCELED_CODE, EXPORT_SAVEPOINT,
};

/// The warning a failed statement gets when SQLite rolled back the transaction it ran in.
pub(super) const LOST_TRANSACTION: &str = "SQLite rolled back the open transaction, so its earlier changes are gone.";

/// Rows a streaming statement may get ahead of the sink.
pub(super) const STREAM_BUFFER: usize = 256;

pub struct SqliteQuerySession {
    worker: SqliteWorker,
}

impl SqliteQuerySession {
    /// Opens a connection of its own to the profile's file, with the startup SQL.
    pub async fn open(config: &SqliteConfig) -> AppResult<SqliteQuerySession> {
        Ok(SqliteQuerySession { worker: SqliteWorker::open_async(config).await? })
    }

    /// Whether the connection is in autocommit mode (no transaction open).
    async fn autocommit(&self) -> AppResult<bool> {
        self.worker.call(|c| c.is_autocommit()).await
    }
}

/// How a parameter binds (the shared rule): INTEGER where it is a signed integer, TEXT for
/// everything else, including an unsigned one above `i64`, NULL for `None`.
fn bind_value(v: &Option<String>) -> Value {
    match crate::query::bind::bind_value(v.as_deref()) {
        BindValue::Null => Value::Null,
        BindValue::Int(i) => Value::Integer(i),
        BindValue::UInt(_) | BindValue::Text(_) => Value::Text(v.clone().unwrap_or_default()),
    }
}

fn bind_values(params: &[Option<String>]) -> Vec<Value> {
    params.iter().map(bind_value).collect()
}

/// SQLite's message for a failure; for a syntax error without the statement and offset that
/// `Display` appends.
pub(super) fn error_message(e: rusqlite::Error) -> String {
    match e {
        rusqlite::Error::SqlInputError { msg, .. } => msg,
        e => e.to_string(),
    }
}

/// A failure as `RawOutcome::Error`: an interrupt has `CANCELED_CODE`, any other SQLite failure
/// `SQLITE_<extended code>`; a syntax error has the 1-based char position of the token SQLite
/// flags.
fn error_outcome(e: rusqlite::Error) -> RawOutcome {
    let code = |f: &rusqlite::ffi::Error| {
        Some(if f.code == ErrorCode::OperationInterrupted { CANCELED_CODE.to_string() } else { format!("SQLITE_{}", f.extended_code) })
    };
    match e {
        rusqlite::Error::SqlInputError { error, msg, sql, offset } => {
            let position = usize::try_from(offset).ok().and_then(|o| sql.get(..o)).map(|before| before.chars().count() + 1);
            RawOutcome::Error { message: msg, code: code(&error), position }
        }
        e => RawOutcome::Error { code: e.sqlite_error().and_then(code), message: e.to_string(), position: None },
    }
}

/// A failure of the worker itself (the connection is closed).
fn worker_error(e: AppError) -> RawOutcome {
    RawOutcome::Error { message: e.message().to_string(), code: None, position: None }
}

/// Each result column's name and declared type, lowercased (`""` for an expression).
fn result_columns(stmt: &Statement) -> Vec<ResultColumn> {
    stmt.columns()
        .iter()
        .map(|c| ResultColumn {
            name: c.name().to_string(),
            data_type: c.decl_type().unwrap_or("").to_lowercase(),
        })
        .collect()
}

/// Runs one statement. One without result columns runs to the end and reports its changes;
/// otherwise up to `cap` rows are read, `truncated` when there is one more.
fn run_statement(c: &Connection, sql: &str, bind: Vec<Value>, cap: usize) -> rusqlite::Result<RawOutcome> {
    let mut stmt = c.prepare(sql)?;
    if stmt.column_count() == 0 {
        let changes = stmt.execute(params_from_iter(bind))?;
        return Ok(RawOutcome::Command { rows_affected: changes as u64 });
    }
    let columns = result_columns(&stmt);
    let n = columns.len();
    let mut rows = stmt.query(params_from_iter(bind))?;
    let (mut texts, mut types) = (Vec::new(), Vec::new());
    let mut truncated = false;
    while let Some(r) = rows.next()? {
        if texts.len() == cap {
            truncated = true;
            break;
        }
        let (row, row_types) = read_row(r, n)?;
        texts.push(row);
        types.push(row_types);
    }
    Ok(RawOutcome::Rows { columns, rows: texts, truncated, cell_types: Some(types) })
}

/// What a streaming statement sends from the worker: its columns once, then its rows.
enum Streamed {
    Columns(Vec<ExportColumn>),
    Row(Vec<Option<String>>, String),
}

/// Streams the statement's rows to `tx`. The columns go out once the statement runs (after its
/// first step), so a stop aimed at the stream can't be lost before it starts. Stops quietly when
/// the receiver is gone.
fn stream_statement(c: &Connection, sql: &str, bind: Vec<Value>, tx: &mpsc::Sender<Streamed>) -> rusqlite::Result<()> {
    let mut stmt = c.prepare(sql)?;
    let columns: Vec<ExportColumn> = result_columns(&stmt)
        .into_iter()
        .map(|c| ExportColumn { name: c.name, data_type: c.data_type })
        .collect();
    let n = columns.len();
    let mut rows = stmt.query(params_from_iter(bind))?;
    let mut columns = Some(columns);
    loop {
        let next = rows.next()?;
        if let Some(columns) = columns.take()
            && tx.blocking_send(Streamed::Columns(columns)).is_err()
        {
            return Ok(());
        }
        let Some(r) = next else { return Ok(()) };
        let (row, types) = read_row(r, n)?;
        if tx.blocking_send(Streamed::Row(row, types)).is_err() {
            return Ok(());
        }
    }
}

/// Hands what a worker job sends over `rx` to `deliver`, until the job has ended and every
/// message is in; then returns the job's result. When `deliver` fails (or this future is
/// dropped), the receiver goes away, so the job can't block on a full channel, and the dropped
/// call interrupts its statement.
pub(super) async fn relay<M, T>(
    job: impl Future<Output = AppResult<T>>,
    mut rx: mpsc::Receiver<M>,
    mut deliver: impl FnMut(M) -> AppResult<()>,
) -> AppResult<T> {
    tokio::pin!(job);
    let mut finished = None;
    loop {
        tokio::select! {
            biased;
            msg = rx.recv() => match msg {
                Some(msg) => {
                    if let Err(e) = deliver(msg) {
                        drop(rx);
                        // Dropping `job` (on return) interrupts the statement if it still runs.
                        return Err(e);
                    }
                }
                None => break,
            },
            // Polled so the job is sent to the worker; its result is kept until every message is in.
            r = &mut job, if finished.is_none() => finished = Some(r),
        }
    }
    match finished {
        Some(r) => r,
        None => job.await,
    }
}

fn deliver(sink: &mut dyn RowSink, msg: Streamed) -> AppResult<()> {
    match msg {
        Streamed::Columns(columns) => sink.begin(&columns),
        Streamed::Row(row, types) => sink.typed_row(row, Some(&types)),
    }
}

#[async_trait]
impl QuerySession for SqliteQuerySession {
    async fn run(&self, sql: &str, params: &[Option<String>], row_cap: usize) -> StatementRun {
        let (sql, bind) = (sql.to_string(), bind_values(params));
        let result = self
            .worker
            .call(move |c| {
                let started = Instant::now();
                let in_transaction = !c.is_autocommit();
                let outcome = run_statement(c, &sql, bind, row_cap).unwrap_or_else(error_outcome);
                // A failure that ended the transaction: SQLite rolled it back. (A COMMIT or
                // ROLLBACK that ends it succeeds.)
                let rolled_back = in_transaction && c.is_autocommit() && matches!(outcome, RawOutcome::Error { .. });
                (outcome, rolled_back, started.elapsed().as_millis() as u64)
            })
            .await;
        let (outcome, rolled_back, elapsed_ms) = result.unwrap_or_else(|e| (worker_error(e), false, 0));
        let notices = if rolled_back {
            vec![Notice { severity: "WARNING".into(), message: LOST_TRANSACTION.into() }]
        } else {
            Vec::new()
        };
        StatementRun { outcome, notices, elapsed_ms }
    }

    /// Every row's text values, without columns or cell types.
    async fn run_simple(&self, sql: &str) -> RawOutcome {
        let sql = sql.to_string();
        let result = self.worker.call(move |c| run_statement(c, &sql, Vec::new(), usize::MAX).unwrap_or_else(error_outcome)).await;
        match result.unwrap_or_else(worker_error) {
            RawOutcome::Rows { rows, truncated, .. } => RawOutcome::Rows { columns: Vec::new(), rows, truncated, cell_types: None },
            other => other,
        }
    }

    /// Rows travel from the worker over a bounded channel (`relay`).
    async fn stream_rows(&self, sql: &str, params: &[Option<String>], sink: &mut dyn RowSink) -> AppResult<()> {
        let (tx, rx) = mpsc::channel::<Streamed>(STREAM_BUFFER);
        let (sql, bind) = (sql.to_string(), bind_values(params));
        // `tx` lives in the job, so the channel closes when the job ends (or never runs).
        let job = self.worker.call(move |c| stream_statement(c, &sql, bind, &tx));
        relay(job, rx, |msg| deliver(sink, msg)).await?.map_err(|e| AppError::Query(error_message(e)))
    }

    async fn in_transaction(&self) -> AppResult<bool> {
        Ok(!self.autocommit().await?)
    }

    /// SQLite has no aborted state: a transaction is either open and usable or gone (`run`'s
    /// `LOST_TRANSACTION` notice says when a failure ended it).
    async fn in_failed_transaction(&self) -> bool {
        false
    }

    /// Outside a transaction there is nothing to protect (and a SAVEPOINT would start one).
    async fn export_savepoint(&self) -> AppResult<bool> {
        self.worker
            .call(|c| {
                if c.is_autocommit() {
                    return Ok(false);
                }
                c.execute_batch(&format!("SAVEPOINT {EXPORT_SAVEPOINT}")).map(|()| true)
            })
            .await?
            .map_err(|e| AppError::Query(error_message(e)))
    }

    /// Interrupts whatever runs on the tab's connection now.
    async fn cancel(&self) -> AppResult<()> {
        self.worker.interrupt_current();
        Ok(())
    }

    async fn tx_status(&self) -> TxStatus {
        match self.autocommit().await {
            Ok(true) => TxStatus::Idle,
            Ok(false) => TxStatus::Open,
            Err(e) => {
                eprintln!("could not read the transaction state: {e}");
                TxStatus::Unknown
            }
        }
    }

    fn is_closed(&self) -> bool {
        self.worker.is_closed()
    }

    /// SQLite has no search path: a schema (an attached database) is named in the statement.
    async fn set_search_path(&self, _schema: Option<&str>) -> AppResult<()> {
        Ok(())
    }

    /// Interrupts what runs (nobody waits for it once the tab closes; an open transaction rolls
    /// back as the connection closes), then waits for the connection to close, up to
    /// `CLOSE_WAIT`.
    async fn close(&self) {
        self.worker.interrupt_current();
        if !self.worker.shutdown(CLOSE_WAIT).await {
            eprintln!("a query tab's SQLite connection is still busy after close; it closes when its work ends");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_integers_bind_as_integer() {
        let bind = |s: &str| bind_value(&Some(s.to_string()));
        assert_eq!(bind("42"), Value::Integer(42));
        assert_eq!(bind("0"), Value::Integer(0));
        assert_eq!(bind("-7"), Value::Integer(-7));
        assert_eq!(bind("-9223372036854775808"), Value::Integer(i64::MIN));
        for text in ["007", "-0", "+5", "1.0", "1e3", " 1", "1 ", "", "9223372036854775808", "x"] {
            assert_eq!(bind(text), Value::Text(text.into()), "{text:?}");
        }
        assert_eq!(bind_value(&None), Value::Null);
    }

    #[test]
    fn errors_carry_code_and_char_position() {
        use rusqlite::ffi;
        let interrupted = rusqlite::Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_INTERRUPT), Some("interrupted".into()));
        assert_eq!(
            error_outcome(interrupted),
            RawOutcome::Error { message: "interrupted".into(), code: Some(CANCELED_CODE.into()), position: None }
        );
        let input = rusqlite::Error::SqlInputError {
            error: ffi::Error::new(ffi::SQLITE_ERROR),
            msg: "near \"frm\": syntax error".into(),
            sql: "select 'é' frm t".into(),
            offset: 12,
        };
        assert_eq!(
            error_outcome(input),
            RawOutcome::Error { message: "near \"frm\": syntax error".into(), code: Some("SQLITE_1".into()), position: Some(12) }
        );
        let other = rusqlite::Error::InvalidParameterCount(1, 2);
        assert!(matches!(error_outcome(other), RawOutcome::Error { code: None, position: None, .. }));
    }
}
