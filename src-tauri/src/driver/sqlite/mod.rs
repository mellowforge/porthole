//! SQLite: one database file, read and written by connection workers (`worker.rs`), each a
//! connection on its own thread. The browse driver keeps `BROWSE_WORKERS` of them, and replaces
//! one whose thread has ended (a job panicked, or a save reset it) when it is next used.
//!
//! The app never changes the file by itself: files are opened without `SQLITE_OPEN_CREATE`,
//! closing never checkpoints a WAL file, and only the user's statements, grid saves and startup
//! SQL write. Foreign keys are on unless the startup SQL turns them off (`worker.rs`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, ErrorCode};

use super::{Dialect, Driver, DEFAULT_COUNT_TIMEOUT_MS};
use crate::error::{AppError, AppResult};
use crate::export::RowSink;
use crate::model::*;
use crate::query::{QueryLanguage, QuerySession};
use crate::sqltext::SqlDialect;

mod catalog;
mod dialect;
mod edit;
mod export;
mod session;
pub mod values;
mod worker;

pub use dialect::SqliteDialect;
pub use session::SqliteQuerySession;
pub use worker::SqliteWorker;

/// How long a statement waits for another connection's lock before failing with `SQLITE_BUSY`.
pub(super) const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections the browse driver keeps, so two page loads can overlap.
pub(super) const BROWSE_WORKERS: usize = 2;
/// How long `close` waits for running work before leaving the connections to close by themselves.
pub(super) const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// What a worker needs to open its connection.
#[derive(Clone, Debug, PartialEq)]
pub struct SqliteConfig {
    pub(crate) path: PathBuf,
    pub(crate) read_only: bool,
    /// Runs on every new connection; `None` when the profile's is empty.
    pub(crate) startup_sql: Option<String>,
}

impl SqliteConfig {
    /// The profile's file and settings. A profile without a file, or with an empty path, is
    /// refused.
    pub fn from_profile(p: &ConnectionProfile) -> AppResult<SqliteConfig> {
        let Transport::File { file_path } = &p.transport else {
            return Err(AppError::Connection("This connection has no database file.".into()));
        };
        if file_path.trim().is_empty() {
            return Err(AppError::Connection("Choose a database file.".into()));
        }
        Ok(SqliteConfig {
            path: PathBuf::from(file_path),
            read_only: p.read_only,
            startup_sql: p.startup_sql.clone().filter(|s| !s.trim().is_empty()),
        })
    }
}

pub struct SqliteDriver {
    config: SqliteConfig,
    /// The browse workers. A slot's worker is replaced when it is found closed (`worker`).
    slots: Vec<Mutex<Arc<SqliteWorker>>>,
    /// Held while a slot's worker is replaced, and by `close`, so a replacement never outlives
    /// the driver's close.
    replacing: tokio::sync::Mutex<()>,
    /// Set by `close`: no worker is replaced from then on, and every request fails.
    closed: AtomicBool,
    /// Counts the requests handed out, so they take the workers in turn (`worker`).
    next_worker: AtomicUsize,
    /// How long a row count may run before it is given up as `RowCount::Unknown`, in ms.
    count_timeout_ms: u64,
}

/// Opens the profile's file with `BROWSE_WORKERS` connections. Every failure (a missing file,
/// opening, the startup SQL) is an `AppError::Connection` with the underlying message.
pub async fn connect_sqlite(profile: &ConnectionProfile) -> AppResult<SqliteDriver> {
    let config = SqliteConfig::from_profile(profile)?;
    let cfg = config.clone();
    // Opening and the startup SQL block (a locked file waits out the busy timeout).
    let workers = tokio::task::spawn_blocking(move || {
        (0..BROWSE_WORKERS).map(|_| SqliteWorker::open(&cfg)).collect::<AppResult<Vec<_>>>()
    })
    .await
    .map_err(|e| AppError::Connection(e.to_string()))??;
    Ok(SqliteDriver {
        config,
        slots: workers.into_iter().map(|w| Mutex::new(Arc::new(w))).collect(),
        replacing: tokio::sync::Mutex::new(()),
        closed: AtomicBool::new(false),
        next_worker: AtomicUsize::new(0),
        count_timeout_ms: DEFAULT_COUNT_TIMEOUT_MS,
    })
}

