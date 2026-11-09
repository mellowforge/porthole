//! MySQL and MariaDB, through `mysql_async`.
//!
//! The browse driver reads through a pool of `POOL_MAX` connections. Every connection, pooled or
//! not, runs the profile's startup SQL and then (for a read-only profile) `SET SESSION
//! TRANSACTION READ ONLY` as `mysql_async` setup statements, which run again each time the pool
//! resets a returned connection. Rows are always read through the text protocol (`text.rs`), so a
//! value has one text on every path (`values.rs`). Time limits are kept with `KILL QUERY` from a
//! second connection (`kill.rs`), never by abandoning a running statement.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mysql_async::prelude::Queryable;
use mysql_async::{
    Conn, DriverError, IoError, Opts, OptsBuilder, Params, Pool, PoolConstraints, PoolOpts, Row, SslOpts, Value,
};

use super::{Dialect, Driver, DEFAULT_COUNT_TIMEOUT_MS};
use crate::error::{AppError, AppResult};
use crate::export::RowSink;
use crate::model::*;
use crate::query::{QueryLanguage, QuerySession};
use crate::sqltext::{split_statements_in, SqlDialect};

mod catalog;
mod dialect;
mod edit;
mod export;
pub mod kill;
pub mod session;
pub mod text;
pub mod values;

pub use dialect::MysqlDialect;
pub use session::MysqlQuerySession;

/// Connections the browse pool keeps at most.
const POOL_MAX: usize = 4;
/// How long a pooled connection may sit unused before the pool closes it.
const POOL_IDLE: Duration = Duration::from_secs(600);
/// How long `close` waits for connections in use to come back before leaving them.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
const READ_ONLY_SQL: &str = "SET SESSION TRANSACTION READ ONLY";
/// Unfiltered tables estimated at this many rows or more aren't counted exactly (as on Postgres).
const EXACT_COUNT_LIMIT: u64 = 50_000;
/// How long closing an abandoned connection may take before it is left to the socket's close.
const DISCARD_WAIT: Duration = Duration::from_secs(2);

/// How every connection of a profile is opened.
#[derive(Clone)]
pub struct MysqlConfig {
    /// Without `client_found_rows`: query sessions get the server's normal affected-row counts.
    opts: Opts,
    /// `prefer`: TLS is tried as with `require`, and dropped if the server has none.
    prefer_tls: bool,
    timeout_secs: u32,
    host: String,
    port: u16,
    /// The profile's CA certificate file, read when a TLS connection is made.
    root_cert: Option<PathBuf>,
}

impl MysqlConfig {
    /// The options for the profile's server address, user, password and database (none when
    /// empty), TLS mode, startup SQL and read-only mode.
    pub fn from_profile(p: &ConnectionProfile, password: Option<String>) -> AppResult<MysqlConfig> {
        let net = p.network()?;
        let startup = p.startup_sql.as_deref().unwrap_or("");
        let chars: Vec<char> = startup.chars().collect();
        let mut setup: Vec<String> = split_statements_in(SqlDialect::Mysql, startup)
            .iter()
            .map(|s| chars[s.start..s.end].iter().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if p.read_only {
            setup.push(READ_ONLY_SQL.into());
        }
        // mysql_async runs the setup statements from the last to the first.
        setup.reverse();

        let root_cert = net.ssl_root_cert.as_deref().filter(|c| !c.trim().is_empty());
        let ssl = match net.ssl_mode {
            SslMode::Disable => None,
            SslMode::Prefer | SslMode::Require => Some(
                SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true),
            ),
            SslMode::VerifyCa => Some(SslOpts::default().with_danger_skip_domain_validation(true)),
            SslMode::VerifyFull => Some(SslOpts::default()),
        }
        .map(|ssl| match root_cert {
            Some(path) => ssl.with_root_certs(vec![PathBuf::from(path).into()]),
            None => ssl,
        });

        let opts = OptsBuilder::default()
            .ip_or_hostname(net.host.clone())
            .tcp_port(net.port)
            .user(Some(net.user.clone()))
            .pass(password)
            .db_name(Some(net.database.clone()).filter(|d| !d.is_empty()))
            // Connect where the profile says, never to a local socket the server names.
            .prefer_socket(false)
            .setup(setup)
            .ssl_opts(ssl);
        Ok(MysqlConfig {
            opts: opts.into(),
            prefer_tls: net.ssl_mode == SslMode::Prefer,
            timeout_secs: p.connect_timeout_secs.max(1),
            host: net.host.clone(),
            port: net.port,
            root_cert: root_cert.map(PathBuf::from),
        })
    }

    /// The options of a single connection: a query session's, or the one `KILL QUERY` runs on.
    pub fn opts(&self) -> Opts {
        self.opts.clone()
    }

