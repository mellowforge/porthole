use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::error::{AppError, AppResult};
use crate::export::RowSink;
use crate::model::*;
use crate::query::{QueryLanguage, QuerySession};

pub mod mongodb;
pub mod mysql;
pub mod pg_session;
pub mod pg_tls;
pub mod postgres;
pub mod sqlite;

/// How long a row count may run before the grid shows the count as unknown, in milliseconds.
pub const DEFAULT_COUNT_TIMEOUT_MS: u64 = 3000;

#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DbKind;
    fn dialect(&self) -> &dyn Dialect;
    /// The language this database's query tabs are written in.
    fn query_language(&self) -> QueryLanguage;
    async fn server_version(&self) -> AppResult<String>;
    /// Databases on the server the user may connect to, by name; templates are left out.
    async fn list_databases(&self) -> AppResult<Vec<String>>;
    /// `database`: `None` (or the profile's own) reads through the pool; another database
    /// is read on a short-lived connection set up like the pool's.
    async fn list_schemas(&self, database: Option<&str>) -> AppResult<Vec<String>>;
    async fn schema_tree(&self, schema: &str) -> AppResult<SchemaTree>;
    async fn table_structure(&self, table: &TableRef) -> AppResult<TableStructure>;
    async fn fetch_page(&self, req: &PageRequest) -> AppResult<TablePage>;
    /// Stops this driver's page loads (`fetch_page`) running now on `table`: none sends another
    /// query, and the ones already sent are cancelled on the server. Returns how many loads it
    /// stopped (0: none was loading). Only Postgres can; the others have none to stop.
    async fn cancel_pages(&self, _table: &TableRef) -> AppResult<u32> {
        Ok(0)
    }
    /// Streams every row of the (filtered, sorted) table to `sink`, without a
    /// statement timeout and without collecting rows. A sink error stops the stream.
    async fn export_table(&self, q: &TableQuery, sink: &mut dyn RowSink) -> AppResult<()>;
    /// Applies the changes in one transaction: all or nothing, and every
    /// UPDATE or DELETE must hit exactly one row.
    async fn apply_changes(&self, set: &ChangeSet) -> AppResult<ApplyResult>;
    /// Tables, partitioned and foreign tables, views and matviews in `schema`,
    /// each mapped to its column names in table order. Partitions are left out,
    /// as in the schema tree. `database` as in `list_schemas`.
    async fn schema_columns(&self, database: Option<&str>, schema: &str)
        -> AppResult<BTreeMap<String, Vec<String>>>;
    /// Opens a dedicated connection for a query tab, set up like the pool's, on `database`
    /// (`None` = the profile's own).
    async fn open_query_session(&self, database: Option<&str>) -> AppResult<Arc<dyn QuerySession>>;
    /// The database the connection was on right after it was set up (MySQL's `DATABASE()`,
    /// after the startup SQL); `None` where the driver doesn't say.
    fn connected_database(&self) -> Option<String> {
        None
    }
    /// The field paths of `collection` in `schema` (MongoDB's database), for completion in query
    /// tabs. Only MongoDB has them; SQL completion uses `schema_columns`.
    async fn field_paths(&self, _schema: &str, _collection: &str) -> AppResult<Vec<String>> {
        Ok(vec![])
    }
    /// True where a save runs one change at a time because the server has no transactions.
    fn one_by_one_saves(&self) -> bool {
        false
    }
    /// The display-only previews of `set`, planned against the table's structure.
    async fn preview_changes(&self, set: &ChangeSet) -> AppResult<Vec<String>> {
        let structure = self.table_structure(&set.table).await?;
        let planned = crate::edit::plan_changes(self.dialect(), &structure, set)?;
        Ok(planned.into_iter().map(|s| s.preview).collect())
    }
    async fn close(&self);
}

fn unsupported(kind: &DbKind) -> AppError {
    let name = match kind {
        DbKind::Postgres => "PostgreSQL",
        DbKind::Mysql => "MySQL",
        DbKind::Sqlite => "SQLite",
        DbKind::Sqlserver => "SQL Server",
        DbKind::Mongodb => "MongoDB",
    };
    AppError::Unsupported(format!("{name} support arrives in a later build"))
}

/// A profile whose database type and address don't go together (a PostgreSQL profile with a
/// file, a SQLite one with a server address). Such a profile still lists, so it can be fixed.
pub const TYPE_ADDRESS_MISMATCH: &str = "This connection's type doesn't match its address. Edit it and save again.";