impl SqliteDriver {
    /// The browse connections as they are now. Test support.
    #[doc(hidden)]
    pub fn workers(&self) -> Vec<Arc<SqliteWorker>> {
        (0..self.slots.len()).map(|n| self.slot(n)).collect()
    }

    /// The file and settings every connection of this driver is opened with. Test support.
    #[doc(hidden)]
    pub fn config(&self) -> &SqliteConfig {
        &self.config
    }

    /// Overrides the row-count timeout (default 3000 ms). Test support.
    #[doc(hidden)]
    pub fn set_count_timeout_ms(&mut self, ms: u64) {
        self.count_timeout_ms = ms;
    }

    fn slot(&self, n: usize) -> Arc<SqliteWorker> {
        self.slots[n].lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The worker for the next request: round-robin over the browse workers. A request (a page
    /// with its structure, rows and counts; a catalog read) runs on one worker, so two requests
    /// in flight run on the two connections at once, and a page stuck in a slow count doesn't
    /// hold up the next page. (Each worker runs its jobs one at a time, in order.)
    ///
    /// A worker whose thread has ended is replaced by a new connection, opened from `config`,
    /// so the request works. Once the driver is closed, every request fails as closed.
    async fn worker(&self) -> AppResult<Arc<SqliteWorker>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(worker::closed());
        }
        let n = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        let current = self.slot(n);
        if !current.is_closed() {
            return Ok(current);
        }
        let _replacing = self.replacing.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return Err(worker::closed());
        }
        // Another request may have replaced it meanwhile.
        let current = self.slot(n);
        if !current.is_closed() {
            return Ok(current);
        }
        eprintln!("a SQLite browse connection to {} had ended; opening a new one", self.config.path.display());
        let fresh = Arc::new(SqliteWorker::open_async(&self.config).await?);
        *self.slots[n].lock().unwrap_or_else(|e| e.into_inner()) = fresh.clone();
        Ok(fresh)
    }

    /// Runs `f` with a browse worker's connection.
    async fn on_worker<T, F>(&self, f: F) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> AppResult<T> + Send + 'static,
    {
        self.worker().await?.call(f).await?
    }

    /// `count(*)` given up after `count_timeout_ms`: then the dropped call interrupts its own
    /// statement (`SqliteWorker::call`) and the count is `Unknown`. `map_err` maps other failures.
    async fn bounded_count(
        &self,
        worker: &SqliteWorker,
        sql: String,
        params: Vec<String>,
        map_err: fn(rusqlite::Error) -> AppError,
    ) -> AppResult<RowCount> {
        let count = worker.call(move |c| c.query_row(&sql, params_from_iter(params), |r| r.get::<_, i64>(0)));
        match tokio::time::timeout(Duration::from_millis(self.count_timeout_ms), count).await {
            Err(_elapsed) => Ok(RowCount::Unknown),
            Ok(done) => match done? {
                Ok(n) => Ok(RowCount::Exact(n.max(0) as u64)),
                Err(e) if e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted) => Ok(RowCount::Unknown),
                Err(e) => Err(map_err(e)),
            },
        }
    }
}

/// A SQLite failure as `AppError::Query`, with SQLite's message.
pub(super) fn query_error(e: rusqlite::Error) -> AppError {
    AppError::Query(e.to_string())
}

/// SQLite's messages for a filter it can't run: the filter's doing, whatever the table.
const FILTER_MESSAGES: [&str; 4] =
    ["LIKE or GLOB pattern too complex", "too many SQL variables", "Expression tree is too large", "parser stack overflow"];