    /// The browse pool's options: `opts`, with UPDATE counting the rows it matched rather than
    /// the rows it changed (`CLIENT_FOUND_ROWS`), so saving a cell's current value counts its row.
    pub fn for_pool(&self) -> Opts {
        let constraints = PoolConstraints::new(0, POOL_MAX).expect("valid pool bounds");
        let pool = PoolOpts::default().with_constraints(constraints).with_inactive_connection_ttl(POOL_IDLE);
        OptsBuilder::from_opts(self.opts.clone()).client_found_rows(true).pool_opts(pool).into()
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.into())
    }

    /// The same options without TLS.
    fn without_tls(&self) -> MysqlConfig {
        MysqlConfig {
            opts: OptsBuilder::from_opts(self.opts.clone()).ssl_opts(None::<SslOpts>).into(),
            prefer_tls: false,
            ..self.clone()
        }
    }

    /// Fails, naming the file, when a TLS connection's CA certificate can't be read: the client's
    /// own error says only that a file is missing.
    fn check_root_cert(&self) -> AppResult<()> {
        match &self.root_cert {
            Some(path) if self.opts.ssl_opts().is_some() => std::fs::File::open(path).map(drop).map_err(|e| {
                AppError::Connection(format!("Couldn't read the CA certificate {}: {e}", path.display()))
            }),
            _ => Ok(()),
        }
    }

    /// A new connection, or the failure; `Err` only when the connect timeout passed first, or
    /// the CA certificate can't be read.
    async fn attempt(&self) -> AppResult<mysql_async::Result<Conn>> {
        self.check_root_cert()?;
        tokio::time::timeout(self.timeout(), Conn::new(self.opts())).await.map_err(|_| {
            AppError::Connection(format!(
                "Timed out after {} s connecting to {}:{}",
                self.timeout_secs, self.host, self.port
            ))
        })
    }

    /// A new connection with `opts`, set up, within the connect timeout.
    pub async fn open(&self) -> AppResult<Conn> {
        self.attempt().await?.map_err(connection_error)
    }

    /// Opens the first connection. Under `prefer`, a server without TLS is connected to again
    /// without it, and the returned config keeps that choice for every later connection.
    async fn resolve(self) -> AppResult<(MysqlConfig, Conn)> {
        match self.attempt().await? {
            Ok(conn) => Ok((self, conn)),
            Err(e) if self.falls_back_without_tls(&e) => {
                let plain = self.without_tls();
                let conn = plain.open().await?;
                Ok((plain, conn))
            }
            Err(e) => Err(connection_error(e)),
        }
    }

    /// Whether a failed first connection is tried again without TLS: only under `prefer`, and
    /// only when the server said it has no TLS.
    fn falls_back_without_tls(&self, e: &mysql_async::Error) -> bool {
        self.prefer_tls && matches!(e, mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer))
    }
}

/// The message of a `mysql_async` error: the server's own text for a server error, the
/// client's otherwise.
pub(crate) fn error_message(e: &mysql_async::Error) -> String {
    match e {
        mysql_async::Error::Server(s) => s.message.clone(),
        mysql_async::Error::Driver(d) => d.to_string(),
        mysql_async::Error::Io(IoError::Io(io)) => io.to_string(),
        mysql_async::Error::Io(IoError::Tls(tls)) => tls.to_string(),
        mysql_async::Error::Other(o) => o.to_string(),
        mysql_async::Error::Url(u) => u.to_string(),
    }
}

/// A failure while connecting or setting a connection up.
pub(crate) fn connection_error(e: mysql_async::Error) -> AppError {
    AppError::Connection(error_message(&e))
}

/// A failure on an open connection: a server error is the statement's, anything else the
/// connection's.
pub(crate) fn query_error(e: mysql_async::Error) -> AppError {
    match e {
        mysql_async::Error::Server(s) => AppError::Query(s.message),
        other => connection_error(other),
    }
}

/// A failure of a filtered read. A value the column type rejects (SQLSTATE class 22) is a filter
/// error, shown inline under the filter bar, as on Postgres.
fn filter_query_error(e: mysql_async::Error) -> AppError {
    match e {
        mysql_async::Error::Server(s) if s.state.starts_with("22") => AppError::Filter { message: s.message, position: 0 },
        other => query_error(other),
    }
}

/// The connection's warnings from its last statement (`SHOW WARNINGS`): each one's code and
/// message. Read as bytes, so a message in another encoding can't fail the read.
async fn warning_codes(conn: &mut Conn) -> mysql_async::Result<Vec<(u16, String)>> {
    // Level, Code, Message.
    let rows: Vec<Row> = conn.query("SHOW WARNINGS").await?;
    Ok(rows
        .iter()
        .map(|r| {
            let code = match r.as_ref(1) {
                Some(Value::Bytes(b)) => std::str::from_utf8(b).ok().and_then(|c| c.parse().ok()),
                Some(Value::Int(i)) => u16::try_from(*i).ok(),
                Some(Value::UInt(u)) => u16::try_from(*u).ok(),
                _ => None,
            };
            let message = match r.as_ref(2) {
                Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
                _ => String::new(),
            };
            (code.unwrap_or(0), message)
        })
        .collect())
}

