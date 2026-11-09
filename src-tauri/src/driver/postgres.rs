use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPoolOptions, PgRow, PgSslMode};
use sqlx::{AssertSqlSafe, Connection, PgPool, Row};

use super::pg_session::{session_config, PgQuerySession, SessionConfig};
use super::{Dialect, Driver, PostgresDialect, DEFAULT_COUNT_TIMEOUT_MS};
use crate::error::{AppError, AppResult};
use crate::export::{ExportColumn, RowSink};
use crate::model::*;
use crate::query::{QueryLanguage, QuerySession};
use crate::sqltext::SqlDialect;

pub struct PostgresDriver {
    pool: PgPool,
    dialect: PostgresDialect,
    /// `statement_timeout` for row counts, in milliseconds.
    count_timeout_ms: u64,
    /// `statement_timeout` for each statement of a grid save, in milliseconds.
    save_statement_timeout_ms: u64,
    /// How query-tab sessions connect: same server, credentials, TLS and setup.
    session: SessionConfig,
    /// How the pool connects, for one-off catalog connections to other databases.
    options: PgConnectOptions,
    /// The profile's database, which the pool and `session` use.
    database: String,
    startup_sql: Option<String>,
    read_only: bool,
    timeout_secs: u32,
    /// The page loads running now, by table (schema, name) and tag, for Stop.
    page_loads: PageLoads,
}

type PageLoads = Mutex<HashMap<(String, String), HashMap<String, LoadState>>>;

/// What Stop needs to know about one page load.
#[derive(Default)]
struct LoadState {
    /// Stop was asked for: the load sends nothing more.
    stopped: bool,
    /// A tagged query of the load was sent, so it may be running on the server.
    sent: bool,
}

/// The map, also after a thread panicked while holding it: each entry is a plain flag write.
fn lock_loads(loads: &PageLoads) -> MutexGuard<'_, HashMap<(String, String), HashMap<String, LoadState>>> {
    loads.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a page load that Stop marked returns instead of sending its next query.
const STOPPED: &str = "Stopped.";

/// Keeps a page load registered under its table, from its start until its row query ends;
/// dropping it, also on an error or when the future is dropped, removes it. Its queries
/// that can wait on the server (the tie-break lookup and the row query) start with its tag.
struct PageLoad<'a> {
    loads: &'a PageLoads,
    key: (String, String),
    tag: String,
}

impl<'a> PageLoad<'a> {
    /// A fresh load with the tag `/* ts-page:<uuid> */`, registered under `table`.
    fn register(loads: &'a PageLoads, table: &TableRef) -> Self {
        let key = (table.schema.clone(), table.name.clone());
        let tag = format!("/* ts-page:{} */", uuid::Uuid::new_v4());
        lock_loads(loads).entry(key.clone()).or_default().insert(tag.clone(), LoadState::default());
        Self { loads, key, tag }
    }

