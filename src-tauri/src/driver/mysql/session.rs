//! A query tab's own MySQL connection: one `Conn`, not pooled, so the tab can hold a transaction
//! and be stopped without touching the browse pool.
//!
//! - Statements run through `text::query_text`, so their rows come through the text protocol
//!   (with values: `PREPARE` / `EXECUTE … USING`), as the grid reads them.
//! - Stop (`cancel`) sends `KILL QUERY {id}` from a second connection; the statement fails with
//!   errno 1317, reported as `CANCELED_CODE`. It needs no lock, so it reaches a running statement.
//! - The transaction state is the `SERVER_STATUS_IN_TRANS` flag of the server's last OK packet,
//!   kept after every statement. MySQL has no aborted transaction state.
//! - The server's warnings and its `info` text become the statement's notices.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mysql_async::consts::StatusFlags;
use mysql_async::prelude::Queryable;
use mysql_async::{Column, Conn, QueryResult, Row, TextProtocol, Value};

use super::kill::{kill_query, ER_QUERY_INTERRUPTED};
use super::{connection_error, error_message, text, values, MysqlConfig, MysqlDialect};
use crate::driver::Dialect;
use crate::error::{AppError, AppResult};
use crate::export::{ExportColumn, RowSink};
use crate::model::{row_types, CellType};
use crate::query::{Notice, QuerySession, RawOutcome, ResultColumn, StatementRun, TxStatus, CANCELED_CODE, EXPORT_SAVEPOINT};
use crate::sqltext::{classify_in, SqlDialect, StatementKind};

const SESSION_CLOSED: &str = "The query session is closed";
/// The server's errno for a syntax error, whose message quotes the text from the error on.
const ER_PARSE_ERROR: u16 = 1064;
/// Reads the server's status flags after a failed statement (an ERR packet carries none) without
/// clearing its diagnostics, so a later `SHOW WARNINGS` still shows the error.
const STATUS_PROBE: &str = "SHOW WARNINGS LIMIT 0";
/// How long `close` waits for a stopped statement to give the connection back.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// The lost reason of a session whose command was dropped before it finished (`Busy`).
const STOPPED_MIDWAY: &str = "a statement was stopped before it finished, so its connection was closed";

pub struct MysqlQuerySession {
    /// `None` after `close`. Held for the whole of each statement.
    conn: tokio::sync::Mutex<Option<Conn>>,
    config: MysqlConfig,
    /// The connection's id on the server, for `KILL QUERY`.
    id: u32,
    /// The database right after setup (the startup SQL may `USE` one), which
    /// `set_search_path(None)` goes back to.
    initial_database: Option<String>,
    /// The server's in-transaction flag after the last statement.
    in_transaction: AtomicBool,
    /// Set when a client or IO error ended the connection, or at `close`.
    lost: AtomicBool,
    lost_reason: Mutex<Option<String>>,
    /// Set when a command's future was dropped while it used the connection (`Busy`): its
    /// statement may still run, and the connection may be in the middle of a reply.
    interrupted: AtomicBool,
}

/// Held by a command (`run`, `run_simple`, `stream_rows`, `set_search_path`) while it uses the
/// connection. Dropped before `finish` (the command's future was dropped mid-way:
/// a timeout, a `select!`), it marks the session lost, so the app opens a new one instead of
/// reusing a connection that is out of step with the server.
struct Busy<'a> {
    session: &'a MysqlQuerySession,
    finished: bool,
}

impl Busy<'_> {
    fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let session = self.session;
            session.interrupted.store(true, Ordering::SeqCst);
            session.lost.store(true, Ordering::SeqCst);
            let mut reason = session.lost_reason.lock().unwrap_or_else(PoisonError::into_inner);
            reason.get_or_insert_with(|| STOPPED_MIDWAY.to_string());
        }
    }
}