/// Warnings a value the column's type can't take raises in a comparison: 1292 (incorrect or
/// truncated value), 1366 (incorrect value), 1265 (data truncated).
const FILTER_CONVERSION_WARNINGS: [u16; 3] = [1292, 1366, 1265];

/// After a filtered read on `conn`: a value the column's type can't take is only a
/// warning on MySQL, and the comparison goes on with whatever the server made of it (`0`), so
/// it would match the wrong rows. Such a warning is the filter's inline error instead.
async fn filter_warnings(conn: &mut Conn) -> AppResult<()> {
    if conn.get_warnings() == 0 {
        return Ok(());
    }
    let warnings = warning_codes(conn).await.map_err(query_error)?;
    match warnings.into_iter().find(|(code, _)| FILTER_CONVERSION_WARNINGS.contains(code)) {
        Some((_, message)) => Err(AppError::Filter { message, position: 0 }),
        None => Ok(()),
    }
}

/// Closes a connection whose statement was abandoned (`kill::Bounded::Abandoned`), or whose save
/// couldn't be rolled back, or whose export was stopped. It must not go back to the pool, whose
/// reset would read the rest of that statement's result. `disconnect` marks it closed at once, so
/// even if closing stalls, the pool discards it.
fn discard(conn: Conn) {
    let id = conn.id();
    tokio::spawn(async move {
        match tokio::time::timeout(DISCARD_WAIT, conn.disconnect()).await {
            Ok(Ok(())) => {}
            // The statement left on it ended as stopped by `KILL QUERY`; the socket is closed.
            Ok(Err(mysql_async::Error::Server(s))) if s.code == kill::ER_QUERY_INTERRUPTED => {}
            Ok(Err(e)) => eprintln!("closing abandoned MySQL connection {id} failed: {}", error_message(&e)),
            Err(_) => eprintln!("closing abandoned MySQL connection {id} took over {DISCARD_WAIT:?}; dropped"),
        }
    });
}

struct BuiltSelect {
    /// SELECT list, FROM, WHERE and ORDER BY; no LIMIT or OFFSET.
    sql: String,
    /// `` `db`.`table` ``.
    from: String,
    where_sql: Option<String>,
    params: Vec<String>,
    /// The `data_type` of each of `params`' columns.
    param_types: Vec<String>,
}

impl BuiltSelect {
    /// The filter's values, each bound by its column (`text::to_column_value`).
    fn values(&self) -> Vec<Value> {
        self.params.iter().zip(&self.param_types).map(|(p, t)| text::to_column_value(Some(p), t)).collect()
    }
}

/// What breaks ties after the user's sort, so that paging sees every row once. (MySQL has no
/// row-id column to fall back on, unlike Postgres' `ctid` and SQLite's `rowid`.)
#[derive(Debug, Clone, PartialEq)]
enum Tiebreak {
    /// Columns that are unique together, ascending.
    Columns(Vec<String>),
    /// Nothing: rows equal on the sort have no fixed order.
    None,
}

/// The grid's SELECT: every column in table order, as stored (`values::cell` reads each value by
/// its result column), the compiled filter, and the sort with NULLs last both ways (as on
/// Postgres), then the primary key ascending as tiebreak (or alone without a sort).
fn build_select(d: &MysqlDialect, columns: &[ColumnInfo], q: &TableQuery) -> AppResult<BuiltSelect> {
    let key = columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect();
    build_page_select(d, columns, q, &Tiebreak::Columns(key))
}