/// Whether a failure of a filtered statement is the filter's doing, so it is a filter error,
/// shown inline under the filter bar, as on Postgres; otherwise it is the query error the
/// unfiltered page would report. SQLite converts rather than rejects values, so filter errors
/// are:
/// - `SQLITE_MISMATCH`, `SQLITE_TOOBIG`, and a generic `SQLITE_ERROR` with one of
///   `FILTER_MESSAGES`;
/// - any other generic `SQLITE_ERROR` only when `unfiltered_ok()` says the same page without the
///   filter loads. A broken view (or one failing on its rows) fails without the filter too, and
///   is not blamed on it.
///
/// Busy, locked, interrupted, I/O and other failures stay query errors, and `unfiltered_ok` isn't
/// called for them.
fn filter_failure(e: rusqlite::Error, unfiltered_ok: impl FnOnce() -> bool) -> AppError {
    let filter_error = |e: rusqlite::Error| AppError::Filter { message: e.to_string(), position: 0 };
    match e.sqlite_error_code() {
        Some(ErrorCode::TypeMismatch | ErrorCode::TooBig) => filter_error(e),
        Some(ErrorCode::Unknown) => {
            let message = e.to_string();
            if FILTER_MESSAGES.iter().any(|m| message.contains(m)) || unfiltered_ok() {
                filter_error(e)
            } else {
                query_error(e)
            }
        }
        _ => query_error(e),
    }
}

/// A failure of the filtered count. It runs after the page with the same filter loaded, so a
/// generic failure is the rows' (the count reads past the page), not the filter's.
fn filtered_count_error(e: rusqlite::Error) -> AppError {
    filter_failure(e, || false)
}

/// Whether the page loads without the filter: the same columns, sort, tie-break, LIMIT and
/// OFFSET. (LIMIT 0 would not do: SQLite then reads no row, so a view failing on its rows passes.)
fn unfiltered_page_loads(
    c: &Connection,
    columns: &[ColumnInfo],
    q: &TableQuery,
    tiebreak: &Tiebreak,
    limit: i64,
    offset: i64,
) -> bool {
    let unfiltered = TableQuery { filter: None, ..q.clone() };
    build_page_select(columns, &unfiltered, tiebreak).is_ok_and(|built| {
        let bind = vec![Value::Integer(limit), Value::Integer(offset)];
        read_rows(c, &page_sql(&built.sql, 0), bind, columns.len()).is_ok()
    })
}

/// Table alias the ORDER BY terms are qualified with, as on Postgres.
const ORDER_ALIAS: &str = "ts_t";

struct BuiltSelect {
    /// SELECT list, FROM, WHERE and ORDER BY; no LIMIT or OFFSET.
    sql: String,
    /// `{schema}.{table}`, quoted.
    from: String,
    where_sql: Option<String>,
    params: Vec<String>,
}