impl MysqlQuerySession {
    fn busy(&self) -> Busy<'_> {
        Busy { session: self, finished: false }
    }

    /// Opens a connection of its own with the profile's options (startup SQL, read-only mode),
    /// on the profile's database (none when empty).
    pub async fn open(config: &MysqlConfig) -> AppResult<MysqlQuerySession> {
        let mut conn = config.open().await?;
        // After the startup SQL, which may have chosen another database.
        let initial_database: Option<Option<String>> = match conn.query_first("SELECT DATABASE()").await {
            Ok(db) => db,
            Err(e) => {
                tokio::spawn(async move { conn.disconnect().await.ok() });
                return Err(connection_error(e));
            }
        };
        Ok(MysqlQuerySession {
            id: conn.id(),
            initial_database: initial_database.flatten(),
            in_transaction: AtomicBool::new(in_transaction_flag(&conn).unwrap_or(false)),
            conn: tokio::sync::Mutex::new(Some(conn)),
            config: config.clone(),
            lost: AtomicBool::new(false),
            lost_reason: Mutex::new(None),
            interrupted: AtomicBool::new(false),
        })
    }

    /// Keeps the server's in-transaction flag after a statement. After a failure the last packet
    /// was an ERR, which carries no flags: they are read with `STATUS_PROBE`, and when that fails
    /// too, the flag stays as it was.
    async fn note_status(&self, conn: &mut Conn) {
        if conn.last_ok_packet().is_none()
            && !self.lost.load(Ordering::SeqCst)
            && let Err(e) = conn.query_drop(STATUS_PROBE).await
        {
            self.note_error(&e);
        }
        if let Some(open) = in_transaction_flag(conn) {
            self.in_transaction.store(open, Ordering::SeqCst);
        }
    }

    /// A client or IO error ends the session, with its message as the reason.
    fn note_error(&self, e: &mysql_async::Error) {
        if !matches!(e, mysql_async::Error::Server(_)) {
            self.lost.store(true, Ordering::SeqCst);
            let mut reason = self.lost_reason.lock().unwrap_or_else(PoisonError::into_inner);
            reason.get_or_insert_with(|| error_message(e));
        }
    }

    /// Stops the statement still running on the connection (its rest isn't wanted). False when
    /// the KILL couldn't be sent: the rest must then be read to the end.
    async fn stop_rest(&self) -> bool {
        match kill_query(&self.config, self.id).await {
            Ok(()) => true,
            Err(e) => {
                eprintln!("couldn't stop the rest of MySQL connection {}'s result: {}", self.id, e.message());
                false
            }
        }
    }
}

/// The in-transaction flag of the connection's last OK packet; `None` when the last packet was
/// an ERR.
fn in_transaction_flag(conn: &Conn) -> Option<bool> {
    conn.last_ok_packet().map(|ok| ok.status_flags().contains(StatusFlags::SERVER_STATUS_IN_TRANS))
}

/// A failure as `RawOutcome::Error`: a statement stopped by `KILL QUERY` has `CANCELED_CODE`,
/// any other server error `MYSQL_{errno}` (a syntax error with its position in `sql`), and a
/// client or IO error no code.
fn error_outcome(e: &mysql_async::Error, sql: &str) -> RawOutcome {
    match e {
        mysql_async::Error::Server(s) => RawOutcome::Error {
            message: s.message.clone(),
            code: Some(if s.code == ER_QUERY_INTERRUPTED { CANCELED_CODE.to_string() } else { format!("MYSQL_{}", s.code) }),
            position: if s.code == ER_PARSE_ERROR { syntax_error_position(&s.message, sql) } else { None },
        },
        other => RawOutcome::Error { message: error_message(other), code: None, position: None },
    }
}

/// Where a syntax error is in `sql`, as a 1-based char position: the server quotes the text from
/// the error on (`… near 'frm t' at line 1`), cut short when long. It is searched from the start
/// of the line the server names. Nothing quoted (the statement ended too soon) is the statement's
/// length; quoted text that isn't found is `None`.
fn syntax_error_position(message: &str, sql: &str) -> Option<usize> {
    let start = message.find("near '")? + "near '".len();
    let end = message.rfind("' at line ")?;
    let line: usize = message[end + "' at line ".len()..].trim_end().parse().ok()?;
    let near = message.get(start..end)?;
    // A quote cut in the middle of a character ends in U+FFFD.
    let near = near.trim_end_matches('\u{fffd}');
    if near.is_empty() {
        return Some(sql.chars().count());
    }
    let chars: Vec<char> = sql.chars().collect();
    let line_start = match line {
        0 | 1 => 0,
        n => chars.iter().enumerate().filter(|(_, c)| **c == '\n').nth(n - 2).map(|(i, _)| i + 1)?,
    };
    let rest: String = chars[line_start..].iter().collect();
    let found = rest.find(near)?;
    Some(line_start + rest[..found].chars().count() + 1)
}