    /// Called right before sending one of the load's tagged queries: an error, without
    /// sending, once Stop marked the load; otherwise notes that a query may be on the server,
    /// so a later Stop cancels it there. Under the same lock as `cancel_pages`' mark.
    fn before_send(&self) -> AppResult<()> {
        let mut m = lock_loads(self.loads);
        match m.get_mut(&self.key).and_then(|loads| loads.get_mut(&self.tag)) {
            Some(state) if state.stopped => Err(AppError::Query(STOPPED.into())),
            Some(state) => {
                state.sent = true;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

impl Drop for PageLoad<'_> {
    fn drop(&mut self) {
        let mut m = lock_loads(self.loads);
        if let Some(loads) = m.get_mut(&self.key) {
            loads.remove(&self.tag);
            if loads.is_empty() {
                m.remove(&self.key);
            }
        }
    }
}

/// Cancels the running queries whose text starts with the tag `$1`, other than this one.
/// Only `active` ones: an idle pooled connection whose last query had the tag may be in use
/// by something else by now.
const CANCEL_TAGGED_SQL: &str = "SELECT pg_cancel_backend(pid) FROM pg_stat_activity \
     WHERE pid <> pg_backend_pid() AND state = 'active' AND left(query, length($1)) = $1";

const DEFAULT_SAVE_STATEMENT_TIMEOUT_MS: u64 = 30_000;

/// Maps an error raised while establishing a connection to `AppError::Connection`.
/// Errors reported by the server are reduced to the server's message; other
/// errors (I/O, TLS, protocol) keep their display text.
fn connection_error(e: sqlx::Error) -> AppError {
    match e {
        sqlx::Error::Database(db) => AppError::Connection(db.message().to_string()),
        other => AppError::Connection(other.to_string()),
    }
}

/// A filter value the column type rejects (SQLSTATE class 22, e.g. 22P02 for
/// `id = "abc"`) or a comparison the type has no operator for (42883) is a
/// filter error, shown inline under the filter bar.
fn filter_query_error(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e
        && let Some(code) = db.code()
        && (code.starts_with("22") || code == "42883")
    {
        return AppError::Filter { message: db.message().to_string(), position: 0 };
    }
    e.into()
}

fn ssl_mode(mode: &SslMode) -> PgSslMode {
    match mode {
        SslMode::Disable => PgSslMode::Disable,
        SslMode::Prefer => PgSslMode::Prefer,
        SslMode::Require => PgSslMode::Require,
        SslMode::VerifyCa => PgSslMode::VerifyCa,
        SslMode::VerifyFull => PgSslMode::VerifyFull,
    }
}

/// Per-connection setup: the user's startup SQL first, then read-only mode, so
/// the startup SQL cannot switch read-only mode off.
async fn setup_session(conn: &mut PgConnection, startup_sql: Option<&str>, read_only: bool) -> sqlx::Result<()> {
    if let Some(sql) = startup_sql {
        sqlx::raw_sql(AssertSqlSafe(sql.to_string())).execute(&mut *conn).await?;
    }
    if read_only {
        sqlx::raw_sql("SET default_transaction_read_only = on").execute(&mut *conn).await?;
    }
    Ok(())
}

pub async fn connect_postgres(
    profile: &ConnectionProfile,
    password: Option<String>,
) -> AppResult<PostgresDriver> {
    // Built first so a bad CA file is reported before any network traffic.
    let net = profile.network()?;
    let session = session_config(profile, password.clone())?;
    let mut options = PgConnectOptions::new()
        .host(&net.host)
        .port(net.port)
        .username(&net.user)
        .database(&net.database)
        .application_name("Porthole")
        .ssl_mode(ssl_mode(&net.ssl_mode));
    if let Some(pw) = password {
        options = options.password(&pw);
    }
    if let Some(cert) = net.ssl_root_cert.as_deref().filter(|c| !c.is_empty()) {
        options = options.ssl_root_cert(Path::new(cert));
    }

    let read_only = profile.read_only;
    let startup_sql = profile.startup_sql.clone().filter(|s| !s.trim().is_empty());
    let timeout_secs = profile.connect_timeout_secs.max(1);
    let timeout = Duration::from_secs(timeout_secs.into());

    // Open one connection directly first. The pool would retry a refused
    // connection until its deadline and hide setup errors behind "pool timed
    // out"; a direct connection reports the real cause.
    let mut probe = match tokio::time::timeout(timeout, PgConnection::connect_with(&options)).await {
        Err(_) => {
            return Err(AppError::Connection(format!(
                "Timed out after {timeout_secs} s connecting to {}:{}",
                net.host, net.port
            )));
        }
        Ok(Err(e)) => return Err(connection_error(e)),
        Ok(Ok(conn)) => conn,
    };
    let setup = setup_session(&mut probe, startup_sql.as_deref(), read_only).await;
    probe.close().await.ok();
    setup.map_err(connection_error)?;

    let pool_startup_sql = startup_sql.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(timeout)
        .after_connect(move |conn, _meta| {
            let startup_sql = pool_startup_sql.clone();
            Box::pin(async move { setup_session(conn, startup_sql.as_deref(), read_only).await })
        })
        .connect_lazy_with(options.clone());
    Ok(PostgresDriver { pool, dialect: PostgresDialect, count_timeout_ms: DEFAULT_COUNT_TIMEOUT_MS,
        save_statement_timeout_ms: DEFAULT_SAVE_STATEMENT_TIMEOUT_MS,
        session,
        options,
        database: net.database.clone(),
        startup_sql,
        read_only,
        timeout_secs,
        page_loads: Mutex::default(),
    })
}

/// A connection for one catalog lookup: a pooled one for the profile's database, or a
/// one-off connection to another database, closed by `finish`.
enum CatalogConn {
    Pooled(sqlx::pool::PoolConnection<sqlx::Postgres>),
    OneOff(PgConnection),
}

impl CatalogConn {
    fn conn(&mut self) -> &mut PgConnection {
        match self {
            CatalogConn::Pooled(c) => c,
            CatalogConn::OneOff(c) => c,
        }
    }

    async fn finish(self) {
        if let CatalogConn::OneOff(c) = self
            && let Err(e) = c.close().await
        {
            eprintln!("could not close a catalog connection: {e}");
        }
    }
}

/// The database's own message when there is one, else the client error text.
fn db_message(e: &sqlx::Error) -> String {
    match e.as_database_error() {
        Some(d) => d.message().to_string(),
        None => e.to_string(),
    }
}

/// Lock and statement limits for a grid save. The only value put into the SQL is the driver's own
/// integer setting (30 000 ms unless a test lowers it); nothing from the user ever is.
fn save_timeouts_sql(statement_timeout_ms: u64) -> String {
    format!("SET LOCAL lock_timeout = '5s'; SET LOCAL statement_timeout = {statement_timeout_ms}")
}
const LOCK_NOT_AVAILABLE: &str = "55P03";
pub const SAVE_LOCKED: &str =
    "Another session is holding a lock on this row (an open transaction in a query tab?).";
pub const SAVE_TIMED_OUT: &str = "The save took longer than 30 s and was canceled.";
pub const COMMIT_UNCERTAIN: &str = "The connection failed while committing, so the save may or may not have been applied. Reload before saving again.";
pub const COMMIT_ABORTED: &str = "The connection failed while committing, and the server rolled the save back.";

/// What the server says became of a transaction whose COMMIT got no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStatus {
    Committed,
    Aborted,
    /// The server could not be asked, or could not tell.
    Unknown,
}

/// Reads a `txid_status` answer; `None` means "still in progress, ask again".
fn commit_status_from(status: Option<&str>) -> Option<CommitStatus> {
    match status {
        Some("committed") => Some(CommitStatus::Committed),
        Some("aborted") => Some(CommitStatus::Aborted),
        Some("in progress") => None,
        _ => Some(CommitStatus::Unknown),
    }
}

/// How long, in all, an uncertain commit may spend asking the server what happened.
const COMMIT_PROBE_BUDGET: Duration = Duration::from_secs(5);
const COMMIT_PROBE_RETRY: Duration = Duration::from_millis(200);

/// The copy for a save that hit its lock or time limit, if it did.
fn save_limit_message(e: &sqlx::Error) -> Option<&'static str> {
    match e.as_database_error()?.code()?.as_ref() {
        LOCK_NOT_AVAILABLE => Some(SAVE_LOCKED),
        QUERY_CANCELED => Some(SAVE_TIMED_OUT),
        _ => None,
    }
}

/// A failed statement of a save: the limits get their own copy, anything else names the change
/// (`UPDATE id = 1: …`).
fn statement_error(describe: &str, e: &sqlx::Error) -> AppError {
    match save_limit_message(e) {
        Some(m) => AppError::Query(m.into()),
        None => AppError::Query(format!("{describe}: {}", db_message(e))),
    }
}

/// A COMMIT the server answered with an error (a deferred constraint, a time limit) was rolled
/// back, so nothing was saved. Any other failure (the connection broke) leaves the outcome unknown.
fn commit_error(e: &sqlx::Error) -> AppError {
    if e.as_database_error().is_some() {
        return AppError::Query(save_limit_message(e).map_or_else(|| db_message(e), str::to_string));
    }
    eprintln!("apply_changes: commit failed: {e}");
    AppError::Uncertain(COMMIT_UNCERTAIN.into())
}

/// Rolls back a failed save. The original error is what the user sees; a rollback failure is only
/// logged (the server discards the transaction when the connection goes).
async fn rollback(tx: sqlx::Transaction<'_, sqlx::Postgres>) {
    if let Err(e) = tx.rollback().await {
        eprintln!("apply_changes: rollback failed: {e}");
    }
}

impl PostgresDriver {
    /// `Some(db)` only when `db` is not the profile's own database.
    fn other_database<'a>(&self, database: Option<&'a str>) -> Option<&'a str> {
        database.filter(|db| *db != self.database)
    }

    /// A connection to `database` for a catalog lookup. Another database gets a one-off
    /// connection with the pool's options and setup (startup SQL, read-only), bounded by the
    /// connect timeout. Failures there are `AppError::Connection` with the server's message.
    async fn catalog_conn(&self, database: Option<&str>) -> AppResult<CatalogConn> {
        let Some(db) = self.other_database(database) else {
            return Ok(CatalogConn::Pooled(self.pool.acquire().await?));
        };
        let options = self.options.clone().database(db);
        let timeout = Duration::from_secs(self.timeout_secs.into());
        let mut conn = match tokio::time::timeout(timeout, PgConnection::connect_with(&options)).await {
            Err(_) => {
                return Err(AppError::Connection(format!(
                    "Timed out after {} s connecting to database {db}",
                    self.timeout_secs
                )));
            }
            Ok(Err(e)) => return Err(connection_error(e)),
            Ok(Ok(conn)) => conn,
        };
        if let Err(e) = setup_session(&mut conn, self.startup_sql.as_deref(), self.read_only).await {
            conn.close().await.ok();
            return Err(connection_error(e));
        }
        Ok(CatalogConn::OneOff(conn))
    }

    /// Overrides the row-count statement timeout (default 3000 ms). Test support.
    #[doc(hidden)]
    pub fn set_count_timeout_ms(&mut self, ms: u64) {
        self.count_timeout_ms = ms;
    }

    /// Overrides a grid save's per-statement timeout (default 30 000 ms). Test support.
    #[doc(hidden)]
    pub fn set_save_statement_timeout_ms(&mut self, ms: u64) {
        self.save_statement_timeout_ms = ms;
    }

    /// Asks the server, on a fresh pooled connection, whether transaction `xid` (a
    /// `txid_current()` value) committed. While the server still reports it in progress (its
    /// backend may be finishing a COMMIT whose answer was lost), asks again, within
    /// `COMMIT_PROBE_BUDGET`. Any failure to ask is `Unknown`, and so is an answer from a server
    /// that restarted (or is another server) since the transaction ran: `server_start` is
    /// `pg_postmaster_start_time()::text` as read inside that transaction. After a crash-restart
    /// the same id can belong to another transaction.
    pub async fn commit_status(&self, xid: i64, server_start: &str) -> CommitStatus {
        let probe = async {
            loop {
                let answer: (Option<String>, String) =
                    match sqlx::query_as("SELECT txid_status($1), pg_postmaster_start_time()::text")
                        .bind(xid)
                        .fetch_one(&self.pool)
                        .await
                    {
                        Ok(a) => a,
                        Err(e) => {
                            eprintln!("apply_changes: commit status probe failed: {e}");
                            return CommitStatus::Unknown;
                        }
                    };
                let (status, started) = answer;
                if started != server_start {
                    eprintln!("apply_changes: the server restarted since the save; its commit status is unknown");
                    return CommitStatus::Unknown;
                }
                match commit_status_from(status.as_deref()) {
                    Some(s) => return s,
                    None => tokio::time::sleep(COMMIT_PROBE_RETRY).await,
                }
            }
        };
        tokio::time::timeout(COMMIT_PROBE_BUDGET, probe).await.unwrap_or(CommitStatus::Unknown)
    }
}