/// `build_select` with `tiebreak` after the sort in place of the primary key.
fn build_page_select(d: &MysqlDialect, columns: &[ColumnInfo], q: &TableQuery, tiebreak: &Tiebreak) -> AppResult<BuiltSelect> {
    let from = format!("{}.{}", d.quote_ident(&q.table.schema), d.quote_ident(&q.table.name));
    let filter = q.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
    let (where_sql, params, param_types) = match filter {
        Some(f) => {
            let expr = crate::filter::parse(f)?;
            let (sql, params, types) = crate::filter::compile_typed(&expr, columns, d, 1)?;
            (Some(sql), params, types)
        }
        None => (None, Vec::new(), Vec::new()),
    };

    let key_terms = |skip: Option<&str>| -> Vec<String> {
        match tiebreak {
            Tiebreak::Columns(names) => {
                names.iter().filter(|n| Some(n.as_str()) != skip).map(|n| format!("{} ASC", d.quote_ident(n))).collect()
            }
            Tiebreak::None => Vec::new(),
        }
    };
    let terms: Vec<String> = match &q.sort {
        Some(s) => {
            let col = columns
                .iter()
                .find(|c| c.name == s.column)
                .ok_or_else(|| AppError::Query(format!("Unknown column {}", s.column)))?;
            let quoted = d.quote_ident(&col.name);
            let dir = if s.descending { "DESC" } else { "ASC" };
            let mut terms = vec![format!("{quoted} IS NULL"), format!("{quoted} {dir}")];
            terms.extend(key_terms(Some(&col.name)));
            terms
        }
        None => key_terms(None),
    };

    let select_list = columns.iter().map(|c| d.quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
    let mut sql = format!("SELECT {select_list} FROM {from}");
    if let Some(w) = &where_sql {
        sql.push_str(&format!(" WHERE {w}"));
    }
    if !terms.is_empty() {
        sql.push_str(&format!(" ORDER BY {}", terms.join(", ")));
    }
    Ok(BuiltSelect { sql, from, where_sql, params, param_types })
}

/// The tie-break of a table without a primary key, from its unique indexes' key parts
/// (`catalog::unique_key_parts`, by index then part): the unique index with the fewest parts,
/// then by name, whose parts all name a NOT NULL column; else none.
fn keyless_tiebreak(parts: &[catalog::KeyPart]) -> Tiebreak {
    let mut indexes: Vec<(&str, Option<Vec<String>>)> = Vec::new();
    for part in parts {
        if indexes.last().is_none_or(|(name, _)| *name != part.index) {
            indexes.push((&part.index, Some(Vec::new())));
        }
        // `None` once a part is functional or nullable: the index can't order the rows.
        let (_, columns) = indexes.last_mut().expect("pushed above");
        if let (Some(c), true, Some(cs)) = (&part.column, part.not_null, columns.as_mut()) {
            cs.push(c.clone());
        } else {
            *columns = None;
        }
    }
    indexes
        .into_iter()
        .filter_map(|(name, columns)| columns.map(|cs| (cs.len(), name, cs)))
        .min_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)))
        .map_or(Tiebreak::None, |(_, _, cs)| Tiebreak::Columns(cs))
}

/// Rows as the grid's text; `None` is NULL.
type GridRows = Vec<Vec<Option<String>>>;

/// The rows of `sql` with `values`, through the text protocol: each value's text and each row's
/// cell types (`row_types`).
async fn read_rows(conn: &mut Conn, sql: &str, values: Vec<Value>) -> mysql_async::Result<(GridRows, Vec<String>)> {
    let mut result = text::query_values(conn, sql, values).await?;
    let rows: Vec<Row> = result.collect().await?;
    result.drop_result().await?;
    let (mut texts, mut types) = (Vec::with_capacity(rows.len()), Vec::with_capacity(rows.len()));
    for row in &rows {
        let columns = row.columns_ref();
        let (row_texts, row_cell_types): (Vec<Option<String>>, Vec<CellType>) =
            (0..row.len()).map(|i| values::cell(row.as_ref(i).unwrap_or(&Value::NULL), &columns[i])).unzip();
        texts.push(row_texts);
        types.push(row_types(&row_cell_types));
    }
    Ok((texts, types))
}

/// The server version as the app shows it: `MySQL 8.4.13`, or `MariaDB 11.4.13` for MariaDB's
/// `11.4.13-MariaDB-ubu2404`.
fn display_version(version: &str) -> String {
    match version.find("-MariaDB") {
        Some(end) => format!("MariaDB {}", &version[..end]),
        None => format!("MySQL {version}"),
    }
}

pub struct MysqlDriver {
    pool: Pool,
    config: MysqlConfig,
    dialect: MysqlDialect,
    /// How long a row count may run before it is stopped and shown as unknown, in ms.
    count_timeout_ms: u64,
    /// `DATABASE()` on the first connection, after its setup: the server's own spelling of the
    /// profile's database, or the one the startup SQL chose.
    database: Option<String>,
}

/// Opens the profile's first connection within its connect timeout, then the browse pool.
/// Every failure (address, TLS, login, startup SQL) is an `AppError::Connection` with the
/// server's or the client's message.
pub async fn connect_mysql(profile: &ConnectionProfile, password: Option<String>) -> AppResult<MysqlDriver> {
    // A direct connection first: it reports the real cause of a failure, and settles `prefer`.
    let (config, mut probe) = MysqlConfig::from_profile(profile, password)?.resolve().await?;
    let database: mysql_async::Result<Option<Option<String>>> = probe.query_first("SELECT DATABASE()").await;
    probe.disconnect().await.ok();
    Ok(MysqlDriver {
        pool: Pool::new(config.for_pool()),
        config,
        dialect: MysqlDialect,
        count_timeout_ms: DEFAULT_COUNT_TIMEOUT_MS,
        database: database.map_err(connection_error)?.flatten(),
    })
}