/// What breaks ties after the user's sort, so that paging sees every row once.
#[derive(Debug, Clone, PartialEq)]
enum Tiebreak {
    /// Columns that are unique together, ascending.
    Columns(Vec<String>),
    /// A rowid alias (`rowid`, `_rowid_` or `oid`), unquoted.
    System(&'static str),
    /// Nothing: rows equal on the sort have no fixed order.
    None,
}

/// The grid's SELECT, in the shape of the Postgres one: every column in table order, as stored
/// (no casts: `values::cell` reads each value's storage type), the compiled filter, the sort with
/// `NULLS LAST`, then the primary key as tiebreak (or alone without a sort).
fn build_select(columns: &[ColumnInfo], q: &TableQuery) -> AppResult<BuiltSelect> {
    build_page_select(columns, q, &primary_key_tiebreak(columns))
}

/// The primary key columns, in column order, as the tie-break.
fn primary_key_tiebreak(columns: &[ColumnInfo]) -> Tiebreak {
    Tiebreak::Columns(columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect())
}

/// `build_select` with `tiebreak` after the sort in place of the primary key.
fn build_page_select(columns: &[ColumnInfo], q: &TableQuery, tiebreak: &Tiebreak) -> AppResult<BuiltSelect> {
    let d = &SqliteDialect;
    let from = format!("{}.{}", d.quote_ident(&q.table.schema), d.quote_ident(&q.table.name));
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

    let select_list = columns.iter().map(|c| d.quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
    let mut sql = format!("SELECT {select_list} FROM {from} AS {ORDER_ALIAS}");
    if let Some(w) = &where_sql {
        sql.push_str(&format!(" WHERE {w}"));
    }
    if let Some(o) = &order_by {
        sql.push_str(&format!(" ORDER BY {o}"));
    }
    Ok(BuiltSelect { sql, from, where_sql, params })
}

/// `select` plus the LIMIT and OFFSET placeholders that follow `n_params` filter parameters.
fn page_sql(select: &str, n_params: usize) -> String {
    format!("{select} LIMIT ?{} OFFSET ?{}", n_params + 1, n_params + 2)
}

/// The tie-break of a relation without a primary key. A table without one is a rowid table
/// (WITHOUT ROWID needs a primary key), so the first of its rowid's names that no column takes
/// (SQLite matches them case-insensitively); none when all three are taken, or for a view.
fn keyless_tiebreak(relation: RelationKind, columns: &[ColumnInfo]) -> Tiebreak {
    if relation == RelationKind::View {
        return Tiebreak::None;
    }
    ["rowid", "_rowid_", "oid"]
        .into_iter()
        .find(|name| !columns.iter().any(|c| c.name.eq_ignore_ascii_case(name)))
        .map_or(Tiebreak::None, Tiebreak::System)
}

/// The SELECT `fetch_page` pages through, without LIMIT or OFFSET, for a table export of the
/// grid's rows in the grid's order; plus the filter's parameters, bound as text.
pub(super) fn table_select(structure: &TableStructure, q: &TableQuery) -> AppResult<(String, Vec<String>)> {
    let built = build_select(&structure.columns, q)?;
    Ok((built.sql, built.params))
}

/// Rows as the grid's text; `None` is NULL.
type GridRows = Vec<Vec<Option<String>>>;

/// The rows of `sql` as the grid's text, and each row's storage types (`row_types`).
fn read_rows(c: &Connection, sql: &str, bind: Vec<Value>, n_columns: usize) -> rusqlite::Result<(GridRows, Vec<String>)> {
    let mut stmt = c.prepare(sql)?;
    let mut rows = stmt.query(params_from_iter(bind))?;
    let (mut texts, mut types) = (Vec::new(), Vec::new());
    while let Some(r) = rows.next()? {
        let (row, row_types) = read_row(r, n_columns)?;
        texts.push(row);
        types.push(row_types);
    }
    Ok((texts, types))
}

/// The first `n_columns` values of a row as the grid's text, and their storage types
/// (`row_types`).
pub(super) fn read_row(r: &rusqlite::Row, n_columns: usize) -> rusqlite::Result<(Vec<Option<String>>, String)> {
    let mut row = Vec::with_capacity(n_columns);
    let mut cell_types = Vec::with_capacity(n_columns);
    for i in 0..n_columns {
        let (text, ty) = values::cell(r.get_ref(i)?);
        row.push(text);
        cell_types.push(ty);
    }
    Ok((row, row_types(&cell_types)))
}

/// What the page job reads on the worker.
struct PageRows {
    structure: TableStructure,
    built: BuiltSelect,
    rows: GridRows,
    types: Vec<String>,
    elapsed_ms: u64,
    unstable_order: bool,
}

#[async_trait]
impl Driver for SqliteDriver {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    fn dialect(&self) -> &dyn Dialect {
        &SqliteDialect
    }

    fn query_language(&self) -> QueryLanguage {
        QueryLanguage::Sql(SqlDialect::Sqlite)
    }

    async fn server_version(&self) -> AppResult<String> {
        let version = self
            .worker()
            .await?
            .call(|c| c.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0)))
            .await?
            .map_err(|e| AppError::Query(e.to_string()))?;
        Ok(format!("SQLite {version}"))
    }