/// Each result column's name and type.
fn result_columns(columns: &[Column]) -> Vec<ResultColumn> {
    columns
        .iter()
        .map(|c| ResultColumn { name: c.name_str().into_owned(), data_type: values::type_name(c) })
        .collect()
}

/// A row's text values and its cell type codes.
pub(super) fn row_cells(row: &Row) -> (Vec<Option<String>>, String) {
    let columns = row.columns_ref();
    let (texts, types): (Vec<Option<String>>, Vec<CellType>) =
        (0..row.len()).map(|i| values::cell(row.as_ref(i).unwrap_or(&Value::NULL), &columns[i])).unzip();
    (texts, row_types(&types))
}

/// The notice for a procedure's result sets after the first.
fn more_sets_notice(n: usize) -> Notice {
    Notice { severity: "INFO".into(), message: format!("The procedure returned {n} more result sets; only the first is shown.") }
}

/// Reads the result sets after the current one (a procedure's) to the end, and counts those with
/// columns: a CALL ends with an OK packet of its own, which isn't one.
async fn skip_more_sets(result: &mut QueryResult<'_, 'static, TextProtocol>) -> mysql_async::Result<usize> {
    let mut sets = 0;
    while !result.is_empty() {
        if !result.columns_ref().is_empty() {
            sets += 1;
        }
        while result.next().await?.is_some() {}
    }
    Ok(sets)
}

/// What a statement gave before its notices are read.
struct Ran {
    outcome: RawOutcome,
    /// Read from the statement's last OK packet; 0 and empty when it wasn't read to the end.
    warnings: u16,
    info: String,
    extra_sets: usize,
}

/// The statement's warnings (`SHOW WARNINGS`, run right after it so they are still there), its
/// `info` text, and a procedure's extra result sets, as notices.
async fn read_notices(conn: &mut Conn, ran: &Ran) -> mysql_async::Result<Vec<Notice>> {
    let mut notices = Vec::new();
    if ran.warnings > 0 {
        // Level, Code, Message; read as bytes, so a message in another encoding can't fail the read.
        let warnings: Vec<Row> = conn.query("SHOW WARNINGS").await?;
        let text = |row: &Row, i: usize| match row.as_ref(i) {
            Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        };
        notices.extend(warnings.iter().map(|w| Notice { severity: text(w, 0).to_uppercase(), message: text(w, 2) }));
    }
    if !ran.info.is_empty() {
        notices.push(Notice { severity: "INFO".into(), message: ran.info.clone() });
    }
    if ran.extra_sets > 0 {
        notices.push(more_sets_notice(ran.extra_sets));
    }
    Ok(notices)
}