impl MysqlDriver {
    /// The browse pool. Test support.
    #[doc(hidden)]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// The options every connection of this driver is opened with. Test support.
    #[doc(hidden)]
    pub fn config(&self) -> &MysqlConfig {
        &self.config
    }

    /// Overrides the row-count timeout (default 3000 ms). Test support.
    #[doc(hidden)]
    pub fn set_count_timeout_ms(&mut self, ms: u64) {
        self.count_timeout_ms = ms;
    }

    /// `count(*)` stopped with `KILL QUERY` after `count_timeout_ms`: `None` then. A count that
    /// couldn't be stopped is `None` too, and its connection is closed (`discard`). The count is
    /// one round trip, so the KILL lands on it. `map_err` maps the count's own failures; a
    /// `filtered` count's conversion warnings are its error (`filter_warnings`).
    async fn bounded_count(
        &self,
        sql: String,
        values: Vec<Value>,
        map_err: fn(mysql_async::Error) -> AppError,
        filtered: bool,
    ) -> AppResult<Option<u64>> {
        let mut conn = self.conn().await?;
        let limit = Duration::from_millis(self.count_timeout_ms);
        let counted = kill::bounded(&self.config, &mut conn, limit, move |c| {
            Box::pin(async move {
                let n: Option<u64> = if values.is_empty() {
                    c.query_first(sql).await?
                } else {
                    c.exec_first(sql, Params::Positional(values)).await?
                };
                Ok(n.unwrap_or(0))
            })
        })
        .await;
        match counted {
            Ok(n) => {
                if filtered {
                    filter_warnings(&mut conn).await?;
                }
                Ok(Some(n))
            }
            Err(kill::Bounded::TimedOut) => Ok(None),
            Err(kill::Bounded::Abandoned) => {
                discard(conn);
                Ok(None)
            }
            Err(kill::Bounded::Failed(e)) => Err(map_err(e)),
        }
    }

    /// As on Postgres: `Estimated(TABLE_ROWS)` at `EXACT_COUNT_LIMIT` rows or more; otherwise a
    /// bounded exact count, and when that is stopped, the estimate if there is one, else `Unknown`.
    /// A view has no estimate.
    async fn total_count(&self, table: &TableRef, from: &str) -> AppResult<RowCount> {
        let estimate = catalog::estimated_rows(&mut self.conn().await?, table).await?.filter(|n| *n > 0);
        if let Some(est) = estimate.filter(|n| *n >= EXACT_COUNT_LIMIT) {
            return Ok(RowCount::Estimated(est));
        }
        Ok(match self.bounded_count(format!("SELECT count(*) FROM {from}"), Vec::new(), query_error, false).await? {
            Some(n) => RowCount::Exact(n),
            None => estimate.map_or(RowCount::Unknown, RowCount::Estimated),
        })
    }

    /// The filtered rows, counted as in `bounded_count`; `Unknown` when stopped.
    async fn filtered_count(&self, from: &str, where_sql: &str, values: Vec<Value>) -> AppResult<RowCount> {
        let sql = format!("SELECT count(*) FROM {from} WHERE {where_sql}");
        Ok(match self.bounded_count(sql, values, filter_query_error, true).await? {
            Some(n) => RowCount::Exact(n),
            None => RowCount::Unknown,
        })
    }

    /// A pooled connection, waited for at most the connect timeout.
    async fn conn(&self) -> AppResult<Conn> {
        match tokio::time::timeout(self.config.timeout(), self.pool.get_conn()).await {
            Ok(conn) => conn.map_err(connection_error),
            Err(_) => Err(AppError::Connection(format!(
                "Timed out after {} s waiting for a connection to {}:{}",
                self.config.timeout_secs, self.config.host, self.config.port
            ))),
        }
    }
}

