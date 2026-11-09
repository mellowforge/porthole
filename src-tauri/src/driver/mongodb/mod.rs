//! MongoDB, through the official `mongodb` driver.
//!
//! One `Client` per window (it pools by itself and is a cheap clone). A database is the app's
//! schema, a collection its table; views and time-series collections are listed as views.
//! Columns come from a deterministic sample of documents (`schema.rs`); every cell carries its
//! BSON type code (`values.rs`); the quick filter and the sort compile to MQL (`filter.rs`).
//! Nothing here builds SQL.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use mongodb::bson::{doc, Document};
use mongodb::error::{Error as MongoError, ErrorKind, WriteFailure};
use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
use mongodb::results::CollectionType;
use mongodb::{Client, Collection, IndexModel};

use super::{Dialect, Driver, DEFAULT_COUNT_TIMEOUT_MS};
use crate::error::{AppError, AppResult};
use crate::export::RowSink;
use crate::model::*;
use crate::query::{QueryLanguage, QuerySession};

mod bulk;
pub mod edit;
mod explain;
mod export;
pub mod filter;
pub mod query;
pub mod schema;
pub mod values;

/// The server's own databases, left out of the database list.
const SYSTEM_DATABASES: [&str; 3] = ["admin", "config", "local"];
/// Collections estimated at this many documents or more aren't counted exactly (as on MySQL).
const EXACT_COUNT_LIMIT: u64 = 50_000;
const APP_NAME: &str = "Porthole";
/// The authentication database when the profile names none.
const DEFAULT_AUTH_SOURCE: &str = "admin";
/// Server error codes the read path handles itself.
const UNAUTHORIZED: i32 = 13;
const MAX_TIME_EXPIRED: i32 = 50;
const COMMAND_NOT_SUPPORTED_ON_VIEW: i32 = 166;

/// How a profile's client is set up.
pub struct MongoConfig;

impl MongoConfig {
    /// The client options for the profile, without any I/O:
    /// - the one host and port, connected to directly (a replica set behind a remapped port
    ///   couldn't be discovered otherwise);
    /// - server selection and connecting each limited by the profile's timeout (the driver's
    ///   default is 30 s);
    /// - a credential only for a non-empty user, against the profile's auth database or `admin`;
    /// - TLS by `ssl_mode`: `disable` and `prefer` (MongoDB has no opportunistic TLS) none;
    ///   `require` without certificate checks; `verify-ca` and `verify-full` full verification
    ///   (the driver's rustls TLS can't skip only the host name check); `ssl_root_cert` as the
    ///   CA file.
    pub fn from_profile(p: &ConnectionProfile, password: Option<String>) -> AppResult<ClientOptions> {
        let net = p.network()?;
        let timeout = Duration::from_secs(p.connect_timeout_secs.max(1).into());
        let mut o = ClientOptions::default();
        o.hosts = vec![ServerAddress::Tcp { host: net.host.clone(), port: Some(net.port) }];
        o.direct_connection = Some(true);
        o.app_name = Some(APP_NAME.into());
        o.server_selection_timeout = Some(timeout);
        o.connect_timeout = Some(timeout);
        if !net.user.is_empty() {
            let source = net.auth_source.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_AUTH_SOURCE);
            o.credential = Some(
                Credential::builder().username(net.user.clone()).password(password).source(source.to_string()).build(),
            );
        }
        let ca_file = net.ssl_root_cert.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(std::path::PathBuf::from);
        o.tls = Some(match net.ssl_mode {
            SslMode::Disable | SslMode::Prefer => Tls::Disabled,
            SslMode::Require => {
                Tls::Enabled(TlsOptions::builder().allow_invalid_certificates(Some(true)).ca_file_path(ca_file).build())
            }
            SslMode::VerifyCa | SslMode::VerifyFull => Tls::Enabled(TlsOptions::builder().ca_file_path(ca_file).build()),
        });
        Ok(o)
    }
}

/// Fails, naming the file, when TLS is on and its CA certificate can't be read: the driver would
/// say so only after the server-selection timeout.
fn check_root_cert(o: &ClientOptions) -> AppResult<()> {
    match &o.tls {
        Some(Tls::Enabled(TlsOptions { ca_file_path: Some(path), .. })) => std::fs::File::open(path)
            .map(drop)
            .map_err(|e| AppError::Connection(format!("Couldn't read the CA certificate {}: {e}", path.display()))),
        _ => Ok(()),
    }
}

/// The code and message of a server error (a failed command or write); `None` for anything else
/// (network, server selection, authentication).
pub(crate) fn server_error(e: &MongoError) -> Option<(i32, String)> {
    match e.kind.as_ref() {
        ErrorKind::Command(c) => Some((c.code, c.message.clone())),
        ErrorKind::Write(WriteFailure::WriteError(w)) => Some((w.code, w.message.clone())),
        ErrorKind::Write(WriteFailure::WriteConcernError(w)) => Some((w.code, w.message.clone())),
        _ => None,
    }
}