impl MysqlQuerySession {
    /// Runs one statement: up to `cap` rows of a result with columns, `truncated` when there is
    /// one more, or the affected rows. Past the cap a read is stopped with KILL QUERY; any other
    /// statement runs to its end, its further rows dropped.
    async fn run_statement(&self, conn: &mut Conn, sql: &str, values: &[Option<String>], cap: usize) -> mysql_async::Result<Ran> {
        let mut result = text::query_text(conn, sql, values).await?;
        let columns = result.columns().filter(|c| !c.is_empty());
        let Some(columns) = columns else {
            let extra_sets = skip_more_sets(&mut result).await?;
            let ran = Ran {
                outcome: RawOutcome::Command { rows_affected: result.affected_rows() },
                warnings: result.warnings(),
                info: result.info().into_owned(),
                extra_sets,
            };
            result.drop_result().await?;
            return Ok(ran);
        };
        let (mut rows, mut types) = (Vec::new(), Vec::new());
        let mut truncated = false;
        while let Some(row) = result.next().await? {
            if rows.len() == cap {
                truncated = true;
                break;
            }
            let (texts, row_types) = row_cells(&row);
            rows.push(texts);
            types.push(row_types);
        }
        let outcome =
            RawOutcome::Rows { columns: result_columns(&columns), rows, truncated, cell_types: Some(types) };
        if truncated && classify_in(SqlDialect::Mysql, sql).0 == StatementKind::Read {
            // The rest of a read may be long and nobody wants it: it is stopped, then read to the
            // end of what was sent. A statement that had already finished reads to its end as usual.
            self.stop_rest().await;
            match result.drop_result().await {
                Ok(()) => {}
                Err(mysql_async::Error::Server(e)) if e.code == ER_QUERY_INTERRUPTED => {}
                Err(e) => return Err(e),
            }
            // Its warnings and info are those of a statement that didn't run to its end.
            return Ok(Ran { outcome, warnings: 0, info: String::new(), extra_sets: 0 });
        }
        if truncated {
            // Anything else (a CALL, a `… RETURNING` write) runs to its end, as on Postgres:
            // stopping it would roll its changes back. Its rows past the cap are read and dropped.
            while result.next().await?.is_some() {}
        }
        let extra_sets = skip_more_sets(&mut result).await?;
        let ran = Ran { outcome, warnings: result.warnings(), info: result.info().into_owned(), extra_sets };
        result.drop_result().await?;
        Ok(ran)
    }

    /// Streams every row of `sql` to `sink`. When the sink fails, the statement is stopped before
    /// its result is read to the end.
    async fn stream_statement(
        &self,
        conn: &mut Conn,
        sql: &str,
        values: &[Option<String>],
        sink: &mut dyn RowSink,
    ) -> Result<(), StreamError> {
        let mut result = text::query_text(conn, sql, values).await.map_err(StreamError::Db)?;
        let columns = result.columns().filter(|c| !c.is_empty());
        let Some(columns) = columns else {
            result.drop_result().await.map_err(StreamError::Db)?;
            return Err(StreamError::Sink(AppError::Query("This statement returns no rows to export.".into())));
        };
        let export_columns: Vec<ExportColumn> =
            result_columns(&columns).into_iter().map(|c| ExportColumn { name: c.name, data_type: c.data_type }).collect();
        let mut delivered = sink.begin(&export_columns);
        while delivered.is_ok() {
            let Some(row) = result.next().await.map_err(StreamError::Db)? else { break };
            let (texts, types) = row_cells(&row);
            delivered = sink.typed_row(texts, Some(&types));
        }
        if let Err(e) = delivered {
            self.stop_rest().await;
            match result.drop_result().await {
                Ok(()) => {}
                Err(mysql_async::Error::Server(s)) if s.code == ER_QUERY_INTERRUPTED => {}
                Err(db) => self.note_error(&db),
            }
            return Err(StreamError::Sink(e));
        }
        result.drop_result().await.map_err(StreamError::Db)
    }
}

enum StreamError {
    Db(mysql_async::Error),
    Sink(AppError),
}

#[async_trait]
impl QuerySession for MysqlQuerySession {
    async fn run(&self, sql: &str, params: &[Option<String>], row_cap: usize) -> StatementRun {
        let mut slot = self.conn.lock().await;
        let Some(conn) = slot.as_mut() else {
            return StatementRun {
                outcome: RawOutcome::Error { message: SESSION_CLOSED.into(), code: None, position: None },
                notices: Vec::new(),
                elapsed_ms: 0,
            };
        };
        let busy = self.busy();
        let started = Instant::now();
        let ran = self.run_statement(conn, sql, params, row_cap).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (outcome, notices) = match ran {
            Ok(ran) => {
                let notices = match read_notices(conn, &ran).await {
                    Ok(n) => n,
                    Err(e) => {
                        self.note_error(&e);
                        eprintln!("couldn't read the statement's warnings: {}", error_message(&e));
                        Vec::new()
                    }
                };
                (ran.outcome, notices)
            }
            Err(e) => {
                self.note_error(&e);
                (error_outcome(&e, sql), Vec::new())
            }
        };
        self.note_status(conn).await;
        busy.finish();
        StatementRun { outcome, notices, elapsed_ms }
    }