const TABLES_SQL: &str = "
SELECT c.relname::text FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1 AND c.relkind = ANY($2) AND NOT c.relispartition
ORDER BY c.relname";

const FUNCTIONS_SQL: &str = "
SELECT p.proname::text, pg_get_function_identity_arguments(p.oid)
FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = $1 AND p.prokind IN ('f','p')
ORDER BY 1, 2";

const COLUMNS_SQL: &str = "
SELECT a.attname::text, t.typname::text, format_type(a.atttypid, a.atttypmod),
       NOT a.attnotnull, pg_get_expr(d.adbin, d.adrelid),
       EXISTS (SELECT 1 FROM pg_index i
               WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey)),
       CASE t.typname WHEN 'bpchar' THEN 'bpchar' WHEN 'bit' THEN 'varbit'
            WHEN '_bpchar' THEN 'bpchar[]' WHEN '_bit' THEN 'varbit[]'
            ELSE format_type(a.atttypid, NULL) END,
       a.attidentity = 'a' OR coalesce(to_jsonb(a) ->> 'attgenerated', '') = 's'
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
JOIN pg_type t ON t.oid = a.atttypid
LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r','p','f','v','m')
ORDER BY a.attnum";

/// Relation kind and primary-key columns in key order. `indnkeyatts` (PG 11+)
/// leaves INCLUDE columns out.
const KEY_SQL: &str = "
SELECT c.relkind::text,
       ARRAY(SELECT a.attname::text
             FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
             WHERE k.ord <= i.indnkeyatts
             ORDER BY k.ord)
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary
WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r','p','f','v','m')";

/// For a relation without a primary key: its relkind, its main fork's size (`r` and `m` only,
/// else 0; NULL when it went away meanwhile), whether other tables inherit from it, and the key
/// columns of its best unique index for paging: valid, not partial, no expression key column,
/// every key column NOT NULL, fewest key columns, then by name.
const TIEBREAK_SQL: &str = "
SELECT c.relkind::text,
       CASE WHEN c.relkind IN ('r','m') THEN pg_relation_size(c.oid) ELSE 0 END,
       c.relhassubclass,
       (SELECT ARRAY(SELECT a.attname::text
                     FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
                     JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                     WHERE k.ord <= i.indnkeyatts
                     ORDER BY k.ord)
        FROM pg_index i
        JOIN pg_class ic ON ic.oid = i.indexrelid
        WHERE i.indrelid = c.oid AND i.indisunique AND i.indisvalid AND i.indpred IS NULL
          AND NOT EXISTS (SELECT 1
                          FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
                          LEFT JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                          WHERE k.ord <= i.indnkeyatts AND NOT coalesce(a.attnotnull, false))
        ORDER BY i.indnkeyatts, ic.relname
        LIMIT 1)
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r','p','f','v','m')";

/// Above this size (16 MiB) a keyless table isn't sorted by `ctid`: the sort would read it all.
const CTID_SIZE_LIMIT: i64 = 16 * 1024 * 1024;

const INDEXES_SQL: &str = "
SELECT ic.relname::text, i.indisunique, i.indisprimary,
       ARRAY(SELECT COALESCE(a.attname::text, pg_get_indexdef(i.indexrelid, k.ord::int, true))
             FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
             LEFT JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
             WHERE k.ord <= i.indnkeyatts
             ORDER BY k.ord),
       ARRAY(SELECT a.attname::text
             FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
             WHERE k.ord > i.indnkeyatts
             ORDER BY k.ord)
FROM pg_index i
JOIN pg_class c ON c.oid = i.indrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_class ic ON ic.oid = i.indexrelid
WHERE n.nspname = $1 AND c.relname = $2
ORDER BY i.indisprimary DESC, ic.relname";

const FOREIGN_KEYS_SQL: &str = "
SELECT con.conname::text,
       ARRAY(SELECT a.attname::text
             FROM unnest(con.conkey) WITH ORDINALITY k(attnum, ord)
             JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum
             ORDER BY k.ord),
       rn.nspname::text, rc.relname::text,
       ARRAY(SELECT a.attname::text
             FROM unnest(con.confkey) WITH ORDINALITY k(attnum, ord)
             JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum
             ORDER BY k.ord)
FROM pg_constraint con
JOIN pg_class c ON c.oid = con.conrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_class rc ON rc.oid = con.confrelid
JOIN pg_namespace rn ON rn.oid = rc.relnamespace
WHERE con.contype = 'f' AND n.nspname = $1 AND c.relname = $2
ORDER BY con.conname";

const SCHEMAS_SQL: &str = "
SELECT nspname::text FROM pg_namespace
WHERE nspname NOT IN ('pg_catalog', 'information_schema')
  AND nspname NOT LIKE 'pg\\_toast%' AND nspname NOT LIKE 'pg\\_temp\\_%'