fn server_code(e: &MongoError) -> Option<i32> {
    server_error(e).map(|(code, _)| code)
}

/// A failure while connecting, or of the connection: the driver's message without the raw
/// server response it appends. A server that couldn't be selected is named with the first cause
/// the driver saw, instead of the driver's whole topology dump.
pub(crate) fn connection_error(e: MongoError) -> AppError {
    if let ErrorKind::ServerSelection { message, .. } = e.kind.as_ref()
        && let Some(short) = unreachable_message(message)
    {
        return AppError::Connection(short);
    }
    AppError::Connection(e.kind.to_string())
}

/// `Couldn't reach the MongoDB server at {host}:{port}.` and the first server's error, from the
/// driver's server-selection message (`… Servers: [ { Address: a, Type: Unknown, Error: Kind:
/// I/O error: Connection refused (os error 61), labels: … } ] }`); `None` when it names no
/// address.
fn unreachable_message(dump: &str) -> Option<String> {
    let between = |text: &'_ str, start: &str, end: &str| -> Option<String> {
        let from = text.find(start)? + start.len();
        let len = text[from..].find(end)?;
        Some(text[from..from + len].to_string())
    };
    let address = between(dump, "Address: ", ", Type: ")?;
    let mut out = format!("Couldn't reach the MongoDB server at {address}.");
    if let Some(cause) = between(dump, "Error: Kind: ", ", labels: ") {
        let cause = cause.strip_prefix("I/O error: ").unwrap_or(&cause);
        let cause = match cause.rfind(" (os error ") {
            Some(at) if cause.ends_with(')') => &cause[..at],
            _ => cause,
        };
        let mut chars = cause.chars();
        if let Some(first) = chars.next() {
            out.push(' ');
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
            if !out.ends_with('.') {
                out.push('.');
            }
        }
    }
    Some(out)
}

/// A failed read of `ns` (`db` or `db.collection`). A server error is the server's message, as on
/// the SQL drivers; code 13 (unauthorized) never echoes the server's text, which repeats the
/// command. Anything else is the connection's.
pub(crate) fn read_error(e: MongoError, ns: &str) -> AppError {
    match server_error(&e) {
        Some((UNAUTHORIZED, _)) => AppError::Query(format!("Not authorized to read {ns}.")),
        Some((_, message)) => AppError::Query(message),
        None => connection_error(e),
    }
}

/// The grid's filter text as a query document; `None` for no filter.
fn parse_filter(text: Option<&str>) -> AppResult<Option<Document>> {
    match text.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => Ok(Some(filter::to_mql(&crate::filter::parse(f)?)?)),
        None => Ok(None),
    }
}

/// Transactions are available on a replica set (`setName`) or through mongos (`isdbgrid`);
/// a standalone server has neither.
fn has_transactions(hello: &Document) -> bool {
    hello.contains_key("setName") || hello.get_str("msg").ok() == Some("isdbgrid")
}

/// The unfiltered count of a collection, by MySQL's rule: an estimate of `EXACT_COUNT_LIMIT` or
/// more stays `Estimated`; otherwise the bounded exact count (`exact`, `None` when it ran out of
/// time), falling back to the estimate if there is one, else `Unknown`.
async fn total_from_estimate<F, Fut>(estimate: u64, exact: F) -> AppResult<RowCount>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = AppResult<Option<u64>>>,
{
    if estimate >= EXACT_COUNT_LIMIT {
        return Ok(RowCount::Estimated(estimate));
    }
    Ok(match exact().await? {
        Some(n) => RowCount::Exact(n),
        None if estimate > 0 => RowCount::Estimated(estimate),
        None => RowCount::Unknown,
    })
}

/// The SQL hooks of `Driver::dialect`. Never called: MongoDB builds no SQL. The filter, the
/// edits and the preview go through `filter.rs` and `values.rs`. Each hook returns its input.
pub struct MongoDialect;

impl Dialect for MongoDialect {
    fn quote_ident(&self, s: &str) -> String {
        s.to_string()
    }
    fn placeholder(&self, n: usize) -> String {
        n.to_string()
    }
    fn cast_param(&self, placeholder: &str, _cast_type: &str) -> String {
        placeholder.to_string()
    }
    fn text_expr(&self, quoted_col: &str) -> String {
        quoted_col.to_string()
    }
}

/// A collection's sampled structure and type, as pages and exports use them.
type Sampled = Arc<(TableStructure, CollectionType)>;