    /// Every row's text, without columns or cell types; a statement without columns is a
    /// `Command`.
    async fn run_simple(&self, sql: &str) -> RawOutcome {
        let mut slot = self.conn.lock().await;
        let Some(conn) = slot.as_mut() else {
            return RawOutcome::Error { message: SESSION_CLOSED.into(), code: None, position: None };
        };
        let busy = self.busy();
        let ran = match self.run_statement(conn, sql, &[], usize::MAX).await {
            Ok(ran) => match ran.outcome {
                RawOutcome::Rows { rows, .. } => RawOutcome::Rows { columns: Vec::new(), rows, truncated: false, cell_types: None },
                other => other,
            },
            Err(e) => {
                self.note_error(&e);
                error_outcome(&e, sql)
            }
        };
        self.note_status(conn).await;
        busy.finish();
        ran
    }

    async fn stream_rows(&self, sql: &str, params: &[Option<String>], sink: &mut dyn RowSink) -> AppResult<()> {
        let mut slot = self.conn.lock().await;
        let Some(conn) = slot.as_mut() else {
            return Err(AppError::Connection(SESSION_CLOSED.into()));
        };
        let busy = self.busy();
        let streamed = self.stream_statement(conn, sql, params, sink).await;
        let result = match streamed {
            Ok(()) => Ok(()),
            Err(StreamError::Sink(e)) => Err(e),
            Err(StreamError::Db(e)) => {
                self.note_error(&e);
                Err(AppError::Query(error_message(&e)))
            }
        };
        self.note_status(conn).await;
        busy.finish();
        result
    }

    async fn in_transaction(&self) -> AppResult<bool> {
        Ok(self.in_transaction.load(Ordering::SeqCst))
    }

    /// A failed statement doesn't abort a MySQL transaction.
    async fn in_failed_transaction(&self) -> bool {
        false
    }

    /// Outside a transaction there is nothing to protect (and the server would forget the
    /// savepoint at once).
    async fn export_savepoint(&self) -> AppResult<bool> {
        if !self.in_transaction.load(Ordering::SeqCst) {
            return Ok(false);
        }
        match self.run_simple(&format!("SAVEPOINT {EXPORT_SAVEPOINT}")).await {
            RawOutcome::Error { message, .. } => Err(AppError::Query(message)),
            _ => Ok(true),
        }
    }

    /// `KILL QUERY` from a second connection: whatever runs on the tab's connection fails with
    /// errno 1317.
    async fn cancel(&self) -> AppResult<()> {
        kill_query(&self.config, self.id).await
    }

    async fn tx_status(&self) -> TxStatus {
        if self.in_transaction.load(Ordering::SeqCst) { TxStatus::Open } else { TxStatus::Idle }
    }