/// Refuses a profile whose type needs another kind of address: PostgreSQL and MySQL a server,
/// SQLite a file. Types not built yet are refused by `connect` itself.
fn check_transport(profile: &ConnectionProfile) -> AppResult<()> {
    let fits = match profile.kind {
        DbKind::Postgres | DbKind::Mysql | DbKind::Mongodb => matches!(profile.transport, Transport::Network(_)),
        DbKind::Sqlite => matches!(profile.transport, Transport::File { .. }),
        DbKind::Sqlserver => true,
    };
    if fits {
        Ok(())
    } else {
        Err(AppError::Connection(TYPE_ADDRESS_MISMATCH.into()))
    }
}

/// Opens a pooled connection for the profile. Every failure while connecting,
/// running the startup SQL or switching to read-only mode is an
/// `AppError::Connection` carrying the underlying message.
pub async fn connect(
    profile: &ConnectionProfile,
    password: Option<String>,
) -> AppResult<Arc<dyn Driver>> {
    check_transport(profile)?;
    match profile.kind {
        DbKind::Postgres => Ok(Arc::new(postgres::connect_postgres(profile, password).await?)),
        DbKind::Sqlite => Ok(Arc::new(sqlite::connect_sqlite(profile).await?)),
        DbKind::Mysql => Ok(Arc::new(mysql::connect_mysql(profile, password).await?)),
        DbKind::Mongodb => mongodb::connect_mongodb(profile, password).await,
        ref other => Err(unsupported(other)),
    }
}

/// Connects, runs `SELECT version()` and reports the round-trip latency.
pub async fn test_connection(
    profile: &ConnectionProfile,
    password: Option<String>,
) -> AppResult<TestResult> {
    let driver = connect(profile, password).await?;
    let started = Instant::now();
    let version = driver.server_version().await;
    let latency_ms = started.elapsed().as_millis() as u64;
    driver.close().await;
    let server_version = version.map_err(|e| AppError::Connection(e.message().to_string()))?;
    Ok(TestResult { latency_ms, server_version })
}

/// SQL dialect hooks used by the filter compiler and the edit builder.
pub trait Dialect: Send + Sync {
    fn quote_ident(&self, s: &str) -> String;
    /// `n` is 1-based.
    fn placeholder(&self, n: usize) -> String;
    fn cast_param(&self, placeholder: &str, cast_type: &str) -> String;
    fn text_expr(&self, quoted_col: &str) -> String;
    /// A value bound for column `col` in an edit; `original` is the cell's type before the edit.
    fn typed_param(&self, ph: &str, col: &ColumnInfo, _value: Option<&str>, _original: Option<CellType>) -> String {
        self.cast_param(ph, &col.cast_type)
    }
    /// How the SQL preview shows a value bound with `typed_param`: display only, never run.
    fn preview_param(&self, col: &ColumnInfo, value: Option<&str>, _original: Option<CellType>) -> String {
        crate::edit::render_literal(value, &col.data_type)
    }
    /// A filter literal compared with `col`; `numeric` for an unquoted number literal.
    fn literal_param(&self, ph: &str, col: &ColumnInfo, _numeric: bool) -> String {
        self.cast_param(ph, &col.cast_type)
    }
    /// An INSERT into `table` (already quoted) that sets no column.
    fn insert_defaults(&self, table: &str) -> String {
        format!("INSERT INTO {table} DEFAULT VALUES")
    }
}

pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn quote_ident(&self, s: &str) -> String {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }
    fn cast_param(&self, placeholder: &str, cast_type: &str) -> String {
        format!("{placeholder}::{cast_type}")
    }
    fn text_expr(&self, quoted_col: &str) -> String {
        format!("{quoted_col}::text")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_defaults_default_text() {
        assert_eq!(PostgresDialect.insert_defaults("\"fx\".\"t\""), "INSERT INTO \"fx\".\"t\" DEFAULT VALUES");
    }

    #[test]
    fn quote_ident_doubles_quotes() {
        assert_eq!(PostgresDialect.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(PostgresDialect.quote_ident("Order Items"), "\"Order Items\"");
        assert_eq!(PostgresDialect.quote_ident("select"), "\"select\"");
    }

    #[test]
    fn postgres_dialect_hooks_equal_cast_param() {
        let col = ColumnInfo {
            name: "id".into(),
            data_type: "int4".into(),
            full_type: "integer".into(),
            cast_type: "int4".into(),
            nullable: false,
            default: None,
            is_primary_key: true,
            read_only: false,
        };
        let d: &dyn Dialect = &PostgresDialect;
        assert_eq!(d.typed_param("$1", &col, Some("5"), Some(CellType::Integer)), "$1::int4");
        assert_eq!(d.typed_param("$1", &col, None, None), "$1::int4");
        assert_eq!(d.literal_param("$1", &col, true), "$1::int4");
        assert_eq!(d.literal_param("$2", &col, false), "$2::int4");
    }
}
