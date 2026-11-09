//! A query tab's own Postgres connection (tokio-postgres), separate from the
//! browsing pool so a tab can hold a transaction, be canceled and stream notices.

use std::error::Error as StdError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::BytesMut;
use futures_util::{pin_mut, StreamExt, TryStreamExt};
use sqlx::PgPool;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::task::JoinHandle;
use tokio_postgres::error::{ErrorPosition, Severity};
use tokio_postgres::types::{to_sql_checked, Format, FromSql, IsNull, Kind, ToSql, Type};
use tokio_postgres::{AsyncMessage, CancelToken, Client, Row, SimpleQueryMessage, Statement};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::pg_tls::{pg_ssl_mode, tls_connector};
use super::{Dialect, PostgresDialect};
use crate::error::{AppError, AppResult};
use crate::model::ConnectionProfile;
use crate::export::{ExportColumn, RowSink};
use crate::query::{Notice, QuerySession, RawOutcome, ResultColumn, StatementRun, TxStatus};
use crate::sqltext::{classify, leading_keyword, StatementKind};

/// Everything needed to open query sessions for one connection profile.
#[derive(Clone)]
pub struct SessionConfig {
    pg: tokio_postgres::Config,
    tls: MakeRustlsConnect,
    startup_sql: Option<String>,
    read_only: bool,
    timeout_secs: u32,
    host: String,
    port: u16,
}

/// Builds the session settings from a profile. Fails only on TLS setup (e.g. an
/// unreadable CA file).
pub fn session_config(profile: &ConnectionProfile, password: Option<String>) -> AppResult<SessionConfig> {
    let net = profile.network()?;
    let mut pg = tokio_postgres::Config::new();
    pg.host(&net.host)
        .port(net.port)
        .user(&net.user)
        .dbname(&net.database)
        .application_name("Porthole")
        .ssl_mode(pg_ssl_mode(&net.ssl_mode));
    if let Some(pw) = password {
        pg.password(pw);
    }
    Ok(SessionConfig {
        pg,
        tls: tls_connector(&net.ssl_mode, net.ssl_root_cert.as_deref())?,
        startup_sql: profile.startup_sql.clone().filter(|s| !s.trim().is_empty()),
        read_only: profile.read_only,
        timeout_secs: profile.connect_timeout_secs.max(1),
        host: net.host.clone(),
        port: net.port,
    })
}

impl SessionConfig {
    /// The same settings (server, credentials, TLS, startup SQL, read-only) for another
    /// database on the same server.
    pub fn for_database(&self, db: &str) -> SessionConfig {
        let mut cfg = self.clone();
        cfg.pg.dbname(db);
        cfg
    }
}

const SESSION_CLOSED: &str = "The query session is closed";

pub struct PgQuerySession {
    /// `None` after `close`. Runs clone the `Arc` so the lock is never held across an await.
    client: Mutex<Option<Arc<Client>>>,
    connection: JoinHandle<()>,
    /// Set when a statement failed in a way that ends the connection (FATAL, closed
    /// socket). The connection task notices a moment later; this makes
    /// `is_closed` true as soon as `run` returns.
    lost: AtomicBool,
    /// Why the connection was lost, when known. See `record_reason`.
    lost_reason: Arc<Mutex<Option<String>>>,
    notices: Mutex<UnboundedReceiver<Notice>>,
    cancel_token: CancelToken,
    tls: MakeRustlsConnect,
    /// The browsing pool, used to read this backend's state from outside.
    pool: PgPool,
    backend_pid: i32,
    /// `search_path` right after setup, restored by `set_search_path(None)`.
    initial_search_path: String,
    /// The profile's connect timeout; bounds quick checks such as the transaction probe.
    probe_timeout: Duration,
}