    fn is_closed(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    fn lost_reason(&self) -> Option<String> {
        self.lost_reason.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// `USE` the database; `None` goes back to the one the session had right after setup (with
    /// none, there is nothing to go back to).
    async fn set_search_path(&self, schema: Option<&str>) -> AppResult<()> {
        let Some(database) = schema.or(self.initial_database.as_deref()) else {
            return Ok(());
        };
        let mut slot = self.conn.lock().await;
        let conn = slot.as_mut().ok_or_else(|| AppError::Connection(SESSION_CLOSED.into()))?;
        let busy = self.busy();
        let used = conn.query_drop(format!("USE {}", MysqlDialect.quote_ident(database))).await;
        if let Err(e) = &used {
            self.note_error(e);
        }
        self.note_status(conn).await;
        busy.finish();
        used.map_err(|e| AppError::Query(error_message(&e)))
    }

    /// A failed `USE` fails the run: the statement would otherwise run on the
    /// database the tab was on, while the chip names another.
    fn schema_switch_fails_run(&self) -> bool {
        true
    }

    fn probe_timeout(&self) -> Duration {
        self.config.timeout()
    }

    /// Closes the connection (an open transaction rolls back as it closes), waiting up to
    /// `CLOSE_WAIT` for it. An idle session (no command holds the connection, and none was
    /// dropped mid-way) just closes: no second connection is opened. Otherwise what runs is
    /// stopped first (nobody waits for it once the tab closes).
    async fn close(&self) {
        self.lost.store(true, Ordering::SeqCst);
        let idle = match self.conn.try_lock() {
            Ok(slot) if !self.interrupted.load(Ordering::SeqCst) => Some(slot),
            _ => None,
        };
        let mut slot = match idle {
            Some(slot) => slot,
            None => {
                if let Err(e) = kill_query(&self.config, self.id).await {
                    eprintln!("couldn't stop MySQL connection {}'s statement at close: {}", self.id, e.message());
                }
                let Ok(slot) = tokio::time::timeout(CLOSE_WAIT, self.conn.lock()).await else {
                    eprintln!(
                        "a query tab's MySQL connection {} is still busy after close; it closes when its work ends",
                        self.id
                    );
                    return;
                };
                slot
            }
        };
        if let Some(conn) = slot.take() {
            match tokio::time::timeout(CLOSE_WAIT, conn.disconnect()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("closing MySQL connection {} failed: {}", self.id, error_message(&e)),
                Err(_) => eprintln!("closing MySQL connection {} took over {CLOSE_WAIT:?}; dropped", self.id),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::ServerError;

    fn server_error(code: u16, message: &str) -> mysql_async::Error {
        mysql_async::Error::Server(ServerError { code, message: message.into(), state: "42000".into() })
    }

    const SYNTAX: &str = "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server \
                          version for the right syntax to use near";

    #[test]
    fn mysql_syntax_error_positions() {
        let at = |near: &str, line: usize, sql: &str| syntax_error_position(&format!("{SYNTAX} '{near}' at line {line}"), sql);
        assert_eq!(at("frm users", 1, "select * frm users"), Some(10));
        // Searched from the start of the named line, in chars.
        assert_eq!(at("frm t", 2, "select 'frm t',\n 'é' frm t"), Some(22));
        // The quoted text may hold quotes of its own.
        assert_eq!(at("'a' 'b'", 1, "select 1 'a' 'b'"), Some(10));
        // Nothing quoted: the statement's length.
        assert_eq!(at("", 1, "select * from"), Some(13));
        // Cut in the middle of a character.
        assert_eq!(at("frm \u{fffd}", 1, "select 1 frm é"), Some(10));
        // Not found, or no line.
        assert_eq!(at("nope", 1, "select 1"), None);
        assert_eq!(at("x", 3, "select x"), None);
        assert_eq!(syntax_error_position("Unknown column", "select 1"), None);
    }

    #[test]
    fn mysql_error_codes() {
        let e = error_outcome(&server_error(1317, "Query execution was interrupted"), "select 1");
        assert_eq!(
            e,
            RawOutcome::Error { message: "Query execution was interrupted".into(), code: Some(CANCELED_CODE.into()), position: None }
        );
        let e = error_outcome(&server_error(1146, "Table 'fx.nope' doesn't exist"), "select * from nope");
        assert!(matches!(e, RawOutcome::Error { code: Some(c), position: None, .. } if c == "MYSQL_1146"));
        let e = error_outcome(&server_error(1064, &format!("{SYNTAX} 'frm t' at line 1")), "select * frm t");
        assert!(matches!(e, RawOutcome::Error { code: Some(c), position: Some(10), .. } if c == "MYSQL_1064"));
        let io = mysql_async::Error::Io(mysql_async::IoError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)));
        assert!(matches!(error_outcome(&io, "select 1"), RawOutcome::Error { code: None, position: None, .. }));
    }

    #[test]
    fn mysql_more_sets_notice_copy() {
        assert_eq!(more_sets_notice(1).message, "The procedure returned 1 more result sets; only the first is shown.");
    }
}