    /// A file is one database.
    async fn list_databases(&self) -> AppResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// `main` and the databases attached by the startup SQL. A query tab's `ATTACH` is on the
    /// tab's own connection, so the browse workers don't see it.
    async fn list_schemas(&self, _database: Option<&str>) -> AppResult<Vec<String>> {
        self.on_worker(|c| catalog::list_schemas(c)).await
    }

    async fn schema_tree(&self, schema: &str) -> AppResult<SchemaTree> {
        let schema = schema.to_string();
        self.on_worker(move |c| catalog::schema_tree(c, &schema)).await
    }

    async fn table_structure(&self, table: &TableRef) -> AppResult<TableStructure> {
        let table = table.clone();
        self.on_worker(move |c| catalog::table_structure(c, &table)).await
    }

    async fn fetch_page(&self, req: &PageRequest) -> AppResult<TablePage> {
        let worker = self.worker().await?;
        let tq = TableQuery { table: req.table.clone(), filter: req.filter.clone(), sort: req.sort.clone() };
        let page_size = i64::from(req.page_size.clamp(1, 1000));
        let offset = i64::from(req.page) * page_size;
        let page = worker
            .call(move |c| -> AppResult<PageRows> {
                let structure = catalog::table_structure(c, &tq.table)?;
                // The page and the filter-failure probe below sort the same way.
                let tiebreak = if structure.primary_key.is_empty() {
                    keyless_tiebreak(structure.relation, &structure.columns)
                } else {
                    primary_key_tiebreak(&structure.columns)
                };
                let built = build_page_select(&structure.columns, &tq, &tiebreak)?;
                let unstable_order = tiebreak == Tiebreak::None;
                let sql = page_sql(&built.sql, built.params.len());
                let bind: Vec<Value> = built
                    .params
                    .iter()
                    .map(|p| Value::Text(p.clone()))
                    .chain([Value::Integer(page_size), Value::Integer(offset)])
                    .collect();
                let started = Instant::now();
                let (rows, types) = read_rows(c, &sql, bind, structure.columns.len()).map_err(|e| {
                    if built.where_sql.is_none() {
                        return query_error(e);
                    }
                    filter_failure(e, || unfiltered_page_loads(c, &structure.columns, &tq, &tiebreak, page_size, offset))
                })?;
                let elapsed_ms = started.elapsed().as_millis() as u64;
                Ok(PageRows { structure, built, rows, types, elapsed_ms, unstable_order })
            })
            .await??;
        let PageRows { structure, built, rows, types, elapsed_ms, unstable_order } = page;

        let total =
            self.bounded_count(&worker, format!("SELECT count(*) FROM {}", built.from), Vec::new(), query_error).await?;
        let matches = match &built.where_sql {
            Some(w) => {
                let sql = format!("SELECT count(*) FROM {} WHERE {w}", built.from);
                Some(self.bounded_count(&worker, sql, built.params, filtered_count_error).await?)
            }
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

    /// On a connection of the export's own (`export.rs`).
    async fn export_table(&self, q: &TableQuery, sink: &mut dyn RowSink) -> AppResult<()> {
        export::export_table(&self.config, q, sink).await
    }

    /// One transaction in one job on a browse worker (`edit.rs`). A save that couldn't roll back
    /// ends its worker, which the next request replaces.
    async fn apply_changes(&self, set: &ChangeSet) -> AppResult<ApplyResult> {
        let set = set.clone();
        self.worker().await?.call_then(move |c| edit::apply(c, &set)).await?
    }

    async fn schema_columns(&self, _database: Option<&str>, schema: &str)
        -> AppResult<BTreeMap<String, Vec<String>>> {
        let schema = schema.to_string();
        self.on_worker(move |c| catalog::schema_columns(c, &schema)).await
    }

    /// A connection of the tab's own. A file is one database, so there is no other to open.
    async fn open_query_session(&self, database: Option<&str>) -> AppResult<Arc<dyn QuerySession>> {
        if database.is_some() {
            return Err(AppError::Unsupported("SQLite has no other databases on this connection.".into()));
        }
        Ok(Arc::new(SqliteQuerySession::open(&self.config).await?))
    }

    /// New calls fail from here on; calls already running finish. Each connection closes on its
    /// thread once its last job is done; this waits for that, up to `CLOSE_WAIT`.
    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // A replacement under way finishes first, so its worker is shut down too.
        let _replacing = self.replacing.lock().await;
        let workers = self.workers();
        let ended = futures_util::future::join_all(workers.iter().map(|w| w.shutdown(CLOSE_WAIT))).await;
        if !ended.iter().all(|&e| e) {
            eprintln!(
                "SQLite connections to {} still busy {} s after close; they close when their work ends",
                self.config.path.display(),
                CLOSE_WAIT.as_secs()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, cast_type: &str, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: cast_type.into(),
            full_type: cast_type.into(),
            cast_type: cast_type.into(),
            nullable: true,
            default: None,
            is_primary_key: pk,
            read_only: false,
        }
    }

    fn query(filter: Option<&str>, sort: Option<(&str, bool)>) -> TableQuery {
        TableQuery {
            table: TableRef { schema: "main".into(), name: "Order \"Items\"".into() },
            filter: filter.map(str::to_string),
            sort: sort.map(|(c, descending)| SortSpec { column: c.into(), descending }),
        }
    }

    #[test]
    fn select_shape() {
        let columns = vec![col("id", "integer", true), col("k", "blob", false), col("name", "text", false)];
        let built = build_select(&columns, &query(Some("k = 5 and name like \"%a%\""), Some(("name", true)))).unwrap();
        assert_eq!(
            built.sql,
            "SELECT \"id\", \"k\", \"name\" FROM \"main\".\"Order \"\"Items\"\"\" AS ts_t \
             WHERE (\"k\" = (?1 + 0) AND CASE WHEN typeof(\"name\") = 'blob' THEN '\\x' || lower(hex(\"name\")) \
             ELSE CAST(\"name\" AS TEXT) END LIKE ?2) \
             ORDER BY ts_t.\"name\" DESC NULLS LAST, ts_t.\"id\" ASC"
        );
        assert_eq!(built.from, "\"main\".\"Order \"\"Items\"\"\"");
        assert_eq!(built.params, vec!["5", "%a%"]);
        assert_eq!(page_sql(&built.sql, built.params.len()), format!("{} LIMIT ?3 OFFSET ?4", built.sql));

        // No sort: the primary key alone; no key and no sort: no ORDER BY.
        let plain = build_select(&columns, &query(Some("  "), None)).unwrap();
        assert!(plain.sql.ends_with("AS ts_t ORDER BY ts_t.\"id\" ASC"), "{}", plain.sql);
        assert_eq!((plain.where_sql, plain.params.len()), (None, 0));
        let no_key = vec![col("a", "text", false)];
        assert!(!build_select(&no_key, &query(None, None)).unwrap().sql.contains("ORDER BY"));
        let asc = build_select(&no_key, &query(None, Some(("a", false)))).unwrap();
        assert!(asc.sql.ends_with("ORDER BY ts_t.\"a\" ASC NULLS LAST"), "{}", asc.sql);
        // The key column itself sorted: no second term.
        let by_id = build_select(&columns, &query(None, Some(("id", true)))).unwrap();
        assert!(by_id.sql.ends_with("ORDER BY ts_t.\"id\" DESC NULLS LAST"), "{}", by_id.sql);

        // The export SELECT is the page's, without LIMIT or OFFSET.
        let structure = TableStructure {
            columns: columns.clone(),
            primary_key: vec!["id".into()],
            relation: RelationKind::Table,
            editing: Editing::Allowed,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        };
        let q = query(Some("k = 5"), Some(("name", false)));
        let (sql, params) = table_select(&structure, &q).unwrap();
        assert_eq!(sql, build_select(&columns, &q).unwrap().sql);
        assert!(!sql.contains("LIMIT"));
        assert_eq!(params, vec!["5"]);

        assert_eq!(
            build_select(&columns, &query(None, Some(("nope", false)))).err().unwrap(),
            AppError::Query("Unknown column nope".into())
        );
    }

    fn order_of(columns: &[ColumnInfo], sort: Option<(&str, bool)>, tiebreak: &Tiebreak) -> String {
        let sql = build_page_select(columns, &query(None, sort), tiebreak).unwrap().sql;
        assert!(sql.starts_with("SELECT \"a\", \"b\" FROM \"main\".\"Order \"\"Items\"\"\" AS ts_t"), "{sql}");
        sql.split_once(" ORDER BY ").map_or(String::new(), |(_, o)| o.to_string())
    }

    #[test]
    fn page_order_tiebreaks_in_the_page_select() {
        let columns = vec![col("a", "integer", false), col("b", "text", false)];
        let ab = Tiebreak::Columns(vec!["a".into(), "b".into()]);
        assert_eq!(order_of(&columns, None, &ab), "ts_t.\"a\" ASC, ts_t.\"b\" ASC");
        assert_eq!(order_of(&columns, Some(("a", true)), &ab), "ts_t.\"a\" DESC NULLS LAST, ts_t.\"b\" ASC");
        let rowid = Tiebreak::System("rowid");
        assert_eq!(order_of(&columns, None, &rowid), "ts_t.rowid");
        assert_eq!(order_of(&columns, Some(("b", false)), &rowid), "ts_t.\"b\" ASC NULLS LAST, ts_t.rowid");
        assert_eq!(order_of(&columns, None, &Tiebreak::None), "");
        assert_eq!(order_of(&columns, Some(("b", false)), &Tiebreak::None), "ts_t.\"b\" ASC NULLS LAST");

        // `build_select` is the page select with the primary key (in column order) as tie-break.
        let keyed = vec![col("k2", "integer", true), col("name", "text", false), col("k1", "integer", true)];
        let q = query(None, Some(("name", false)));
        let built = build_select(&keyed, &q).unwrap();
        assert_eq!(
            built.sql,
            "SELECT \"k2\", \"name\", \"k1\" FROM \"main\".\"Order \"\"Items\"\"\" AS ts_t \
             ORDER BY ts_t.\"name\" ASC NULLS LAST, ts_t.\"k2\" ASC, ts_t.\"k1\" ASC"
        );
        let key = Tiebreak::Columns(vec!["k2".into(), "k1".into()]);
        assert_eq!(build_page_select(&keyed, &q, &key).unwrap().sql, built.sql);
    }

    #[test]
    fn page_order_without_a_primary_key_uses_a_free_rowid_name() {
        let cols = |names: &[&str]| names.iter().map(|n| col(n, "text", false)).collect::<Vec<_>>();
        assert_eq!(keyless_tiebreak(RelationKind::Table, &cols(&["a"])), Tiebreak::System("rowid"));
        assert_eq!(keyless_tiebreak(RelationKind::Table, &cols(&["a", "RowID"])), Tiebreak::System("_rowid_"));
        assert_eq!(keyless_tiebreak(RelationKind::Table, &cols(&["rowid", "_ROWID_"])), Tiebreak::System("oid"));
        assert_eq!(keyless_tiebreak(RelationKind::Table, &cols(&["Rowid", "_rowid_", "OID"])), Tiebreak::None);
        assert_eq!(keyless_tiebreak(RelationKind::View, &cols(&["a"])), Tiebreak::None);
    }

    fn failure(code: std::ffi::c_int, message: &str) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some(message.into()))
    }