/// `cache[key]`, or else `load`'s result, which is then cached (a failure isn't). The lock is never
/// held across the load.
async fn cached<K, V, F, Fut>(cache: &Mutex<HashMap<K, Arc<V>>>, key: K, load: F) -> AppResult<Arc<V>>
where
    K: Eq + Hash,
    F: FnOnce() -> Fut,
    Fut: Future<Output = AppResult<V>>,
{
    if let Some(v) = lock(cache).get(&key) {
        return Ok(v.clone());
    }
    let v = Arc::new(load().await?);
    lock(cache).insert(key, v.clone());
    Ok(v)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct MongoDriver {
    client: Client,
    dialect: MongoDialect,
    /// The server has transactions (a replica set or mongos).
    transactional: bool,
    /// The profile's database, when it names one.
    database: Option<String>,
    /// How long a count may run before it is shown as unknown, in ms.
    count_timeout_ms: u64,
    /// The client's server-selection timeout (the profile's connect timeout).
    selection_timeout: Duration,
    /// The profile is read-only: MongoDB has no read-only sessions, so saves are refused here.
    read_only: bool,
    /// The profile's statement time limit for query tabs; `None` (also for a stored 0, which the
    /// form refuses) is no limit.
    statement_limit: Option<Duration>,
    /// Each collection's sample, by (database, collection), for this connection: pages and exports
    /// reuse it; the Structure view and every save sample again.
    sampled: Mutex<HashMap<(String, String), Sampled>>,
}

/// Opens the client and runs `hello` and `buildInfo` on it, so a wrong address, TLS setting or
/// password fails here (the client itself connects lazily). Every failure is an
/// `AppError::Connection`.
pub async fn connect_mongodb(profile: &ConnectionProfile, password: Option<String>) -> AppResult<Arc<dyn Driver>> {
    let options = MongoConfig::from_profile(profile, password)?;
    check_root_cert(&options)?;
    let selection_timeout = options.server_selection_timeout.unwrap_or(Duration::from_secs(30));
    let client = Client::with_options(options).map_err(connection_error)?;
    let admin = client.database("admin");
    let probed = async {
        let hello = admin.run_command(doc! { "hello": 1 }).await?;
        admin.run_command(doc! { "buildInfo": 1 }).await?;
        Ok::<_, MongoError>(hello)
    }
    .await;
    let hello = match probed {
        Ok(hello) => hello,
        Err(e) => {
            client.shutdown().immediate(true).await;
            return Err(connection_error(e));
        }
    };
    let database = Some(profile.network()?.database.clone()).filter(|d| !d.is_empty());
    Ok(Arc::new(MongoDriver {
        client,
        dialect: MongoDialect,
        transactional: has_transactions(&hello),
        database,
        count_timeout_ms: DEFAULT_COUNT_TIMEOUT_MS,
        selection_timeout,
        read_only: profile.read_only,
        statement_limit: profile.statement_timeout_secs.filter(|s| *s > 0).map(|s| Duration::from_secs(u64::from(s))),
        sampled: Mutex::new(HashMap::new()),
    }))
}

impl MongoDriver {
    fn collection(&self, table: &TableRef) -> Collection<Document> {
        self.client.database(&table.schema).collection(&table.name)
    }

    /// The collections of `database` matching `filter` that the user may use, each with its type
    /// (`None` for one this app doesn't know): `listCollections` with `nameOnly` (which still
    /// returns the type) and `authorizedCollections`, so a user who may read a collection but not
    /// list the database's collections sees the ones they may read. The driver's typed
    /// `list_collections` can't: it sends `nameOnly: false`, with which the server ignores
    /// `authorizedCollections`. Code 13 is still `Not authorized to read {ns}.`
    async fn list_collections(&self, database: &str, filter: Document, ns: &str) -> AppResult<Vec<(String, Option<CollectionType>)>> {
        let command = doc! { "listCollections": 1, "filter": filter, "nameOnly": true, "authorizedCollections": true };
        let specs: Vec<Document> = self
            .client
            .database(database)
            .run_cursor_command(command)
            .await
            .map_err(|e| read_error(e, ns))?
            .try_collect()
            .await
            .map_err(|e| read_error(e, ns))?;
        Ok(specs.iter().filter_map(|spec| Some((spec.get_str("name").ok()?.to_string(), collection_kind(spec)))).collect())
    }

    /// The collection's type; `NotFound` when there is none of that name. A type this app doesn't
    /// know opens read-only, as a view.
    async fn collection_type(&self, table: &TableRef) -> AppResult<CollectionType> {
        let ns = format!("{}.{}", table.schema, table.name);
        match self.list_collections(&table.schema, doc! { "name": &table.name }, &ns).await?.into_iter().next() {
            Some((_, kind)) => Ok(kind.unwrap_or(CollectionType::View)),
            None => Err(AppError::NotFound(ns)),
        }
    }

    /// The sampled columns, the key `_id`, and the indexes, with the collection's type. A view
    /// or time-series collection can't be edited.
    async fn structure(&self, table: &TableRef) -> AppResult<(TableStructure, CollectionType)> {
        let ns = format!("{}.{}", table.schema, table.name);
        let kind = self.collection_type(table).await?;
        let docs = schema::sample(&self.collection(table)).await.map_err(|e| read_error(e, &ns))?;
        let columns = schema::infer(&docs);
        let (relation, editing) = if is_view_like(&kind) {
            (RelationKind::View, Editing::View)
        } else {
            (RelationKind::Table, schema::editing(&docs))
        };
        let indexes = if kind == CollectionType::View { Vec::new() } else { self.indexes(table, &ns).await? };
        let structure =
            TableStructure { columns, primary_key: vec!["_id".into()], relation, editing, indexes, foreign_keys: Vec::new() };
        Ok((structure, kind))
    }

    fn sample_key(table: &TableRef) -> (String, String) {
        (table.schema.clone(), table.name.clone())
    }

    /// The structure pages and exports use: sampled once per collection on this connection.
    pub(super) async fn sampled_structure(&self, table: &TableRef) -> AppResult<Sampled> {
        cached(&self.sampled, Self::sample_key(table), || self.structure(table)).await
    }

    /// Samples the collection again, and pages and exports use that from now on.
    async fn resample(&self, table: &TableRef) -> AppResult<Sampled> {
        let fresh = Arc::new(self.structure(table).await?);
        lock(&self.sampled).insert(Self::sample_key(table), fresh.clone());
        Ok(fresh)
    }

    /// The collection's indexes, the `_id` index first, then by name. A view has none.
    async fn indexes(&self, table: &TableRef, ns: &str) -> AppResult<Vec<IndexInfo>> {
        let models: Vec<IndexModel> = match self.collection(table).list_indexes().await {
            Ok(cursor) => cursor.try_collect().await.map_err(|e| read_error(e, ns))?,
            Err(e) if server_code(&e) == Some(COMMAND_NOT_SUPPORTED_ON_VIEW) => Vec::new(),
            Err(e) => return Err(read_error(e, ns)),
        };
        let mut indexes: Vec<IndexInfo> = models
            .into_iter()
            .map(|m| {
                let name = m.options.as_ref().and_then(|o| o.name.clone()).unwrap_or_default();
                let primary = name == "_id_";
                let unique = primary || m.options.as_ref().and_then(|o| o.unique).unwrap_or(false);
                IndexInfo { columns: m.keys.keys().cloned().collect(), included: Vec::new(), name, unique, primary }
            })
            .collect();
        indexes.sort_by(|a, b| (!a.primary, &a.name).cmp(&(!b.primary, &b.name)));
        Ok(indexes)
    }

    /// `count_documents(filter)` within `count_timeout_ms`; `None` when it ran out of time.
    async fn bounded_count(&self, coll: &Collection<Document>, filter: Document, ns: &str) -> AppResult<Option<u64>> {
        match coll.count_documents(filter).max_time(Duration::from_millis(self.count_timeout_ms)).await {
            Ok(n) => Ok(Some(n)),
            Err(e) if server_code(&e) == Some(MAX_TIME_EXPIRED) => Ok(None),
            Err(e) => Err(read_error(e, ns)),
        }
    }

    /// A collection's estimate, by `total_from_estimate`. A view or time-series collection has no
    /// cheap estimate (`estimated_document_count` would run a view's pipeline): it is counted
    /// within the limit, or `Unknown`.
    async fn total_count(&self, coll: &Collection<Document>, view_like: bool, ns: &str) -> AppResult<RowCount> {
        if view_like {
            return Ok(self.bounded_count(coll, doc! {}, ns).await?.map_or(RowCount::Unknown, RowCount::Exact));
        }
        let estimate = coll.estimated_document_count().await.map_err(|e| read_error(e, ns))?;
        total_from_estimate(estimate, || self.bounded_count(coll, doc! {}, ns)).await
    }
}

/// A `listCollections` entry's type.
fn collection_kind(spec: &Document) -> Option<CollectionType> {
    match spec.get_str("type").ok()? {
        "collection" => Some(CollectionType::Collection),
        "view" => Some(CollectionType::View),
        "timeseries" => Some(CollectionType::Timeseries),
        _ => None,
    }
}

/// The database list, without the server's own, sorted. A user who may not list databases
/// (code 13) sees the profile's database, when it names one; that is logged.
fn schema_list(listed: Result<Vec<String>, MongoError>, database: Option<&str>) -> AppResult<Vec<String>> {
    match listed {
        Ok(mut names) => {
            names.retain(|n| !SYSTEM_DATABASES.contains(&n.as_str()));
            names.sort();
            Ok(names)
        }
        Err(e) if server_code(&e) == Some(UNAUTHORIZED) && database.is_some() => {
            eprintln!("list_schemas: may not list databases, showing the profile's: {}", e.kind);
            Ok(database.into_iter().map(String::from).collect())
        }
        Err(e) => Err(read_error(e, "the database list")),
    }
}

fn is_view_like(kind: &CollectionType) -> bool {
    !matches!(kind, CollectionType::Collection)
}

#[async_trait]
impl Driver for MongoDriver {
    fn kind(&self) -> DbKind {
        DbKind::Mongodb
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }

    fn query_language(&self) -> QueryLanguage {
        QueryLanguage::Mongo
    }

    /// `MongoDB {buildInfo.version}`, asked again each time (Test Connection times it).
    async fn server_version(&self) -> AppResult<String> {
        let info = self.client.database("admin").run_command(doc! { "buildInfo": 1 }).await.map_err(|e| read_error(e, "admin"))?;
        let version = info.get_str("version").map_err(|_| AppError::Query("The server reported no version.".into()))?;
        Ok(format!("MongoDB {version}"))
    }

    /// The server's databases are the app's schemas, so there is no database list.
    async fn list_databases(&self) -> AppResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// The databases the user may see, without the server's own, sorted. A user who may not list
    /// databases (code 13) sees the profile's database, when it names one.
    async fn list_schemas(&self, _database: Option<&str>) -> AppResult<Vec<String>> {
        schema_list(self.client.list_database_names().await, self.database.as_deref())
    }

    /// Collections as tables; views and time-series collections as views; `system.*` left out.
    async fn schema_tree(&self, schema: &str) -> AppResult<SchemaTree> {
        let (mut tables, mut views) = (Vec::new(), Vec::new());
        for (name, kind) in self.list_collections(schema, doc! {}, schema).await? {
            if name.starts_with("system.") {
                continue;
            }
            match kind {
                Some(CollectionType::Collection) => tables.push(name),
                Some(CollectionType::View | CollectionType::Timeseries) => views.push(name),
                _ => {}
            }
        }
        tables.sort();
        views.sort();
        Ok(SchemaTree { tables, views, functions: Vec::new() })
    }

    /// Samples again: the Structure view shows what is there now, and the pages after it too.
    async fn table_structure(&self, table: &TableRef) -> AppResult<TableStructure> {
        Ok(self.resample(table).await?.0.clone())
    }

    async fn fetch_page(&self, req: &PageRequest) -> AppResult<TablePage> {
        let ns = format!("{}.{}", req.table.schema, req.table.name);
        let sampled = self.sampled_structure(&req.table).await?;
        let (structure, kind) = (&sampled.0, &sampled.1);
        let filter = parse_filter(req.filter.as_deref())?;
        let page_size = i64::from(req.page_size.clamp(1, 1000));
        let skip = u64::from(req.page) * page_size as u64;
        let coll = self.collection(&req.table);

        let started = Instant::now();
        let docs: Vec<Document> = coll
            .find(filter.clone().unwrap_or_default())
            .sort(filter::sort_doc(req.sort.as_ref()))
            .skip(skip)
            .limit(page_size)
            .await
            .map_err(|e| read_error(e, &ns))?
            .try_collect()
            .await
            .map_err(|e| read_error(e, &ns))?;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let columns = schema::with_page_fields(structure.columns.clone(), &docs);
        let (mut rows, mut types) = (Vec::with_capacity(docs.len()), Vec::with_capacity(docs.len()));
        for d in &docs {
            let (texts, codes): (Vec<Option<String>>, Vec<CellType>) =
                columns.iter().map(|c| values::cell(d.get(&c.name))).unzip();
            rows.push(texts);
            types.push(row_types(&codes));
        }

        let total = self.total_count(&coll, is_view_like(kind), &ns).await?;
        let matches = match filter {
            Some(f) => Some(self.bounded_count(&coll, f, &ns).await?.map_or(RowCount::Unknown, RowCount::Exact)),
            None => None,
        };
        Ok(TablePage {
            columns,
            primary_key: structure.primary_key.clone(),
            editing: structure.editing,
            rows,
            total,
            matches,
            elapsed_ms,
            cell_types: Some(types),
            unstable_order: false,
        })
    }

    async fn export_table(&self, q: &TableQuery, sink: &mut dyn RowSink) -> AppResult<()> {
        export::export_table(self, q, sink).await
    }

    /// A read-only profile is refused before anything is sent (MongoDB has no read-only
    /// sessions). Otherwise the save is planned against a fresh sample and run in one
    /// transaction, or one change at a time on a server without transactions (`edit.rs`). The
    /// documents changed, so the page after it samples again.
    async fn apply_changes(&self, set: &ChangeSet) -> AppResult<ApplyResult> {
        if self.read_only {
            return Err(AppError::Query(edit::READ_ONLY_REFUSAL.into()));
        }
        let sampled = self.resample(&set.table).await?;
        let ops = edit::plan_indexed(&sampled.0, set)?;
        let db = self.client.database(&set.table.schema);
        let applied = edit::apply(&db, &set.table.name, &ops, set.changes.len(), self.transactional, self.selection_timeout).await;
        lock(&self.sampled).remove(&Self::sample_key(&set.table));
        Ok(ApplyResult { applied: applied? })
    }

    /// One mongosh line per write, planned against a fresh sample.
    async fn preview_changes(&self, set: &ChangeSet) -> AppResult<Vec<String>> {
        let sampled = self.resample(&set.table).await?;
        Ok(edit::preview(&set.table.name, &edit::plan(&sampled.0, set)?))
    }

    /// No SQL completion for MongoDB.
    async fn schema_columns(&self, _database: Option<&str>, _schema: &str) -> AppResult<BTreeMap<String, Vec<String>>> {
        Ok(BTreeMap::new())
    }

    /// The paths in the collection's sample (`schema::sample`), sampled again on every call: the
    /// UI caches them per connection. A collection that isn't there has none.
    async fn field_paths(&self, schema: &str, collection: &str) -> AppResult<Vec<String>> {
        let ns = format!("{schema}.{collection}");
        let coll = self.client.database(schema).collection::<Document>(collection);
        let docs = schema::sample(&coll).await.map_err(|e| read_error(e, &ns))?;
        Ok(schema::field_paths(&docs))
    }

    /// On `database`, else the profile's, else `admin`. It shares the driver's client.
    async fn open_query_session(&self, database: Option<&str>) -> AppResult<Arc<dyn QuerySession>> {
        let database = database.map(String::from).or_else(|| self.database.clone()).unwrap_or_else(|| "admin".into());
        Ok(Arc::new(query::MongoQuerySession::new(
            self.client.clone(),
            database,
            self.selection_timeout,
            self.read_only,
            self.statement_limit,
        )))
    }

    fn one_by_one_saves(&self) -> bool {
        !self.transactional
    }

    /// Immediately: a plain `shutdown` waits for every live cursor and session.
    async fn close(&self) {
        self.client.clone().shutdown().immediate(true).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(ssl_mode: SslMode, user: &str, auth_source: Option<&str>) -> ConnectionProfile {
        ConnectionProfile {
            id: "g".into(),
            name: "g".into(),
            kind: DbKind::Mongodb,
            environment: Environment::Local,
            default_schema: None,
            connect_timeout_secs: 7,
            read_only: false,
            startup_sql: None,
            statement_timeout_secs: None,
            transport: Transport::Network(NetworkTarget {
                host: "db.example".into(),
                port: 27018,
                user: user.into(),
                database: "fx".into(),
                ssl_mode,
                ssl_root_cert: None,
                auth_source: auth_source.map(String::from),
            }),
        }
    }

    fn options(p: &ConnectionProfile) -> ClientOptions {
        MongoConfig::from_profile(p, Some("pw".into())).unwrap()
    }

    fn tls_options(mode: SslMode, root: Option<&str>) -> Option<Tls> {
        let mut p = profile(mode, "u", None);
        if let Transport::Network(n) = &mut p.transport {
            n.ssl_root_cert = root.map(String::from);
        }
        options(&p).tls
    }

    #[test]
    fn mongo_config_host_timeouts_direct_connection() {
        let o = options(&profile(SslMode::Disable, "u", None));
        assert_eq!(o.hosts, [ServerAddress::Tcp { host: "db.example".into(), port: Some(27018) }]);
        assert_eq!(o.direct_connection, Some(true));
        assert_eq!(o.app_name.as_deref(), Some("Porthole"));
        assert_eq!(o.server_selection_timeout, Some(Duration::from_secs(7)));
        assert_eq!(o.connect_timeout, Some(Duration::from_secs(7)));
        let mut p = profile(SslMode::Disable, "u", None);
        p.connect_timeout_secs = 0;
        assert_eq!(options(&p).server_selection_timeout, Some(Duration::from_secs(1)));
    }

    #[test]
    fn mongo_config_credential_and_auth_source() {
        let c = options(&profile(SslMode::Disable, "u", None)).credential.unwrap();
        assert_eq!(c.username.as_deref(), Some("u"));
        assert_eq!(c.password.as_deref(), Some("pw"));
        assert_eq!(c.source.as_deref(), Some("admin"));
        let c = options(&profile(SslMode::Disable, "u", Some("fx"))).credential.unwrap();
        assert_eq!(c.source.as_deref(), Some("fx"));
        let c = options(&profile(SslMode::Disable, "u", Some("  "))).credential.unwrap();
        assert_eq!(c.source.as_deref(), Some("admin"));
        // No user, no credential (a server without auth), whatever the password.
        assert!(options(&profile(SslMode::Disable, "", Some("fx"))).credential.is_none());
    }

    #[test]
    fn mongo_config_tls_modes() {
        for mode in [SslMode::Disable, SslMode::Prefer] {
            assert_eq!(tls_options(mode.clone(), Some("/tmp/ca.pem")), Some(Tls::Disabled), "{mode:?}");
        }
        let Some(Tls::Enabled(require)) = tls_options(SslMode::Require, None) else { panic!("require has TLS") };
        assert_eq!(require.allow_invalid_certificates, Some(true));
        assert_eq!(require.ca_file_path, None);
        // verify-ca is verified in full: rustls can't skip only the host name check.
        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let Some(Tls::Enabled(t)) = tls_options(mode.clone(), None) else { panic!("{mode:?} has TLS") };
            assert_eq!(t.allow_invalid_certificates, None, "{mode:?}");
            assert_eq!(t.ca_file_path, None);
            let Some(Tls::Enabled(t)) = tls_options(mode.clone(), Some("/tmp/ca.pem")) else { panic!() };
            assert_eq!(t.ca_file_path, Some("/tmp/ca.pem".into()), "{mode:?}");
        }
        let Some(Tls::Enabled(t)) = tls_options(SslMode::VerifyFull, Some("  ")) else { panic!() };
        assert_eq!(t.ca_file_path, None);
    }

    #[test]
    fn mongo_config_needs_a_server_address() {
        let mut p = profile(SslMode::Disable, "u", None);
        p.transport = Transport::File { file_path: "/x".into() };
        assert!(matches!(MongoConfig::from_profile(&p, None), Err(AppError::Connection(_))));
    }

    /// A CA certificate file that isn't there fails the connect with an error naming it, before
    /// any connection is tried.
    #[tokio::test]
    async fn mongo_a_missing_root_cert_is_named() {
        let mut p = profile(SslMode::VerifyCa, "u", None);
        if let Transport::Network(n) = &mut p.transport {
            n.host = "127.0.0.1".into();
            n.port = 1;
            n.ssl_root_cert = Some("/nonexistent/ts-ca.pem".into());
        }
        let started = Instant::now();
        match connect_mongodb(&p, None).await {
            Err(AppError::Connection(m)) => assert!(m.contains("/nonexistent/ts-ca.pem"), "{m}"),
            Err(e) => panic!("expected a connection error, got {e:?}"),
            Ok(_) => panic!("expected a connection error"),
        }
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn mongo_unreachable_servers_get_a_short_message() {
        let dump = |address: &str, error: &str| {
            format!("Server selection timeout: No available servers. Topology: {{ Type: Single, Servers: [ {{ Address: {address}, Type: Unknown, Error: Kind: {error}, labels: {{\"RetryableError\"}}, source: None, server response: None }} ] }}")
        };
        assert_eq!(
            unreachable_message(&dump("127.0.0.1:27999", "I/O error: Connection refused (os error 61)")).as_deref(),
            Some("Couldn't reach the MongoDB server at 127.0.0.1:27999. Connection refused.")
        );
        assert_eq!(
            unreachable_message(&dump("10.255.255.1:27017", "I/O error: timed out")).as_deref(),
            Some("Couldn't reach the MongoDB server at 10.255.255.1:27017. Timed out.")
        );
        assert_eq!(
            unreachable_message(&dump("db.invalid:27017", "I/O error: failed to lookup address information: nodename nor servname provided, or not known")).as_deref(),
            Some("Couldn't reach the MongoDB server at db.invalid:27017. Failed to lookup address information: nodename nor servname provided, or not known.")
        );
        // A server that answered but doesn't suit: no cause to add.
        let unsuitable = "Server selection timeout: None of the available servers suitable for criteria Predicate. Topology: { Type: Single, Servers: [ { Address: h:1, Type: RsSecondary, Average RTT: 1ms } ] }";
        assert_eq!(unreachable_message(unsuitable).as_deref(), Some("Couldn't reach the MongoDB server at h:1."));
        assert_eq!(unreachable_message("Server selection timeout: No available servers. Topology: { Type: Unknown }"), None);
    }

    /// The driver's real server-selection failure, from a closed port.
    #[tokio::test]
    async fn mongo_a_closed_port_names_the_server() {
        let mut p = profile(SslMode::Disable, "", None);
        p.connect_timeout_secs = 1;
        if let Transport::Network(n) = &mut p.transport {
            n.host = "127.0.0.1".into();
            n.port = 1;
        }
        match connect_mongodb(&p, None).await {
            Err(e) => assert_eq!(e, AppError::Connection("Couldn't reach the MongoDB server at 127.0.0.1:1. Connection refused.".into())),
            Ok(_) => panic!("connected to a closed port"),
        }
    }

    #[test]
    fn mongo_transactions_on_a_replica_set_or_mongos() {
        assert!(!has_transactions(&doc! { "isWritablePrimary": true, "ok": 1 }));
        assert!(has_transactions(&doc! { "isWritablePrimary": true, "setName": "rs0" }));
        assert!(has_transactions(&doc! { "msg": "isdbgrid" }));
        assert!(!has_transactions(&doc! { "msg": "other" }));
    }

    #[tokio::test]
    async fn mongo_total_counts_follow_the_mysql_rule() {
        let exact = |n: Option<u64>| move || async move { Ok(n) };
        // At the limit or above: the estimate, without counting.
        let never = || async { panic!("counted a large collection") };
        assert_eq!(total_from_estimate(50_000, never).await, Ok(RowCount::Estimated(50_000)));
        assert_eq!(total_from_estimate(1_000_000, never).await, Ok(RowCount::Estimated(1_000_000)));
        // Below it: the exact count; when that runs out of time, the estimate, or Unknown.
        assert_eq!(total_from_estimate(20_000, exact(Some(20_001))).await, Ok(RowCount::Exact(20_001)));
        assert_eq!(total_from_estimate(20_000, exact(None)).await, Ok(RowCount::Estimated(20_000)));
        assert_eq!(total_from_estimate(0, exact(None)).await, Ok(RowCount::Unknown));
        assert_eq!(total_from_estimate(0, exact(Some(0))).await, Ok(RowCount::Exact(0)));
        let failing = || async { Err(AppError::Query("x".into())) };
        assert_eq!(total_from_estimate(10, failing).await, Err(AppError::Query("x".into())));
    }

    /// Two lookups of one key load it once; a failed load isn't cached; keys are apart.
    #[tokio::test]
    async fn mongo_samples_are_cached_per_collection() {
        let cache: Mutex<HashMap<(String, String), Arc<u32>>> = Mutex::new(HashMap::new());
        let loads = std::sync::atomic::AtomicU32::new(0);
        let load = || async {
            Ok(loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
        };
        let key = |n: &str| ("fx".to_string(), n.to_string());
        assert_eq!(*cached(&cache, key("users"), load).await.unwrap(), 1);
        assert_eq!(*cached(&cache, key("users"), load).await.unwrap(), 1);
        assert_eq!(*cached(&cache, key("typed"), load).await.unwrap(), 2);
        let failing = || async { Err::<u32, _>(AppError::Query("x".into())) };
        assert!(cached(&cache, key("big"), failing).await.is_err());
        assert_eq!(*cached(&cache, key("big"), load).await.unwrap(), 3);
        assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    fn command_error(code: i32, message: &str) -> MongoError {
        let e: mongodb::error::CommandError =
            mongodb::bson::deserialize_from_document(doc! { "code": code, "codeName": "X", "errmsg": message }).unwrap();
        ErrorKind::Command(e).into()
    }

    #[test]
    fn mongo_database_list_falls_back_to_the_profiles_on_code_13() {
        let names = |n: &[&str]| Ok(n.iter().map(|s| s.to_string()).collect());
        assert_eq!(schema_list(names(&["zz", "admin", "fx", "local", "config"]), Some("fx")), Ok(vec!["fx".into(), "zz".into()]));
        let denied = || Err(command_error(13, "not authorized on admin to execute command { listDatabases: 1 }"));
        assert_eq!(schema_list(denied(), Some("fx")), Ok(vec!["fx".into()]));
        // Without a database in the profile there is nothing to show instead.
        assert_eq!(schema_list(denied(), None), Err(AppError::Query("Not authorized to read the database list.".into())));
        assert_eq!(schema_list(Err(command_error(2, "bad")), Some("fx")), Err(AppError::Query("bad".into())));
    }

    #[test]
    fn mongo_collection_kinds() {
        assert_eq!(collection_kind(&doc! { "name": "a", "type": "collection" }), Some(CollectionType::Collection));
        assert_eq!(collection_kind(&doc! { "name": "a", "type": "view" }), Some(CollectionType::View));
        assert_eq!(collection_kind(&doc! { "name": "a", "type": "timeseries" }), Some(CollectionType::Timeseries));
        assert_eq!(collection_kind(&doc! { "name": "a", "type": "other" }), None);
        assert_eq!(collection_kind(&doc! { "name": "a" }), None);
    }

    #[test]
    fn mongo_read_errors_show_the_server_message() {
        assert_eq!(server_code(&command_error(50, "operation exceeded time limit")), Some(50));
        assert_eq!(
            read_error(command_error(2, "bad $regex"), "fx.users"),
            AppError::Query("bad $regex".into())
        );
        // Code 13's text repeats the command, values included: never shown.
        let unauthorized = command_error(13, r#"not authorized on fx to execute command { find: "users", filter: { pw: "s3cret" } }"#);
        assert_eq!(read_error(unauthorized, "fx.users"), AppError::Query("Not authorized to read fx.users.".into()));
        // Anything but a server error is the connection's.
        let io = || std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let refused: MongoError = ErrorKind::Io(Arc::new(io())).into();
        assert_eq!(server_code(&refused), None);
        assert_eq!(read_error(refused, "fx"), AppError::Connection(format!("I/O error: {}", io())));
    }

    #[test]
    fn mongo_dialect_returns_plain_values() {
        let d = MongoDialect;
        assert_eq!(d.quote_ident("a.b"), "a.b");
        assert_eq!(d.placeholder(3), "3");
        assert_eq!(d.cast_param("p", "int32"), "p");
        assert_eq!(d.text_expr("c"), "c");
    }
}