ORDER BY nspname";

/// Relations with their columns in `attnum` order; partitions are left out, as
/// in the schema tree.
const SCHEMA_COLUMNS_SQL: &str = "
SELECT c.relname::text,
       coalesce(array_agg(a.attname::text ORDER BY a.attnum) FILTER (WHERE a.attnum IS NOT NULL), '{}')
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
WHERE n.nspname = $1 AND c.relkind IN ('r','p','f','v','m') AND NOT c.relispartition
GROUP BY c.relname";

/// Unfiltered counts below this many estimated rows are computed exactly.
const EXACT_COUNT_LIMIT: f64 = 50_000.0;
/// Table alias for ORDER BY: a bare column name would resolve to the `::text`
/// output column of the same name and sort lexicographically.
const ORDER_ALIAS: &str = "ts_t";
const QUERY_CANCELED: &str = "57014";

/// A pooled connection streaming an export. If the stream is abandoned (cancel, sink
/// or stream error, or the future being dropped), returning the connection would make
/// sqlx read and discard every remaining row while the server keeps producing them and
/// the pool slot stays taken. So an unfinished guard asks the server to cancel the query
/// and closes the connection instead.
///
/// The connection exports with `extra_float_digits = 3`, so float4/float8 text round-trips
/// exactly on PG 11 too (PG 12+ prints the shortest exact text either way). The pool's own
/// value is put back before the connection returns to the pool, so the grid is unchanged.
struct ExportConn {
    conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
    pool: PgPool,
    pid: i32,
    /// `extra_float_digits` as the pool had it.
    float_digits: String,
    done: bool,
}

impl ExportConn {
    async fn acquire(pool: &PgPool) -> AppResult<Self> {
        let mut conn = pool.acquire().await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *conn).await?;
        let float_digits: String =
            sqlx::query_scalar("SELECT current_setting('extra_float_digits')").fetch_one(&mut *conn).await?;
        let mut guard = Self { conn: Some(conn), pool: pool.clone(), pid, float_digits, done: false };
        // From here on a failure drops the guard, which closes the connection instead of
        // returning it to the pool with the export's setting.
        sqlx::query("SELECT set_config('extra_float_digits', '3', false)").execute(guard.conn()).await?;
        Ok(guard)
    }

    fn conn(&mut self) -> &mut PgConnection {
        self.conn.as_mut().expect("export connection is present until drop")
    }

    /// The stream ran to its end: the pool's setting is put back and the connection goes back
    /// to the pool as usual. If that fails, the connection is closed instead (nothing runs on
    /// it any more, so there is nothing to cancel).
    async fn finished(&mut self) {
        let restore = sqlx::query("SELECT set_config('extra_float_digits', $1, false)")
            .bind(self.float_digits.clone())
            .execute(self.conn())
            .await;
        if let Err(e) = restore {
            eprintln!("could not restore extra_float_digits after an export, closing that connection: {e}");
            if let Some(mut conn) = self.conn.take() {
                conn.close_on_drop();
            }
        }
        self.done = true;
    }
}

impl Drop for ExportConn {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Some(mut conn) = self.conn.take() {
            conn.close_on_drop();
        }
        let (pool, pid) = (self.pool.clone(), self.pid);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(e) = sqlx::query("SELECT pg_cancel_backend($1)").bind(pid).execute(&pool).await {
                        eprintln!(
                            "ERROR: could not cancel the abandoned export query on backend pid {pid}; \
                             it may run on until the server notices the closed connection: {e}"
                        );
                    }
                });
            }
            Err(e) => eprintln!(
                "ERROR: no async runtime to cancel the abandoned export query on backend pid {pid}; \
                 it may run on until the server notices the closed connection: {e}"
            ),
        }
    }
}

struct BuiltSelect {
    /// SELECT list, FROM, WHERE and ORDER BY; no LIMIT or OFFSET.
    sql: String,
    where_sql: Option<String>,
    params: Vec<String>,
}

fn from_clause(d: &PostgresDialect, table: &TableRef) -> String {
    format!("{}.{}", d.quote_ident(&table.schema), d.quote_ident(&table.name))
}