    #[test]
    fn filter_errors_are_the_filters_doing() {
        use rusqlite::ffi;
        let filter = |m: &str| AppError::Filter { message: m.into(), position: 0 };
        let never = || -> bool { panic!("no probe for this failure") };
        // The filter's, whatever the table: no probe.
        for code in [ffi::SQLITE_MISMATCH, ffi::SQLITE_TOOBIG] {
            assert_eq!(filter_failure(failure(code, "m"), never), filter("m"));
        }
        for m in FILTER_MESSAGES {
            assert_eq!(filter_failure(failure(ffi::SQLITE_ERROR, m), never), filter(m));
        }
        // Any other generic failure: the filter's only when the page loads without it.
        assert_eq!(filter_failure(failure(ffi::SQLITE_ERROR, "malformed JSON"), || true), filter("malformed JSON"));
        assert_eq!(
            filter_failure(failure(ffi::SQLITE_ERROR, "malformed JSON"), || false),
            AppError::Query("malformed JSON".into())
        );
        assert_eq!(filtered_count_error(failure(ffi::SQLITE_ERROR, "malformed JSON")), AppError::Query("malformed JSON".into()));
        assert_eq!(filtered_count_error(failure(ffi::SQLITE_ERROR, FILTER_MESSAGES[0])), filter(FILTER_MESSAGES[0]));
        for code in [ffi::SQLITE_BUSY, ffi::SQLITE_LOCKED, ffi::SQLITE_INTERRUPT, ffi::SQLITE_IOERR, ffi::SQLITE_CORRUPT] {
            assert_eq!(filter_failure(failure(code, "m"), never), AppError::Query("m".into()));
        }
        assert_eq!(query_error(failure(ffi::SQLITE_ERROR, "no such table: x")), AppError::Query("no such table: x".into()));
    }