impl PgQuerySession {
    /// Connects and runs the same setup as the pool: startup SQL, then read-only mode.
    pub async fn open(cfg: &SessionConfig, pool: PgPool) -> AppResult<PgQuerySession> {
        let timeout = Duration::from_secs(cfg.timeout_secs.into());
        let (client, mut connection) = match tokio::time::timeout(timeout, cfg.pg.connect(cfg.tls.clone())).await {
            Err(_) => {
                return Err(AppError::Connection(format!(
                    "Timed out after {} s connecting to {}:{}",
                    cfg.timeout_secs, cfg.host, cfg.port
                )));
            }
            Ok(Err(e)) => return Err(AppError::Connection(error_message(&e))),
            Ok(Ok(pair)) => pair,
        };

        let (notice_tx, notice_rx) = unbounded_channel();
        let lost_reason = Arc::new(Mutex::new(None));
        let task_reason = lost_reason.clone();
        let connection_task = tokio::spawn(async move {
            let mut messages = futures_util::stream::poll_fn(move |cx| connection.poll_message(cx));
            // Ends on a connection error or once every client handle is gone.
            while let Some(message) = messages.next().await {
                match message {
                    Ok(AsyncMessage::Notice(n)) => {
                        // `parsed_severity` is the untranslated keyword; `severity` follows lc_messages.
                        let severity = n.parsed_severity().map_or_else(|| n.severity().to_string(), |s| s.to_string());
                        let notice = Notice { severity, message: n.message().to_string() };
                        // The receiver only goes away with the session.
                        let _ = notice_tx.send(notice);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let reason = error_message(&e);
                        eprintln!("query session connection ended: {reason}");
                        record_reason(&task_reason, reason, false);
                        break;
                    }
                }
            }
        });

        let setup = async {
            if let Some(sql) = &cfg.startup_sql {
                client.batch_execute(sql).await?;
            }
            if cfg.read_only {
                client.batch_execute("SET default_transaction_read_only = on").await?;
            }
            let row = client.query_one("SELECT pg_backend_pid(), current_setting('search_path')", &[]).await?;
            Ok::<_, tokio_postgres::Error>((row.try_get::<_, i32>(0)?, row.try_get::<_, String>(1)?))
        };
        let (backend_pid, initial_search_path) = match setup.await {
            Ok(v) => v,
            Err(e) => {
                // Dropping the only client ends the connection task.
                drop(client);
                return Err(AppError::Connection(error_message(&e)));
            }
        };

        let session = PgQuerySession {
            cancel_token: client.cancel_token(),
            client: Mutex::new(Some(Arc::new(client))),
            connection: connection_task,
            lost: AtomicBool::new(false),
            lost_reason,
            notices: Mutex::new(notice_rx),
            tls: cfg.tls.clone(),
            pool,
            backend_pid,
            initial_search_path,
            probe_timeout: Duration::from_secs(cfg.timeout_secs.into()),
        };
        // Notices raised by the startup SQL must not attach to the first statement.
        session.drain_notices();
        Ok(session)
    }

    fn client_slot(&self) -> MutexGuard<'_, Option<Arc<Client>>> {
        self.client.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn client(&self) -> Option<Arc<Client>> {
        self.client_slot().clone()
    }

    fn drain_notices(&self) -> Vec<Notice> {
        let mut rx = self.notices.lock().unwrap_or_else(PoisonError::into_inner);
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// A failure that ends the connection marks the session lost, with the reason.
    fn note_failure(&self, failure: &Failure) {
        if failure.ends_session() {
            self.lost.store(true, Ordering::SeqCst);
            let is_server_error = failure.error.as_db_error().is_some();
            record_reason(&self.lost_reason, error_message(&failure.error), is_server_error);
        }
    }

    /// One read of this backend's state from `pg_stat_activity` (through the pool).
    async fn read_tx_status(&self) -> Result<TxStatus, sqlx::Error> {
        let state = sqlx::query_scalar::<_, Option<String>>("SELECT state FROM pg_stat_activity WHERE pid = $1")
            .bind(self.backend_pid)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match state.flatten().as_deref() {
            Some("idle in transaction") => TxStatus::Open,
            Some("idle in transaction (aborted)") => TxStatus::Failed,
            Some("idle") => TxStatus::Idle,
            _ => TxStatus::Unknown,
        })
    }
}

/// Keeps why the connection was lost. A session-ending server error from a statement
/// (`overwrite`) is the most specific reason and always wins; the connection task's
/// error (often just "connection closed" after it) is kept only when none is known yet.
fn record_reason(slot: &Mutex<Option<String>>, reason: String, overwrite: bool) {
    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if overwrite || slot.is_none() {
        *slot = Some(reason);
    }
}