/// What breaks ties after the user's sort, so that paging sees every row once.
#[derive(Debug, Clone, PartialEq)]
enum Tiebreak {
    /// Columns that are unique together, ascending.
    Columns(Vec<String>),
    /// A system column (`ctid`), unquoted.
    System(&'static str),
    /// Nothing: rows equal on the sort have no fixed order.
    None,
}

/// `keep_padding` reads `char(n)` columns without the `::text` cast, which strips
/// their trailing spaces; export wants the stored value. Paging keeps the cast.
/// The primary key breaks ties (`build_page_select` with `Tiebreak::Columns`).
fn build_select(
    d: &PostgresDialect,
    columns: &[ColumnInfo],
    q: &TableQuery,
    keep_padding: bool,
) -> AppResult<BuiltSelect> {
    let key = columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect();
    build_page_select(d, columns, q, keep_padding, &Tiebreak::Columns(key))
}

/// `build_select` with `tiebreak` after the sort in place of the primary key.
fn build_page_select(
    d: &PostgresDialect,
    columns: &[ColumnInfo],
    q: &TableQuery,
    keep_padding: bool,
    tiebreak: &Tiebreak,
) -> AppResult<BuiltSelect> {
    let from = from_clause(d, &q.table);
    let filter = q.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
    let (where_sql, params) = match filter {
        Some(f) => {
            let expr = crate::filter::parse(f)?;
            let (sql, params) = crate::filter::compile(&expr, columns, d, 1)?;
            (Some(sql), params)
        }
        None => (None, Vec::new()),
    };

    let order_term = |name: &str, dir: &str| format!("{ORDER_ALIAS}.{} {dir}", d.quote_ident(name));
    // Tie-break terms after the sort column `skip` (if any), so that paging is stable.
    let tiebreak_terms = |skip: Option<&str>| -> Vec<String> {
        match tiebreak {
            Tiebreak::Columns(names) => {
                names.iter().filter(|n| Some(n.as_str()) != skip).map(|n| order_term(n, "ASC")).collect()
            }
            Tiebreak::System(name) => vec![format!("{ORDER_ALIAS}.{name}")],
            Tiebreak::None => Vec::new(),
        }
    };
    let order_by = match &q.sort {
        Some(s) => {
            let col = columns
                .iter()
                .find(|c| c.name == s.column)
                .ok_or_else(|| AppError::Query(format!("Unknown column {}", s.column)))?;
            let dir = if s.descending { "DESC NULLS LAST" } else { "ASC NULLS LAST" };
            let terms: Vec<String> =
                std::iter::once(order_term(&col.name, dir)).chain(tiebreak_terms(Some(&col.name))).collect();
            Some(terms.join(", "))
        }
        None => {
            let terms = tiebreak_terms(None);
            (!terms.is_empty()).then(|| terms.join(", "))
        }
    };

    let select_list = columns
        .iter()
        .map(|c| {
            let q = d.quote_ident(&c.name);
            if keep_padding && c.data_type == "bpchar" {
                // format() prints through the type's output function, which keeps the padding.
                format!("CASE WHEN {q} IS NULL THEN NULL ELSE format('%s', {q}) END AS {q}")
            } else {
                format!("{q}::text AS {q}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("SELECT {select_list} FROM {from} AS {ORDER_ALIAS}");
    if let Some(w) = &where_sql {
        sql.push_str(&format!(" WHERE {w}"));
    }
    if let Some(o) = &order_by {
        sql.push_str(&format!(" ORDER BY {o}"));
    }
    Ok(BuiltSelect { sql, where_sql, params })
}

/// `select` plus the LIMIT and OFFSET placeholders that follow `n_params` filter parameters.
fn page_sql(select: &str, n_params: usize) -> String {
    format!("{select} LIMIT ${} OFFSET ${}", n_params + 1, n_params + 2)
}

/// The tie-break of a relation without a primary key, from `TIEBREAK_SQL`'s row: the unique
/// index if there is one, else `ctid` for a table or materialized view of at most
/// `CTID_SIZE_LIMIT` bytes, else none (views, partitioned and foreign tables, larger tables, a
/// size unknown). A table others inherit from has none either: the page reads its children too
/// (no `ONLY`), where its unique index doesn't apply, ctids repeat and its size says nothing.
fn keyless_tiebreak(relkind: &str, size: Option<i64>, has_children: bool, unique_key: Option<Vec<String>>) -> Tiebreak {
    if relkind == "r" && has_children {
        return Tiebreak::None;
    }
    match unique_key {
        Some(key) if !key.is_empty() => Tiebreak::Columns(key),
        _ if matches!(relkind, "r" | "m") && size.is_some_and(|s| s <= CTID_SIZE_LIMIT) => Tiebreak::System("ctid"),
        _ => Tiebreak::None,
    }
}

impl PostgresDriver {
    /// The export SELECT: every column as text (`char(n)` keeps its padding, unlike paging's
    /// `::text`), FROM, WHERE and ORDER BY in the grid's order, without LIMIT or OFFSET,
    /// plus the filter's bind parameters.
    pub fn table_select(&self, structure: &TableStructure, q: &TableQuery) -> AppResult<(String, Vec<String>)> {
        let built = build_select(&self.dialect, &structure.columns, q, true)?;
        Ok((built.sql, built.params))
    }
}

impl PostgresDriver {
    /// Runs a `count(*)` query under `statement_timeout`; `None` when it times out.
    async fn bounded_count(&self, sql: String, params: &[String]) -> sqlx::Result<Option<u64>> {
        let mut tx = self.pool.begin().await?;
        sqlx::raw_sql(AssertSqlSafe(format!("SET LOCAL statement_timeout = {}", self.count_timeout_ms)))
            .execute(&mut *tx)
            .await?;
        let mut q = sqlx::query_scalar::<_, i64>(AssertSqlSafe(sql));
        for p in params {
            q = q.bind(p.clone());
        }
        match q.fetch_one(&mut *tx).await {
            Ok(n) => {
                tx.rollback().await?;
                Ok(Some(n as u64))
            }
            // The transaction is aborted; dropping it rolls it back.
            Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some(QUERY_CANCELED) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `Estimated(reltuples)` for large relations; otherwise a bounded exact count.
    /// Views, partitioned parents and unanalyzed tables report 0 or -1 and are
    /// counted exactly when the count finishes in time, else `Unknown`.
    async fn total_count(&self, table: &TableRef, from: &str) -> AppResult<RowCount> {
        let reltuples: Option<f64> = sqlx::query_scalar(
            "SELECT c.reltuples::float8 FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relname = $2",
        )
        .bind(&table.schema)
        .bind(&table.name)
        .fetch_optional(&self.pool)
        .await?;
        let estimate = reltuples.filter(|r| *r > 0.0);
        if let Some(est) = estimate.filter(|r| *r >= EXACT_COUNT_LIMIT) {
            return Ok(RowCount::Estimated(est as u64));
        }
        Ok(match self.bounded_count(format!("SELECT count(*) FROM {from}"), &[]).await? {
            Some(n) => RowCount::Exact(n),
            None => estimate.map_or(RowCount::Unknown, |est| RowCount::Estimated(est as u64)),
        })
    }

    /// Counts filtered rows, giving up (`Unknown`) after the statement timeout.
    async fn filtered_count(&self, from: &str, where_sql: &str, params: &[String]) -> AppResult<RowCount> {
        let sql = format!("SELECT count(*) FROM {from} WHERE {where_sql}");
        Ok(match self.bounded_count(sql, params).await.map_err(filter_query_error)? {
            Some(n) => RowCount::Exact(n),
            None => RowCount::Unknown,
        })
    }

    async fn relation_names(&self, schema: &str, relkinds: &[&str]) -> AppResult<Vec<String>> {
        let kinds: Vec<String> = relkinds.iter().map(|k| k.to_string()).collect();
        let rows = sqlx::query(TABLES_SQL)
            .bind(schema)
            .bind(kinds)
            .fetch_all(&self.pool)
            .await?;
        decode_rows(&rows, |r| r.try_get(0))
    }
}

/// Decodes every row, turning a type mismatch into `AppError::Query` instead of a panic.
fn decode_rows<T>(rows: &[PgRow], f: impl Fn(&PgRow) -> sqlx::Result<T>) -> AppResult<Vec<T>> {
    rows.iter().map(|r| f(r).map_err(AppError::from)).collect()
}

#[async_trait]
impl Driver for PostgresDriver {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }

    fn query_language(&self) -> QueryLanguage {
        QueryLanguage::Sql(SqlDialect::Postgres)
    }

    async fn server_version(&self) -> AppResult<String> {
        let row = sqlx::query("SELECT version()").fetch_one(&self.pool).await?;
        Ok(row.try_get(0)?)
    }

    async fn list_databases(&self) -> AppResult<Vec<String>> {
        let rows = sqlx::query(
            "SELECT datname::text FROM pg_database
             WHERE datallowconn AND NOT datistemplate AND has_database_privilege(datname, 'CONNECT')
             ORDER BY datname",
        )
        .fetch_all(&self.pool)
        .await?;
        decode_rows(&rows, |r| r.try_get(0))
    }

    async fn list_schemas(&self, database: Option<&str>) -> AppResult<Vec<String>> {
        let mut conn = self.catalog_conn(database).await?;
        let rows = sqlx::query(SCHEMAS_SQL).fetch_all(conn.conn()).await;
        conn.finish().await;
        decode_rows(&rows?, |r| r.try_get(0))
    }

    async fn schema_tree(&self, schema: &str) -> AppResult<SchemaTree> {
        let tables = self.relation_names(schema, &["r", "p", "f"]).await?;
        let views = self.relation_names(schema, &["v", "m"]).await?;
        let rows = sqlx::query(FUNCTIONS_SQL).bind(schema).fetch_all(&self.pool).await?;
        let functions = decode_rows(&rows, |r| Ok(FunctionInfo { name: r.try_get(0)?, signature: r.try_get(1)?, kind: None }))?;
        Ok(SchemaTree { tables, views, functions })
    }

    async fn table_structure(&self, table: &TableRef) -> AppResult<TableStructure> {
        let rows = sqlx::query(COLUMNS_SQL)
            .bind(&table.schema)
            .bind(&table.name)
            .fetch_all(&self.pool)
            .await?;
        if rows.is_empty() {
            return Err(AppError::NotFound(format!("{}.{}", table.schema, table.name)));
        }
        let columns = decode_rows(&rows, |r| {
            Ok(ColumnInfo {
                name: r.try_get(0)?,
                data_type: r.try_get(1)?,
                full_type: r.try_get(2)?,
                nullable: r.try_get(3)?,
                default: r.try_get(4)?,
                is_primary_key: r.try_get(5)?,
                cast_type: r.try_get(6)?,
                read_only: r.try_get(7)?,
            })
        })?;

        let rows = sqlx::query(INDEXES_SQL)
            .bind(&table.schema)
            .bind(&table.name)
            .fetch_all(&self.pool)
            .await?;
        let indexes = decode_rows(&rows, |r| {
            Ok(IndexInfo {
                name: r.try_get(0)?,
                unique: r.try_get(1)?,
                primary: r.try_get(2)?,
                columns: r.try_get(3)?,
                included: r.try_get(4)?,
            })
        })?;

        let rows = sqlx::query(FOREIGN_KEYS_SQL)
            .bind(&table.schema)
            .bind(&table.name)
            .fetch_all(&self.pool)
            .await?;
        let foreign_keys = decode_rows(&rows, |r| {
            Ok(ForeignKeyInfo {
                name: r.try_get(0)?,
                columns: r.try_get(1)?,
                ref_table: TableRef { schema: r.try_get(2)?, name: r.try_get(3)? },
                ref_columns: r.try_get(4)?,
            })
        })?;

        let row = sqlx::query(KEY_SQL)
            .bind(&table.schema)
            .bind(&table.name)
            .fetch_one(&self.pool)
            .await?;
        let relkind: String = row.try_get(0)?;
        let primary_key: Vec<String> = row.try_get(1)?;
        let relation = match relkind.as_str() {
            "p" => RelationKind::Partitioned,
            "f" => RelationKind::Foreign,
            "v" => RelationKind::View,
            "m" => RelationKind::Materialized,
            _ => RelationKind::Table,
        };
        let is_float_key = primary_key.iter().any(|k| {
            columns.iter().any(|c| &c.name == k && matches!(c.data_type.as_str(), "float4" | "float8"))
        });
        let editing = if matches!(relation, RelationKind::View | RelationKind::Materialized) {
            Editing::View
        } else if primary_key.is_empty() {
            Editing::NoPrimaryKey
        } else if is_float_key {
            Editing::FloatKey
        } else {
            Editing::Allowed
        };

        Ok(TableStructure { columns, primary_key, relation, editing, indexes, foreign_keys })
    }

    async fn fetch_page(&self, req: &PageRequest) -> AppResult<TablePage> {
        // Registered first, so Stop during the structure lookup still stops the load.
        let load = PageLoad::register(&self.page_loads, &req.table);
        let structure = self.table_structure(&req.table).await?;
        let (key_names, editing) = (structure.primary_key.clone(), structure.editing);
        let d = &self.dialect;
        let from = from_clause(d, &req.table);
        let tq = TableQuery { table: req.table.clone(), filter: req.filter.clone(), sort: req.sort.clone() };
        // Taken before the Stop checks, so a load waiting for a pooled connection sends nothing
        // once stopped. The tie-break lookup and the row query run on it, tagged (Stop finds
        // them in pg_stat_activity; `pg_relation_size` can wait on a lock) and not kept as
        // prepared statements (each text is used once).
        let mut conn = self.pool.acquire().await?;
        let (built, unstable_order) = if key_names.is_empty() {
            load.before_send()?;
            let row = sqlx::query(AssertSqlSafe(format!("{}{TIEBREAK_SQL}", load.tag)))
                .persistent(false)
                .bind(&req.table.schema)
                .bind(&req.table.name)
                .fetch_optional(&mut *conn)
                .await?;
            // No row: the relation went away since `table_structure`; the page query says so.
            let tiebreak = match row {
                Some(r) => keyless_tiebreak(&r.try_get::<String, _>(0)?, r.try_get(1)?, r.try_get(2)?, r.try_get(3)?),
                None => Tiebreak::None,
            };
            (build_page_select(d, &structure.columns, &tq, false, &tiebreak)?, tiebreak == Tiebreak::None)
        } else {
            (build_select(d, &structure.columns, &tq, false)?, false)
        };
        let columns = structure.columns;
        let (where_sql, params) = (built.where_sql, built.params);
        let page_size = req.page_size.clamp(1, 1000) as i64;
        let offset = req.page as i64 * page_size;
        let sql = format!("{} {}", load.tag, page_sql(&built.sql, params.len()));

        let mut q = sqlx::query(AssertSqlSafe(sql)).persistent(false);
        for p in &params {
            q = q.bind(p.clone());
        }
        load.before_send()?;
        let started = Instant::now();
        let fetched = q.bind(page_size).bind(offset).fetch_all(&mut *conn).await;
        drop(conn);
        drop(load);
        let pg_rows = fetched.map_err(|e| if where_sql.is_some() { filter_query_error(e) } else { e.into() })?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let rows = decode_rows(&pg_rows, |r| {
            (0..columns.len()).map(|i| r.try_get::<Option<String>, _>(i)).collect()
        })?;

        let total = self.total_count(&req.table, &from).await?;
        let matches = match &where_sql {
            Some(w) => Some(self.filtered_count(&from, w, &params).await?),
            None => None,
        };
        Ok(TablePage { columns, primary_key: key_names, editing, rows, total, matches, elapsed_ms, cell_types: None, unstable_order })
    }

    /// Marks every load of `table` stopped (none sends another query) and cancels on the server
    /// the queries they already sent. Returns how many loads it stopped: 0 when none was loading.
    /// The cancel runs on a fresh connection, not the pool: slow pages may hold every pooled one.
    async fn cancel_pages(&self, table: &TableRef) -> AppResult<u32> {
        let (stopped, sent): (usize, Vec<String>) = {
            let mut m = lock_loads(&self.page_loads);
            match m.get_mut(&(table.schema.clone(), table.name.clone())) {
                None => (0, Vec::new()),
                Some(loads) => {
                    for state in loads.values_mut() {
                        state.stopped = true;
                    }
                    let sent = loads.iter().filter(|(_, s)| s.sent).map(|(tag, _)| tag.clone()).collect();
                    (loads.len(), sent)
                }
            }
        };
        let stopped = u32::try_from(stopped).unwrap_or(u32::MAX);
        if sent.is_empty() {
            return Ok(stopped);
        }
        let options = self.pool.connect_options();
        let timeout = Duration::from_secs(self.timeout_secs.into());
        let mut conn = match tokio::time::timeout(timeout, PgConnection::connect_with(&options)).await {
            Err(_) => {
                return Err(AppError::Connection(format!(
                    "Timed out after {} s connecting to stop the page",
                    self.timeout_secs
                )));
            }
            Ok(Err(e)) => return Err(connection_error(e)),
            Ok(Ok(conn)) => conn,
        };
        let mut failed = None;
        for tag in &sent {
            if let Err(e) = sqlx::query(CANCEL_TAGGED_SQL).bind(tag).execute(&mut conn).await {
                failed = Some(e);
                break;
            }
        }
        conn.close().await.ok();
        match failed {
            Some(e) => Err(e.into()),
            None => Ok(stopped),
        }
    }

    async fn export_table(&self, q: &TableQuery, sink: &mut dyn RowSink) -> AppResult<()> {
        use futures_util::TryStreamExt;
        let structure = self.table_structure(&q.table).await?;
        let (sql, params) = self.table_select(&structure, q)?;
        let filtered = q.filter.as_deref().is_some_and(|f| !f.trim().is_empty());
        let columns: Vec<ExportColumn> = structure
            .columns
            .iter()
            .map(|c| ExportColumn { name: c.name.clone(), data_type: c.data_type.clone() })
            .collect();
        sink.begin(&columns)?;
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for p in &params {
            query = query.bind(p.clone());
        }
        let map_err = |e: sqlx::Error| if filtered { filter_query_error(e) } else { e.into() };
        let mut guard = ExportConn::acquire(&self.pool).await?;
        {
            let mut stream = query.fetch(guard.conn());
            while let Some(row) = stream.try_next().await.map_err(map_err)? {
                let values = (0..columns.len())
                    .map(|i| row.try_get::<Option<String>, _>(i))
                    .collect::<sqlx::Result<Vec<_>>>()?;
                sink.row(values)?;
            }
        }
        guard.finished().await;
        Ok(())
    }

    async fn apply_changes(&self, set: &ChangeSet) -> AppResult<ApplyResult> {
        let structure = self.table_structure(&set.table).await?;
        let statements = crate::edit::plan_changes(&self.dialect, &structure, set)?;
        let mut tx = self.pool.begin().await.map_err(|e| AppError::Query(db_message(&e)))?;
        // A row locked elsewhere (an open transaction in a query tab) must not hang the save.
        sqlx::raw_sql(AssertSqlSafe(save_timeouts_sql(self.save_statement_timeout_ms)))
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::Query(db_message(&e)))?;
        for st in &statements {
            let mut q = sqlx::query(AssertSqlSafe(st.sql.clone()));
            for p in &st.params {
                q = q.bind(p.clone());
            }
            let done = match q.execute(&mut *tx).await {
                Ok(d) => d,
                Err(e) => {
                    rollback(tx).await;
                    return Err(statement_error(&st.describe, &e));
                }
            };
            if st.expect_one_row && done.rows_affected() != 1 {
                rollback(tx).await;
                return Err(crate::edit::row_changed_error(&st.key_label));
            }
        }
        // Lets an unanswered COMMIT be settled afterwards by asking the server (`txid_status`).
        let mark: (i64, String) = match sqlx::query_as("SELECT txid_current(), pg_postmaster_start_time()::text")
            .fetch_one(&mut *tx)
            .await
        {
            Ok(x) => x,
            Err(e) => {
                rollback(tx).await;
                return Err(AppError::Query(db_message(&e)));
            }
        };
        let applied = ApplyResult { applied: statements.len() };
        match tx.commit().await {
            Ok(()) => Ok(applied),
            Err(e) => match commit_error(&e) {
                uncertain @ AppError::Uncertain(_) => match self.commit_status(mark.0, &mark.1).await {
                    CommitStatus::Committed => Ok(applied),
                    CommitStatus::Aborted => Err(AppError::Query(COMMIT_ABORTED.into())),
                    CommitStatus::Unknown => Err(uncertain),
                },
                other => Err(other),
            },
        }
    }

    async fn schema_columns(&self, database: Option<&str>, schema: &str)
        -> AppResult<BTreeMap<String, Vec<String>>> {
        let mut conn = self.catalog_conn(database).await?;
        let rows = sqlx::query(SCHEMA_COLUMNS_SQL).bind(schema).fetch_all(conn.conn()).await;
        conn.finish().await;
        Ok(decode_rows(&rows?, |r| Ok((r.try_get(0)?, r.try_get(1)?)))?.into_iter().collect())
    }

    async fn open_query_session(&self, database: Option<&str>) -> AppResult<Arc<dyn QuerySession>> {
        let session = match self.other_database(database) {
            Some(db) => PgQuerySession::open(&self.session.for_database(db), self.pool.clone()).await?,
            None => PgQuerySession::open(&self.session, self.pool.clone()).await?,
        };
        Ok(Arc::new(session))
    }

    async fn close(&self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            full_type: ty.into(),
            cast_type: ty.into(),
            nullable: true,
            default: None,
            is_primary_key: pk,
            read_only: false,
        }
    }

    #[test]
    fn table_select_matches_fetch_page_shape() {
        let columns = vec![col("id", "int4", true), col("name", "text", false), col("c", "bpchar", false)];
        let q = TableQuery {
            table: TableRef { schema: "fx".into(), name: "users".into() },
            filter: Some("name = 'a'".into()),
            sort: Some(SortSpec { column: "name".into(), descending: true }),
        };
        let page = build_select(&PostgresDialect, &columns, &q, false).unwrap();
        assert!(!page.sql.contains("LIMIT"));
        assert!(page.sql.contains("WHERE") && page.sql.contains("ORDER BY"));
        assert_eq!(page.params.len(), 1);
        assert_eq!(page_sql(&page.sql, page.params.len()), format!("{} LIMIT $2 OFFSET $3", page.sql));
        // Export differs only in how char(n) is read.
        let export = build_select(&PostgresDialect, &columns, &q, true).unwrap();
        assert!(export.sql.contains("format('%s'") && !page.sql.contains("format("));
        let no_char = vec![col("id", "int4", true), col("name", "text", false)];
        assert_eq!(
            build_select(&PostgresDialect, &no_char, &q, true).unwrap().sql,
            build_select(&PostgresDialect, &no_char, &q, false).unwrap().sql
        );
    }

    fn order_of(columns: &[ColumnInfo], sort: Option<(&str, bool)>, tiebreak: &Tiebreak) -> String {
        let q = TableQuery {
            table: TableRef { schema: "fx".into(), name: "t".into() },
            filter: None,
            sort: sort.map(|(c, d)| SortSpec { column: c.into(), descending: d }),
        };
        let sql = build_page_select(&PostgresDialect, columns, &q, false, tiebreak).unwrap().sql;
        assert!(sql.starts_with("SELECT \"a\"::text AS \"a\", \"b\"::text AS \"b\" FROM \"fx\".\"t\" AS ts_t"), "{sql}");
        sql.split_once(" ORDER BY ").map_or(String::new(), |(_, o)| o.to_string())
    }

    #[test]
    fn page_order_tiebreaks_in_the_page_select() {
        let columns = vec![col("a", "int4", false), col("b", "text", false)];
        let ab = Tiebreak::Columns(vec!["a".into(), "b".into()]);
        assert_eq!(order_of(&columns, None, &ab), "ts_t.\"a\" ASC, ts_t.\"b\" ASC");
        // The sort column isn't repeated as a tie-break.
        assert_eq!(order_of(&columns, Some(("a", true)), &ab), "ts_t.\"a\" DESC NULLS LAST, ts_t.\"b\" ASC");
        let ctid = Tiebreak::System("ctid");
        assert_eq!(order_of(&columns, None, &ctid), "ts_t.ctid");
        assert_eq!(order_of(&columns, Some(("b", false)), &ctid), "ts_t.\"b\" ASC NULLS LAST, ts_t.ctid");
        assert_eq!(order_of(&columns, None, &Tiebreak::None), "");
        assert_eq!(order_of(&columns, Some(("b", false)), &Tiebreak::None), "ts_t.\"b\" ASC NULLS LAST");

        // `build_select` is the page select with the primary key (in column order) as tie-break.
        let keyed = vec![col("k2", "int4", true), col("name", "text", false), col("k1", "int4", true)];
        let q = TableQuery {
            table: TableRef { schema: "fx".into(), name: "t".into() },
            filter: Some("name = 'x'".into()),
            sort: Some(SortSpec { column: "name".into(), descending: false }),
        };
        let built = build_select(&PostgresDialect, &keyed, &q, false).unwrap();
        assert_eq!(
            built.sql,
            "SELECT \"k2\"::text AS \"k2\", \"name\"::text AS \"name\", \"k1\"::text AS \"k1\" FROM \"fx\".\"t\" AS ts_t \
             WHERE \"name\" = $1::text ORDER BY ts_t.\"name\" ASC NULLS LAST, ts_t.\"k2\" ASC, ts_t.\"k1\" ASC"
        );
        let key = Tiebreak::Columns(vec!["k2".into(), "k1".into()]);
        assert_eq!(build_page_select(&PostgresDialect, &keyed, &q, false, &key).unwrap().sql, built.sql);
        let no_key = vec![col("a", "int4", false), col("b", "text", false)];
        assert_eq!(order_of(&no_key, None, &Tiebreak::Columns(Vec::new())), "");
    }

    #[test]
    fn page_order_without_a_primary_key_prefers_a_unique_index_then_ctid() {
        let unique = || Some(vec!["a".to_string()]);
        assert_eq!(keyless_tiebreak("r", Some(8192), false, unique()), Tiebreak::Columns(vec!["a".into()]));
        assert_eq!(keyless_tiebreak("m", Some(1 << 30), false, unique()), Tiebreak::Columns(vec!["a".into()]));
        assert_eq!(keyless_tiebreak("p", Some(0), false, unique()), Tiebreak::Columns(vec!["a".into()]));
        assert_eq!(keyless_tiebreak("r", Some(0), false, None), Tiebreak::System("ctid"));
        assert_eq!(keyless_tiebreak("m", Some(CTID_SIZE_LIMIT), false, None), Tiebreak::System("ctid"));
        for kind in ["v", "p", "f"] {
            assert_eq!(keyless_tiebreak(kind, Some(0), false, None), Tiebreak::None, "{kind}");
        }
    }

    #[test]
    fn page_order_of_an_inheritance_parent_or_an_unknown_size_is_not_fixed() {
        let unique = || Some(vec!["a".to_string()]);
        // The page reads the children too: neither the parent's unique index nor ctid orders them.
        assert_eq!(keyless_tiebreak("r", Some(0), true, unique()), Tiebreak::None);
        assert_eq!(keyless_tiebreak("r", Some(0), true, None), Tiebreak::None);
        // A partitioned table's unique index covers its partitions.
        assert_eq!(keyless_tiebreak("p", Some(0), true, unique()), Tiebreak::Columns(vec!["a".into()]));
        // NULL size: the relation went away meanwhile.
        assert_eq!(keyless_tiebreak("r", None, false, None), Tiebreak::None);
        assert_eq!(keyless_tiebreak("r", None, false, unique()), Tiebreak::Columns(vec!["a".into()]));
    }

    #[test]
    fn page_order_skips_ctid_on_a_large_table() {
        assert_eq!(CTID_SIZE_LIMIT, 16_777_216);
        assert_eq!(keyless_tiebreak("r", Some(16_777_216), false, None), Tiebreak::System("ctid"));
        assert_eq!(keyless_tiebreak("r", Some(16_777_216 + 1), false, None), Tiebreak::None);
        assert_eq!(keyless_tiebreak("m", Some(16_777_216 + 1), false, None), Tiebreak::None);
    }

    #[test]
    fn commit_failure_without_a_server_answer_is_uncertain() {
        let io = sqlx::Error::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset"));
        for e in [io, sqlx::Error::PoolClosed, sqlx::Error::WorkerCrashed] {
            assert_eq!(commit_error(&e), AppError::Uncertain(COMMIT_UNCERTAIN.into()));
        }
        assert_eq!(
            COMMIT_UNCERTAIN,
            "The connection failed while committing, so the save may or may not have been applied. Reload before saving again."
        );
    }

    #[test]
    fn txid_status_answers_map_to_a_commit_status() {
        assert_eq!(commit_status_from(Some("committed")), Some(CommitStatus::Committed));
        assert_eq!(commit_status_from(Some("aborted")), Some(CommitStatus::Aborted));
        assert_eq!(commit_status_from(Some("in progress")), None);
        assert_eq!(commit_status_from(None), Some(CommitStatus::Unknown));
        assert_eq!(commit_status_from(Some("something new")), Some(CommitStatus::Unknown));
    }

    #[test]
    fn save_timeouts_put_only_the_integer_setting_into_sql() {
        assert_eq!(
            save_timeouts_sql(30_000),
            "SET LOCAL lock_timeout = '5s'; SET LOCAL statement_timeout = 30000"
        );
    }

    #[test]
    fn statement_failure_without_a_code_names_the_change() {
        let e = statement_error("UPDATE id = 1", &sqlx::Error::PoolClosed);
        assert!(matches!(e, AppError::Query(m) if m.starts_with("UPDATE id = 1: ")));
    }
}