#[async_trait]
impl Driver for MysqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::Mysql
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }

    fn query_language(&self) -> QueryLanguage {
        QueryLanguage::Sql(SqlDialect::Mysql)
    }

    async fn server_version(&self) -> AppResult<String> {
        let mut conn = self.conn().await?;
        let version: Option<String> = conn.query_first("SELECT VERSION()").await.map_err(query_error)?;
        let version = version.ok_or_else(|| AppError::Query("The server reported no version.".into()))?;
        Ok(display_version(&version))
    }

    /// The server's databases are the app's schemas, so there is no database list.
    async fn list_databases(&self) -> AppResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// The server's databases, without its own (`information_schema`, `mysql`, …).
    async fn list_schemas(&self, _database: Option<&str>) -> AppResult<Vec<String>> {
        catalog::list_schemas(&mut self.conn().await?).await
    }

    async fn schema_tree(&self, schema: &str) -> AppResult<SchemaTree> {
        catalog::schema_tree(&mut self.conn().await?, schema).await
    }

    async fn table_structure(&self, table: &TableRef) -> AppResult<TableStructure> {
        catalog::table_structure(&mut self.conn().await?, table).await
    }

    async fn fetch_page(&self, req: &PageRequest) -> AppResult<TablePage> {
        let structure = self.table_structure(&req.table).await?;
        let tq = TableQuery { table: req.table.clone(), filter: req.filter.clone(), sort: req.sort.clone() };
        let (built, unstable_order) = if structure.primary_key.is_empty() {
            let parts = catalog::unique_key_parts(&mut self.conn().await?, &req.table).await?;
            let tiebreak = keyless_tiebreak(&parts);
            (build_page_select(&self.dialect, &structure.columns, &tq, &tiebreak)?, tiebreak == Tiebreak::None)
        } else {
            (build_select(&self.dialect, &structure.columns, &tq)?, false)
        };
        let page_size = u64::from(req.page_size.clamp(1, 1000));
        let offset = u64::from(req.page) * page_size;
        let sql = format!("{} LIMIT ? OFFSET ?", built.sql);
        // The filter's values bind by their columns; LIMIT and OFFSET need integers.
        let mut values = built.values();
        values.extend([page_size, offset].map(|n| text::to_value(Some(&n.to_string()))));

        let mut conn = self.conn().await?;
        let started = Instant::now();
        let read = read_rows(&mut conn, &sql, values).await;
        let (rows, types) = read.map_err(|e| if built.where_sql.is_some() { filter_query_error(e) } else { query_error(e) })?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if built.where_sql.is_some() {
            filter_warnings(&mut conn).await?;
        }
        drop(conn);

        let total = self.total_count(&req.table, &built.from).await?;
        let matches = match &built.where_sql {
            Some(w) => Some(self.filtered_count(&built.from, w, built.values()).await?),
            None => None,
        };
        Ok(TablePage {
            columns: structure.columns,
            primary_key: structure.primary_key,
            editing: structure.editing,
            rows,
            total,
            matches,
            elapsed_ms,
            cell_types: Some(types),
            unstable_order,
        })
    }

    async fn export_table(&self, q: &TableQuery, sink: &mut dyn RowSink) -> AppResult<()> {
        export::export_table(self, q, sink).await
    }

    async fn apply_changes(&self, set: &ChangeSet) -> AppResult<ApplyResult> {
        edit::apply_changes(self, set).await
    }

    async fn schema_columns(&self, _database: Option<&str>, schema: &str) -> AppResult<BTreeMap<String, Vec<String>>> {
        catalog::schema_columns(&mut self.conn().await?, schema).await
    }

    /// A session on the profile's database; the tab's database is then chosen with
    /// `set_search_path` (`USE`), so a tab never opens a session on another one.
    async fn open_query_session(&self, database: Option<&str>) -> AppResult<Arc<dyn QuerySession>> {
        if database.is_some() {
            return Err(AppError::Unsupported("MySQL tabs switch databases with the Database list.".into()));
        }
        Ok(Arc::new(MysqlQuerySession::open(&self.config).await?))
    }

    fn connected_database(&self) -> Option<String> {
        self.database.clone()
    }

    async fn close(&self) {
        // Waits for connections in use to come back; a stuck one is left to close by itself.
        if tokio::time::timeout(CLOSE_WAIT, self.pool.clone().disconnect()).await.is_err() {
            eprintln!("a MySQL connection to {}:{} was still in use at close", self.config.host, self.config.port);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(ssl_mode: SslMode, database: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: "m".into(),
            name: "m".into(),
            kind: DbKind::Mysql,
            environment: Environment::Local,
            default_schema: None,
            connect_timeout_secs: 5,
            read_only: false,
            startup_sql: None,
            statement_timeout_secs: None,
            transport: Transport::Network(NetworkTarget {
                host: "db.example".into(),
                port: 3306,
                user: "u".into(),
                database: database.into(),
                ssl_mode,
                ssl_root_cert: None,
                auth_source: None,
            }),
        }
    }

    #[test]
    fn mysql_prefer_falls_back_once_to_no_tls() {
        let no_tls = || mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer);
        let prefer = MysqlConfig::from_profile(&profile(SslMode::Prefer, "fx"), None).unwrap();
        assert!(prefer.opts().ssl_opts().is_some());
        assert!(prefer.falls_back_without_tls(&no_tls()));
        // Other failures, and other modes, are reported as they are.
        assert!(!prefer.falls_back_without_tls(&mysql_async::Error::Driver(DriverError::ConnectionClosed)));
        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            let cfg = MysqlConfig::from_profile(&profile(mode.clone(), "fx"), None).unwrap();
            assert!(!cfg.falls_back_without_tls(&no_tls()), "{mode:?}");
        }
        // The fallback config has no TLS and never falls back again; the rest is unchanged.
        let plain = prefer.without_tls();
        assert!(plain.opts().ssl_opts().is_none());
        assert!(!plain.falls_back_without_tls(&no_tls()));
        assert_eq!(plain.opts().db_name(), prefer.opts().db_name());
        assert_eq!(plain.opts().setup(), prefer.opts().setup());
        assert!(!plain.opts().prefer_socket());
        assert!(plain.for_pool().ssl_opts().is_none() && plain.for_pool().client_found_rows());
    }

    #[test]
    fn mysql_build_select_sorts_nulls_last_then_by_key() {
        let col = |name: &str, pk: bool| ColumnInfo {
            name: name.into(),
            data_type: "int".into(),
            full_type: "int".into(),
            cast_type: "int".into(),
            nullable: true,
            default: None,
            is_primary_key: pk,
            read_only: false,
        };
        let columns = vec![col("id", true), col("k2", true), col("plan", false)];
        let q = |filter: Option<&str>, sort: Option<(&str, bool)>| TableQuery {
            table: TableRef { schema: "fx".into(), name: "Order Items".into() },
            filter: filter.map(String::from),
            sort: sort.map(|(c, d)| SortSpec { column: c.into(), descending: d }),
        };
        let built = build_select(&MysqlDialect, &columns, &q(None, None)).unwrap();
        assert_eq!(built.sql, "SELECT `id`, `k2`, `plan` FROM `fx`.`Order Items` ORDER BY `id` ASC, `k2` ASC");
        assert_eq!(built.from, "`fx`.`Order Items`");
        let built = build_select(&MysqlDialect, &columns, &q(Some("plan = 1"), Some(("plan", true)))).unwrap();
        assert_eq!(
            built.sql,
            "SELECT `id`, `k2`, `plan` FROM `fx`.`Order Items` WHERE `plan` = ? \
             ORDER BY `plan` IS NULL, `plan` DESC, `id` ASC, `k2` ASC"
        );
        assert_eq!((built.where_sql.as_deref(), built.params), (Some("`plan` = ?"), vec!["1".to_string()]));
        // A key column as the sort isn't repeated as a tiebreak.
        let built = build_select(&MysqlDialect, &columns, &q(None, Some(("k2", false)))).unwrap();
        assert!(built.sql.ends_with("ORDER BY `k2` IS NULL, `k2` ASC, `id` ASC"), "{}", built.sql);
    }

    #[test]
    fn page_order_tiebreaks_in_the_page_select() {
        let col = |name: &str, pk: bool| ColumnInfo {
            name: name.into(),
            data_type: "int".into(),
            full_type: "int".into(),
            cast_type: "int".into(),
            nullable: true,
            default: None,
            is_primary_key: pk,
            read_only: false,
        };
        let q = |filter: Option<&str>, sort: Option<(&str, bool)>| TableQuery {
            table: TableRef { schema: "fx".into(), name: "t".into() },
            filter: filter.map(String::from),
            sort: sort.map(|(c, d)| SortSpec { column: c.into(), descending: d }),
        };
        let columns = vec![col("a", false), col("b", false)];
        let order_of = |sort, tiebreak: &Tiebreak| {
            let sql = build_page_select(&MysqlDialect, &columns, &q(None, sort), tiebreak).unwrap().sql;
            assert!(sql.starts_with("SELECT `a`, `b` FROM `fx`.`t`"), "{sql}");
            sql.split_once(" ORDER BY ").map_or(String::new(), |(_, o)| o.to_string())
        };
        let ab = Tiebreak::Columns(vec!["a".into(), "b".into()]);
        assert_eq!(order_of(None, &ab), "`a` ASC, `b` ASC");
        assert_eq!(order_of(Some(("a", true)), &ab), "`a` IS NULL, `a` DESC, `b` ASC");
        assert_eq!(order_of(None, &Tiebreak::None), "");
        assert_eq!(order_of(Some(("b", false)), &Tiebreak::None), "`b` IS NULL, `b` ASC");

        // `build_select` is the page select with the primary key (in column order) as tie-break.
        let keyed = vec![col("k2", true), col("plan", false), col("k1", true)];
        let filtered = q(Some("plan = 1"), Some(("plan", false)));
        let built = build_select(&MysqlDialect, &keyed, &filtered).unwrap();
        assert_eq!(
            built.sql,
            "SELECT `k2`, `plan`, `k1` FROM `fx`.`t` WHERE `plan` = ? ORDER BY `plan` IS NULL, `plan` ASC, `k2` ASC, `k1` ASC"
        );
        let key = Tiebreak::Columns(vec!["k2".into(), "k1".into()]);
        assert_eq!(build_page_select(&MysqlDialect, &keyed, &filtered, &key).unwrap().sql, built.sql);
    }

    #[test]
    fn page_order_without_a_primary_key_uses_a_unique_not_null_index() {
        let part = |index: &str, column: Option<&str>, not_null: bool| catalog::KeyPart {
            index: index.into(),
            column: column.map(String::from),
            not_null,
        };
        let names = |v: &[&str]| Tiebreak::Columns(v.iter().map(|s| s.to_string()).collect());
        assert_eq!(keyless_tiebreak(&[]), Tiebreak::None);
        assert_eq!(keyless_tiebreak(&[part("u", Some("a"), true)]), names(&["a"]));
        // Fewest parts first, then by name.
        let parts = [
            part("a_two", Some("a"), true),
            part("a_two", Some("b"), true),
            part("z_one", Some("c"), true),
            part("y_one", Some("d"), true),
        ];
        assert_eq!(keyless_tiebreak(&parts), names(&["d"]));
        let parts = [part("b", Some("x"), true), part("a", Some("y"), true)];
        assert_eq!(keyless_tiebreak(&parts), names(&["y"]));
        // A nullable part or a functional part rules the index out.
        let parts = [
            part("n", Some("a"), false),
            part("f", None, false),
            part("two", Some("a"), true),
            part("two", Some("b"), true),
        ];
        assert_eq!(keyless_tiebreak(&parts), names(&["a", "b"]));
        let parts = [part("mixed", Some("a"), true), part("mixed", None, false)];
        assert_eq!(keyless_tiebreak(&parts), Tiebreak::None);
    }

    #[test]
    fn mysql_display_version() {
        assert_eq!(display_version("8.4.13"), "MySQL 8.4.13");
        assert_eq!(display_version("11.4.13-MariaDB-ubu2404"), "MariaDB 11.4.13");
        assert_eq!(display_version("10.6.21-MariaDB-log"), "MariaDB 10.6.21");
    }

    #[test]
    fn mysql_setup_runs_startup_sql_in_order_then_read_only() {
        let mut p = profile(SslMode::Disable, "fx");
        p.read_only = true;
        p.startup_sql = Some("SET @a = 1;\n\nDELIMITER //\nSET @b = 2//\n".into());
        let cfg = MysqlConfig::from_profile(&p, Some("pw".into())).unwrap();
        // mysql_async runs `setup` from the last entry to the first.
        assert_eq!(cfg.opts().setup(), ["SET SESSION TRANSACTION READ ONLY", "SET @b = 2", "SET @a = 1"]);
        assert_eq!(cfg.opts().db_name(), Some("fx"));
        assert_eq!(cfg.opts().pass(), Some("pw"));
        assert!(!cfg.opts().prefer_socket());
        p.read_only = false;
        p.startup_sql = Some("  ".into());
        assert!(MysqlConfig::from_profile(&p, None).unwrap().opts().setup().is_empty());
    }

    #[test]
    fn mysql_found_rows_only_for_the_pool() {
        let cfg = MysqlConfig::from_profile(&profile(SslMode::Disable, ""), None).unwrap();
        assert!(!cfg.opts().client_found_rows());
        assert!(cfg.for_pool().client_found_rows());
        assert_eq!(cfg.for_pool().pool_opts().constraints().max(), 4);
        // An empty database is no default database.
        assert_eq!(cfg.opts().db_name(), None);
    }

    /// A CA certificate file that isn't there fails the connect with an error naming it, before
    /// any connection is tried.
    #[tokio::test]
    async fn mysql_a_missing_root_cert_is_named() {
        let mut p = profile(SslMode::VerifyCa, "fx");
        if let Transport::Network(n) = &mut p.transport {
            n.host = "127.0.0.1".into();
            n.port = 1;
            n.ssl_root_cert = Some("/nonexistent/ts-ca.pem".into());
        }
        match connect_mysql(&p, None).await {
            Err(AppError::Connection(m)) => assert!(m.contains("/nonexistent/ts-ca.pem"), "{m}"),
            Err(e) => panic!("expected a connection error, got {e:?}"),
            Ok(_) => panic!("expected a connection error"),
        }
    }

    #[test]
    fn mysql_tls_modes_map_to_ssl_opts() {
        let ssl = |mode| MysqlConfig::from_profile(&profile(mode, "fx"), None).unwrap().opts().ssl_opts().cloned();
        assert!(ssl(SslMode::Disable).is_none());
        for mode in [SslMode::Require, SslMode::Prefer] {
            let s = ssl(mode).unwrap();
            assert!(s.accept_invalid_certs() && s.skip_domain_validation());
        }
        let ca = ssl(SslMode::VerifyCa).unwrap();
        assert!(!ca.accept_invalid_certs() && ca.skip_domain_validation());
        let full = ssl(SslMode::VerifyFull).unwrap();
        assert!(!full.accept_invalid_certs() && !full.skip_domain_validation());
        let mut p = profile(SslMode::VerifyFull, "fx");
        if let Transport::Network(n) = &mut p.transport {
            n.ssl_root_cert = Some("/tmp/ca.pem".into());
        }
        let cfg = MysqlConfig::from_profile(&p, None).unwrap();
        assert_eq!(cfg.opts().ssl_opts().unwrap().root_certs().len(), 1);
    }
}