#[async_trait]
impl QuerySession for PgQuerySession {
    async fn run(&self, sql: &str, params: &[Option<String>], row_cap: usize) -> StatementRun {
        let started = Instant::now();
        let outcome = match self.client() {
            Some(client) => match run_statement(&client, sql, params, row_cap).await {
                Ok(outcome) => outcome,
                Err(failure) => {
                    self.note_failure(&failure);
                    failure.into_outcome()
                }
            },
            None => RawOutcome::Error { message: SESSION_CLOSED.into(), code: None, position: None },
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        // The connection task handles server messages in order and queues a notice
        // before it delivers the completion that follows it, so the statement's
        // notices are already in the channel when its result arrives here. The yield
        // only lets the task run once more; nothing depends on it.
        tokio::task::yield_now().await;
        StatementRun { outcome, notices: self.drain_notices(), elapsed_ms }
    }

    async fn run_simple(&self, sql: &str) -> RawOutcome {
        let Some(client) = self.client() else {
            return RawOutcome::Error { message: SESSION_CLOSED.into(), code: None, position: None };
        };
        let outcome = match client.simple_query(sql).await {
            Ok(messages) => {
                let rows = messages
                    .iter()
                    .filter_map(|m| match m {
                        SimpleQueryMessage::Row(row) => {
                            Some((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect())
                        }
                        _ => None,
                    })
                    .collect();
                RawOutcome::Rows { columns: vec![], rows, truncated: false, cell_types: None }
            }
            Err(e) => {
                let failure = Failure::new(e, |_| None);
                self.note_failure(&failure);
                failure.into_outcome()
            }
        };
        // Its notices (if any) must not attach to the next statement.
        tokio::task::yield_now().await;
        self.drain_notices();
        outcome
    }

    async fn stream_rows(&self, sql: &str, params: &[Option<String>], sink: &mut dyn RowSink) -> AppResult<()> {
        let Some(client) = self.client() else {
            return Err(AppError::Connection(SESSION_CLOSED.into()));
        };
        let result = stream_statement(&client, sql, params, sink).await;
        // Like `run`: the statement's notices must not attach to the next statement.
        tokio::task::yield_now().await;
        self.drain_notices();
        match result {
            Ok(()) => Ok(()),
            Err(StreamError::Sink(e)) => Err(e),
            Err(StreamError::Db(e)) => {
                let failure = Failure::new(e, |_| None);
                self.note_failure(&failure);
                Err(AppError::Query(error_message(&failure.error)))
            }
        }
    }

    async fn cancel(&self) -> AppResult<()> {
        self.cancel_token
            .cancel_query(self.tls.clone())
            .await
            .map_err(|e| AppError::Connection(error_message(&e)))
    }

    /// Read from another connection; a failed read is logged and tried once more
    /// before the state is reported as Unknown (which the UI asks about).
    async fn tx_status(&self) -> TxStatus {
        for attempt in 1..=2 {
            match self.read_tx_status().await {
                Ok(status) => return status,
                Err(e) => eprintln!("could not read the transaction state (attempt {attempt}): {e}"),
            }
        }
        TxStatus::Unknown
    }

    fn is_closed(&self) -> bool {
        self.lost.load(Ordering::SeqCst) || self.client_slot().as_ref().is_none_or(|c| c.is_closed())
    }

    fn lost_reason(&self) -> Option<String> {
        self.lost_reason.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    async fn set_search_path(&self, schema: Option<&str>) -> AppResult<()> {
        let value = match schema {
            Some(s) => {
                let quoted = PostgresDialect.quote_ident(s);
                if s == "public" { quoted } else { format!("{quoted}, public") }
            }
            None => self.initial_search_path.clone(),
        };
        let client = self.client().ok_or_else(|| AppError::Connection(SESSION_CLOSED.into()))?;
        client
            .query_one("SELECT set_config('search_path', $1, false)", &[&value])
            .await
            .map(drop)
            .map_err(|e| AppError::Query(error_message(&e)))
    }

    fn probe_timeout(&self) -> Duration {
        self.probe_timeout
    }

    async fn close(&self) {
        let client = self.client_slot().take();
        drop(client);
        self.connection.abort();
    }
}

/// Server errors become their message; anything else keeps its display text.
fn error_message(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db.message().to_string(),
        None => e.to_string(),
    }
}

/// A failed statement. `position` is already relative to the statement the
/// caller ran (1-based chars).
struct Failure {
    error: tokio_postgres::Error,
    position: Option<usize>,
}

impl Failure {
    /// `map_position` turns the server's 1-based char position into one relative
    /// to the caller's statement, or `None` when it falls outside it.
    fn new(error: tokio_postgres::Error, map_position: impl Fn(usize) -> Option<usize>) -> Failure {
        let position = match error.as_db_error().and_then(|db| db.position()) {
            Some(ErrorPosition::Original(p)) => map_position(*p as usize),
            _ => None,
        };
        Failure { error, position }
    }

    /// The server ends the connection after FATAL and PANIC errors.
    fn ends_session(&self) -> bool {
        match self.error.as_db_error() {
            Some(db) => matches!(db.parsed_severity(), Some(Severity::Fatal | Severity::Panic)),
            None => self.error.is_closed(),
        }
    }

    fn into_outcome(self) -> RawOutcome {
        let code = self.error.code().map(|c| c.code().to_string());
        RawOutcome::Error { message: error_message(&self.error), code, position: self.position }
    }
}

async fn run_statement(
    client: &Client,
    sql: &str,
    params: &[Option<String>],
    row_cap: usize,
) -> Result<RawOutcome, Failure> {
    let params: Vec<TextParam> = params.iter().map(|p| TextParam(p.as_deref())).collect();
    let stmt = client.prepare(sql).await.map_err(|e| Failure::new(e, Some))?;
    if stmt.columns().is_empty() {
        let rows_affected = client.execute_raw(&stmt, params.iter()).await.map_err(|e| Failure::new(e, Some))?;
        return Ok(RawOutcome::Command { rows_affected });
    }
    let columns: Vec<ResultColumn> = stmt
        .columns()
        .iter()
        .map(|c| ResultColumn { name: c.name().to_string(), data_type: c.type_().name().to_string() })
        .collect();

    if !wrappable(sql) {
        return fetch_unwrapped(client, &stmt, &params, row_cap, columns).await;
    }

    let wrapped = TextWrapper::new(sql, columns.len(), Some(row_cap.saturating_add(1)));
    let wrapped_stmt = match client.prepare(&wrapped.sql).await {
        Ok(s) => s,
        // Not expected for the statements `wrappable` admits. Inside a transaction
        // the failed prepare has already aborted it, and the fallback reports that.
        Err(e) => {
            eprintln!("could not prepare the wrapped statement, reading it unwrapped: {}", error_message(&e));
            return fetch_unwrapped(client, &stmt, &params, row_cap, columns).await;
        }
    };
    let (rows, truncated) =
        fetch_rows(client, &wrapped_stmt, &params, row_cap, |row, i| row.try_get::<_, Option<String>>(i))
            .await
            .map_err(|e| Failure::new(e, |p| wrapped.position_in_body(p)))?;
    Ok(RawOutcome::Rows { columns, rows, truncated, cell_types: None })
}

/// A statement wrapped in a CTE so the server casts every value to text, the way the grid
/// shows it: `WITH ts_r(c1,..) AS (<statement>) SELECT c1::text, .. FROM ts_r [LIMIT n]`.
struct TextWrapper {
    sql: String,
    prefix_len: usize,
    body_len: usize,
}

impl TextWrapper {
    fn new(sql: &str, columns: usize, limit: Option<usize>) -> TextWrapper {
        let body = sql.trim_end_matches(|c: char| c.is_whitespace() || c == ';');
        let aliases: Vec<String> = (1..=columns).map(|i| format!("c{i}")).collect();
        let prefix = format!("WITH ts_r({}) AS (\n", aliases.join(","));
        let select_list: Vec<String> = aliases.iter().map(|a| format!("{a}::text")).collect();
        let mut wrapped = format!("{prefix}{body}\n) SELECT {} FROM ts_r", select_list.join(", "));
        if let Some(n) = limit {
            wrapped.push_str(&format!(" LIMIT {n}"));
        }
        TextWrapper { sql: wrapped, prefix_len: prefix.chars().count(), body_len: body.chars().count() }
    }

    /// The server's 1-based char position in the wrapped text, as a position in the
    /// statement, or `None` when it falls in the wrapper.
    fn position_in_body(&self, p: usize) -> Option<usize> {
        p.checked_sub(self.prefix_len).filter(|p| (1..=self.body_len).contains(p))
    }
}

/// Streams every row of a read statement into `sink`, as the same text `run_statement` gives.
async fn stream_statement(
    client: &Client,
    sql: &str,
    params: &[Option<String>],
    sink: &mut dyn RowSink,
) -> Result<(), StreamError> {
    let params: Vec<TextParam> = params.iter().map(|p| TextParam(p.as_deref())).collect();
    let stmt = client.prepare(sql).await.map_err(StreamError::Db)?;
    let columns: Vec<ExportColumn> = stmt
        .columns()
        .iter()
        .map(|c| ExportColumn { name: c.name().to_string(), data_type: c.type_().name().to_string() })
        .collect();
    if columns.is_empty() {
        return Err(StreamError::Sink(AppError::Query("This statement returns no rows to export.".into())));
    }
    // Same choice as `run_statement`: the wrapper only for statements it accepts, decided
    // from the text, so a failed wrapped prepare never aborts the user's transaction.
    let wrapped = if wrappable(sql) {
        let w = TextWrapper::new(sql, columns.len(), None);
        Some(client.prepare(&w.sql).await.map_err(StreamError::Db)?)
    } else {
        None
    };
    sink.begin(&columns).map_err(StreamError::Sink)?;
    let stream = client.query_raw(wrapped.as_ref().unwrap_or(&stmt), params.iter()).await.map_err(StreamError::Db)?;
    pin_mut!(stream);
    while let Some(row) = stream.try_next().await.map_err(StreamError::Db)? {
        let values = if wrapped.is_some() {
            (0..row.len()).map(|i| row.try_get::<_, Option<String>>(i)).collect::<Result<Vec<_>, _>>()
        } else {
            (0..row.len())
                .map(|i| row.try_get::<_, Option<AnyText>>(i).map(|v| v.map(|t| t.0)))
                .collect::<Result<Vec<_>, _>>()
        };
        sink.row(values.map_err(StreamError::Db)?).map_err(StreamError::Sink)?;
    }
    Ok(())
}

enum StreamError {
    Db(tokio_postgres::Error),
    Sink(AppError),
}

/// Whether `sql` can be wrapped in `WITH ts_r(...) AS (...)`. Decided from the
/// text, before anything reaches the server: a wrapped prepare that fails
/// (EXPLAIN, SHOW, data-modifying WITH) would abort the user's open transaction.
fn wrappable(sql: &str) -> bool {
    let (kind, verb) = classify(sql);
    if leading_keyword(sql).as_deref() == Some("WITH") {
        return kind == StatementKind::Read;
    }
    matches!(verb.as_str(), "SELECT" | "VALUES" | "TABLE" | "INSERT" | "UPDATE" | "DELETE" | "MERGE")
}

/// Runs the statement itself and turns its binary values into text here.
async fn fetch_unwrapped(
    client: &Client,
    stmt: &Statement,
    params: &[TextParam<'_>],
    row_cap: usize,
    columns: Vec<ResultColumn>,
) -> Result<RawOutcome, Failure> {
    let (rows, truncated) =
        fetch_rows(client, stmt, params, row_cap, |row, i| row.try_get::<_, Option<AnyText>>(i).map(|v| v.map(|t| t.0)))
            .await
            .map_err(|e| Failure::new(e, Some))?;
    Ok(RawOutcome::Rows { columns, rows, truncated, cell_types: None })
}

type Rows = Vec<Vec<Option<String>>>;

/// Reads up to `row_cap` rows; `truncated` when one more exists. Dropping the
/// stream early leaves the client usable (the rest is drained in the background).
async fn fetch_rows(
    client: &Client,
    stmt: &Statement,
    params: &[TextParam<'_>],
    row_cap: usize,
    decode: impl Fn(&Row, usize) -> Result<Option<String>, tokio_postgres::Error>,
) -> Result<(Rows, bool), tokio_postgres::Error> {
    let stream = client.query_raw(stmt, params.iter()).await?;
    pin_mut!(stream);
    let mut rows = Vec::new();
    while let Some(row) = stream.try_next().await? {
        if rows.len() == row_cap {
            return Ok((rows, true));
        }
        rows.push((0..row.len()).map(|i| decode(&row, i)).collect::<Result<Vec<_>, _>>()?);
    }
    Ok((rows, false))
}

/// A parameter sent in text format, so the server parses it as whatever type it
/// inferred for the placeholder. `None` binds NULL.
#[derive(Debug)]
struct TextParam<'a>(Option<&'a str>);

impl ToSql for TextParam<'_> {
    fn to_sql(&self, _ty: &Type, out: &mut BytesMut) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        match self.0 {
            None => Ok(IsNull::Yes),
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

/// Decodes a binary result value of any type into display text; types without a
/// decoder show as `‹typname›`.
struct AnyText(String);

impl<'a> FromSql<'a> for AnyText {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        let text = match *ty {
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN | Type::JSON | Type::XML => {
                std::str::from_utf8(raw)?.to_string()
            }
            Type::JSONB => {
                // Binary jsonb is a version byte followed by the JSON text.
                let json = raw.get(1..).ok_or("empty jsonb value")?;
                std::str::from_utf8(json)?.to_string()
            }
            Type::BOOL => bool::from_sql(ty, raw)?.to_string(),
            Type::INT2 => i16::from_sql(ty, raw)?.to_string(),
            Type::INT4 => i32::from_sql(ty, raw)?.to_string(),
            Type::INT8 => i64::from_sql(ty, raw)?.to_string(),
            Type::OID => u32::from_sql(ty, raw)?.to_string(),
            Type::FLOAT4 => f32::from_sql(ty, raw)?.to_string(),
            Type::FLOAT8 => f64::from_sql(ty, raw)?.to_string(),
            Type::UUID => uuid::Uuid::from_slice(raw)?.hyphenated().to_string(),
            _ if matches!(ty.kind(), Kind::Enum(_)) => std::str::from_utf8(raw)?.to_string(),
            _ => format!("‹{}›", ty.name()),
        };
        Ok(AnyText(text))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any_text(ty: &Type, raw: &[u8]) -> String {
        AnyText::from_sql(ty, raw).unwrap().0
    }

    #[test]
    fn wrappable_only_admits_statements_the_wrapper_accepts() {
        for sql in ["select 1", "(select 1)", "values (1)", "table t", "update t set a = 1 returning a",
                    "insert into t values (1) returning a", "delete from t returning a",
                    "with a as (select 1) select * from a"] {
            assert!(wrappable(sql), "{sql}");
        }
        for sql in ["explain select 1", "show search_path", "with d as (delete from t returning a) select a from d",
                    "with a as (select 1) update t set x = 1 returning x", "fetch 10 from c", "do $$ begin end $$"] {
            assert!(!wrappable(sql), "{sql}");
        }
    }

    #[test]
    fn record_reason_prefers_a_server_error() {
        let slot = Mutex::new(None);
        record_reason(&slot, "connection closed".into(), false);
        record_reason(&slot, "later".into(), false);
        assert_eq!(slot.lock().unwrap().as_deref(), Some("connection closed"));
        record_reason(&slot, "terminating connection due to administrator command".into(), true);
        record_reason(&slot, "connection closed".into(), false);
        assert_eq!(slot.lock().unwrap().as_deref(), Some("terminating connection due to administrator command"));
    }

    #[test]
    fn any_text_decodes_binary_values() {
        assert_eq!(any_text(&Type::TEXT, b"hi"), "hi");
        assert_eq!(any_text(&Type::JSONB, b"\x01{\"a\": 1}"), "{\"a\": 1}");
        assert_eq!(any_text(&Type::BOOL, &[1]), "true");
        assert_eq!(any_text(&Type::INT2, &7i16.to_be_bytes()), "7");
        assert_eq!(any_text(&Type::INT4, &(-3i32).to_be_bytes()), "-3");
        assert_eq!(any_text(&Type::INT8, &9_000_000_000i64.to_be_bytes()), "9000000000");
        assert_eq!(any_text(&Type::OID, &42u32.to_be_bytes()), "42");
        assert_eq!(any_text(&Type::FLOAT8, &1.5f64.to_be_bytes()), "1.5");
        let uuid = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        assert_eq!(any_text(&Type::UUID, uuid.as_bytes()), "01234567-89ab-cdef-0123-456789abcdef");
        assert_eq!(any_text(&Type::NUMERIC, &[0, 0]), "‹numeric›");
    }
}