    fn file_profile(path: &std::path::Path) -> ConnectionProfile {
        ConnectionProfile {
            id: "s".into(),
            name: "s".into(),
            kind: DbKind::Sqlite,
            environment: Environment::Local,
            default_schema: None,
            connect_timeout_secs: 5,
            read_only: false,
            startup_sql: None,
            statement_timeout_secs: None,
            transport: Transport::File { file_path: path.to_string_lossy().into_owned() },
        }
    }

    fn page_of_t() -> PageRequest {
        PageRequest {
            table: TableRef { schema: "main".into(), name: "t".into() },
            filter: None,
            sort: None,
            page: 0,
            page_size: 100,
        }
    }

    /// A save whose rollback fails ends its worker; a panic ends the other. Both are replaced on
    /// their next use, and the save's transaction is gone with its connection.
    #[tokio::test]
    async fn ended_browse_workers_are_replaced_on_next_use() {
        let (_dir, path) = edit::tests::save_fixture();
        let d = connect_sqlite(&file_profile(&path)).await.unwrap();
        let [first, second]: [Arc<SqliteWorker>; 2] = d.workers().try_into().ok().unwrap();

        let set = edit::tests::failing_save();
        let err = first
            .call_then(move |c| edit::apply_with(c, &set, edit::tests::failing_rollback))
            .await
            .unwrap()
            .unwrap_err();
        assert!(err.message().ends_with(" (the connection was reset)"), "{err:?}");
        assert!(first.is_closed());
        assert!(second.call(|_| -> () { panic!("the job fails") }).await.is_err());
        assert!(second.is_closed());

        // One request per slot: each finds its worker closed and opens a new one.
        for _ in 0..BROWSE_WORKERS {
            let page = d.fetch_page(&page_of_t()).await.unwrap();
            assert_eq!(page.rows, vec![vec![Some("1".to_string()), Some("one".to_string())]]);
        }
        let now = d.workers();
        assert!(!Arc::ptr_eq(&now[0], &first) && !Arc::ptr_eq(&now[1], &second));
        assert!(now.iter().all(|w| !w.is_closed()));

        // The reset connection let go of its write lock: a save gets the file.
        let save = ChangeSet {
            table: page_of_t().table,
            changes: vec![RowChange::Update {
                key: vec!["1".into()],
                values: [("v".to_string(), Some("saved".to_string()))].into(),
                original_types: [("id".to_string(), CellType::Integer)].into(),
            }],
        };
        assert_eq!(d.apply_changes(&save).await.unwrap(), ApplyResult { applied: 1 });

        // A closed driver replaces nothing.
        d.close().await;
        assert!(d.workers().iter().all(|w| w.is_closed()));
        assert_eq!(d.fetch_page(&page_of_t()).await.unwrap_err(), worker::closed());
    }
}
