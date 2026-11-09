use std::collections::BTreeMap;
use std::sync::Arc;

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::appdb::AppDb;
use crate::driver::{self, Driver};
use crate::error::{AppError, AppResult};
use crate::export::run::{run_mongo_export, run_query_export, run_rows_export, run_table_export, ExportOutcome, ExportProgress};
use crate::export::{ExportFormat, ExportJobs};
use crate::model::*;
use crate::query::sessions::{QuerySessions, QUERY_RUNNING};
use crate::query::{QueryLanguage, ResultColumn};
use crate::sqltext::SqlDialect;
use crate::query::{runner, RunEvent, RunOutcome, RunRequest, StatementResult, TxStatus};
use crate::secrets::{password_key, SecretStore};
use crate::session::Sessions;

#[tauri::command]
pub async fn list_connections(db: State<'_, AppDb>) -> AppResult<Vec<ConnectionProfile>> {
    db.list_connections().await
}

/// Applies a password change from the form: `None` keeps the stored password,
/// `Some("")` deletes it and `Some(p)` stores `p`.
pub fn apply_password(secrets: &dyn SecretStore, id: &str, password: Option<String>) -> AppResult<()> {
    let key = password_key(id);
    match password.as_deref() {
        None => Ok(()),
        Some("") => secrets.delete(&key),
        Some(p) => secrets.set(&key, p),
    }
}

/// Saves the profile and applies the password change. If a new profile's
/// password cannot be stored, the just-inserted row is removed again.
pub async fn save_profile(
    db: &AppDb,
    secrets: &dyn SecretStore,
    profile: ConnectionProfile,
    password: Option<String>,
) -> AppResult<ConnectionProfile> {
    let is_new = profile.id.is_empty();
    let saved = db.upsert_connection(profile).await?;
    if let Err(e) = apply_password(secrets, &saved.id, password) {
        if is_new
            && let Err(cleanup) = db.delete_connection(&saved.id).await
        {
            eprintln!("could not remove connection {} after a password error: {cleanup}", saved.id);
        }
        return Err(e);
    }
    Ok(saved)
}

/// Deletes the stored password first (a missing one is fine), then the profile,
/// so a failure never leaves a password without its profile.
pub async fn delete_profile(db: &AppDb, secrets: &dyn SecretStore, id: &str) -> AppResult<()> {
    secrets.delete(&password_key(id))?;
    db.delete_connection(id).await
}

/// Every window refreshes its connection list on this (global) event.
pub const CONNECTIONS_CHANGED: &str = "connections-changed";
/// Windows on `connection_id` refresh their saved queries on this (global) event.
pub const SAVED_QUERIES_CHANGED: &str = "saved-queries-changed";

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQueriesChanged {
    pub connection_id: String,
}

/// Tells every window about a change. The change itself has been made, so a failure
/// to tell is logged rather than returned.
fn notify<S: serde::Serialize + Clone>(app: &AppHandle, event: &str, payload: S) {
    if let Err(e) = app.emit(event, payload) {
        eprintln!("could not send {event}: {e}");
    }
}

/// `password`: `None` keeps the stored one, `Some("")` deletes it, `Some(p)` stores it.
#[tauri::command]
pub async fn save_connection(
    app: AppHandle,
    db: State<'_, AppDb>,
    secrets: State<'_, Arc<dyn SecretStore>>,
    profile: ConnectionProfile,
    password: Option<String>,
) -> AppResult<ConnectionProfile> {
    let saved = save_profile(&db, secrets.inner().as_ref(), profile, password).await?;
    notify(&app, CONNECTIONS_CHANGED, ());
    Ok(saved)
}

#[tauri::command]
pub async fn delete_connection(
    app: AppHandle,
    db: State<'_, AppDb>,
    secrets: State<'_, Arc<dyn SecretStore>>,
    id: String,
) -> AppResult<()> {
    delete_profile(&db, secrets.inner().as_ref(), &id).await?;
    notify(&app, CONNECTIONS_CHANGED, ());
    Ok(())
}

/// A `None` password falls back to the stored one.
#[tauri::command]
pub async fn test_connection(
    secrets: State<'_, Arc<dyn SecretStore>>,
    profile: ConnectionProfile,
    password: Option<String>,
) -> AppResult<TestResult> {
    let password = match password {
        Some(p) => Some(p),
        None => secrets.get(&password_key(&profile.id))?,
    };
    driver::test_connection(&profile, password).await
}

#[tauri::command]
pub async fn connect(
    window: tauri::Window,
    db: State<'_, AppDb>,
    secrets: State<'_, Arc<dyn SecretStore>>,
    sessions: State<'_, Sessions>,
    query_sessions: State<'_, QuerySessions>,
    jobs: State<'_, ExportJobs>,
    id: String,
) -> AppResult<ConnectedInfo> {
    // The window's session is about to be replaced: its exports stop, so the old driver's
    // close (in the background) isn't kept waiting by them.
    jobs.cancel_window(window.label());
    query_sessions.close_window(window.label()).await;
    let profile = db.get_connection(&id).await?;
    let password = secrets.get(&password_key(&profile.id))?;
    let driver = driver::connect(&profile, password).await?;
    let info = match describe(&profile, driver.as_ref()).await {
        Ok(info) => info,
        Err(e) => {
            driver.close().await;
            return Err(e);
        }
    };
    // The window may have been closed while we were connecting. Its `Destroyed` cleanup
    // found nothing to remove then, so don't leave a session (and tab-state ownership)
    // behind under a dead label.
    let app = window.app_handle();
    let label = window.label();
    if !sessions.set_if_alive(label, profile, driver, || app.get_webview_window(label).is_some()).await {
        return Err(AppError::NotFound("The window was closed while connecting".into()));
    }
    Ok(info)
}

/// The server version, the schemas, and the schema the window opens on. `pub` for tests.
#[doc(hidden)]
pub async fn describe(profile: &ConnectionProfile, driver: &dyn Driver) -> AppResult<ConnectedInfo> {
    let server_version = driver.server_version().await?;
    let schemas = driver.list_schemas(None).await?;
    let schema = match profile.kind {
        DbKind::Mysql => mysql_initial_schema(profile, driver.connected_database().as_deref(), &schemas),
        DbKind::Mongodb => mongo_initial_schema(profile, &schemas),
        _ => initial_schema(profile, &schemas),
    };
    Ok(ConnectedInfo { profile: profile.clone(), server_version, schemas, schema, one_by_one_saves: driver.one_by_one_saves() })
}

/// The schema a Postgres or SQLite window opens on: the profile's default schema; then
/// `public`; then the first. (A MySQL window goes through `mysql_initial_schema`; the MySQL
/// branch here is older, and is kept because its existing test pins it.)
fn initial_schema(profile: &ConnectionProfile, schemas: &[String]) -> String {
    let listed = |name: &str| schemas.iter().find(|s| s.as_str() == name).cloned();
    let profile_database = match (&profile.kind, &profile.transport) {
        (DbKind::Mysql, Transport::Network(n)) => listed(&n.database),
        _ => None,
    };
    profile
        .default_schema
        .clone()
        .or(profile_database)
        .or_else(|| listed("public"))
        .or_else(|| schemas.first().cloned())
        .unwrap_or_else(|| "public".to_string())
}

/// The database a MySQL window opens on: the profile's default schema; else, when
/// the profile names a database, the one the connection is on (`connected`: `DATABASE()` after
/// the startup SQL, in the server's own spelling); else the first listed. Never `public`, which
/// on MySQL is just a database name; with nothing listed, the connection's database or empty.
fn mysql_initial_schema(profile: &ConnectionProfile, connected: Option<&str>, schemas: &[String]) -> String {
    let names_database = matches!(&profile.transport, Transport::Network(n) if !n.database.is_empty());
    profile
        .default_schema
        .clone()
        .or_else(|| connected.filter(|_| names_database).map(str::to_string))
        .or_else(|| schemas.first().cloned())
        .or_else(|| connected.map(str::to_string))
        .unwrap_or_default()
}

/// The database a MongoDB window opens on: the profile's database when it names one (MongoDB
/// creates a database on its first write, so it needn't be listed yet); else the first listed;
/// else none. Never `public`.
fn mongo_initial_schema(profile: &ConnectionProfile, schemas: &[String]) -> String {
    match &profile.transport {
        Transport::Network(n) if !n.database.trim().is_empty() => n.database.clone(),
        _ => schemas.first().cloned().unwrap_or_default(),
    }
}

#[tauri::command]
pub async fn disconnect(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    query_sessions: State<'_, QuerySessions>,
    jobs: State<'_, ExportJobs>,
) -> AppResult<()> {
    jobs.cancel_window(window.label());
    query_sessions.close_window(window.label()).await;
    sessions.remove(window.label()).await;
    Ok(())
}

#[tauri::command]
pub async fn schema_tree(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    schema: String,
) -> AppResult<SchemaTree> {
    let (_, driver) = sessions.get(window.label()).await?;
    driver.schema_tree(&schema).await
}

#[tauri::command]
pub async fn table_structure(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    table: TableRef,
) -> AppResult<TableStructure> {
    let (_, driver) = sessions.get(window.label()).await?;
    driver.table_structure(&table).await
}

#[tauri::command]
pub async fn fetch_page(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    req: PageRequest,
) -> AppResult<TablePage> {
    let (_, driver) = sessions.get(window.label()).await?;
    driver.fetch_page(&req).await
}

/// Stop for a slow table page: stops the page loads running on `table` and returns how many
/// it stopped (0 when none was loading, or where the driver can't).
#[tauri::command]
pub async fn cancel_page(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    table: TableRef,
) -> AppResult<u32> {
    let (_, driver) = sessions.get(window.label()).await?;
    driver.cancel_pages(&table).await
}

/// Plans the changes against the catalog structure of `set.table` and returns
/// the display-only SQL previews. Nothing runs.
#[tauri::command]
pub async fn preview_changes(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    set: ChangeSet,
) -> AppResult<Vec<String>> {
    let (_, driver) = sessions.get(window.label()).await?;
    driver.preview_changes(&set).await
}

#[tauri::command]
pub async fn apply_changes(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    set: ChangeSet,
) -> AppResult<ApplyResult> {
    let (_, driver) = sessions.get(window.label()).await?;
    driver.apply_changes(&set).await
}

#[tauri::command]
pub async fn analyze_sql(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    sql: String,
) -> AppResult<crate::sqltext::SqlAnalysis> {
    let lang = match sessions.get(window.label()).await {
        Ok((_, driver)) => driver.query_language(),
        Err(_) => QueryLanguage::Sql(SqlDialect::Postgres),
    };
    match lang {
        QueryLanguage::Sql(dialect) => Ok(crate::sqltext::analyze_in(dialect, &sql)),
        QueryLanguage::Mongo => Ok(crate::mongoquery::analyze(&sql)),
    }
}

/// Format for a MongoDB query tab: `text` (the selection or the whole tab) reprinted. Text
/// that doesn't lex is a query error with the lexer's message.
#[tauri::command]
pub async fn format_mongo(text: String) -> AppResult<String> {
    crate::mongoquery::format::format(&text).map_err(|e| AppError::Query(e.message))
}

// Query storage commands. The connection id always comes from the window's session.

async fn session_conn_id(window: &tauri::Window, sessions: &Sessions) -> AppResult<String> {
    Ok(sessions.get(window.label()).await?.0.id)
}

#[tauri::command]
pub async fn list_saved_queries(
    window: tauri::Window,
    db: State<'_, AppDb>,
    sessions: State<'_, Sessions>,
) -> AppResult<Vec<SavedQuery>> {
    db.list_saved_queries(&session_conn_id(&window, &sessions).await?).await
}

#[tauri::command]
pub async fn save_query(
    app: AppHandle,
    window: tauri::Window,
    db: State<'_, AppDb>,
    sessions: State<'_, Sessions>,
    mut query: SavedQuery,
) -> AppResult<SavedQuery> {
    let connection_id = session_conn_id(&window, &sessions).await?;
    query.connection_id = connection_id.clone();
    let saved = db.upsert_saved_query(query).await?;
    notify(&app, SAVED_QUERIES_CHANGED, SavedQueriesChanged { connection_id });
    Ok(saved)
}

#[tauri::command]
pub async fn delete_saved_query(
    app: AppHandle,
    window: tauri::Window,
    db: State<'_, AppDb>,
    sessions: State<'_, Sessions>,
    id: String,
) -> AppResult<()> {
    let connection_id = session_conn_id(&window, &sessions).await?;
    db.delete_saved_query(&id).await?;
    notify(&app, SAVED_QUERIES_CHANGED, SavedQueriesChanged { connection_id });
    Ok(())
}

#[tauri::command]
pub async fn list_history(
    window: tauri::Window,
    db: State<'_, AppDb>,
    sessions: State<'_, Sessions>,
    limit: u32,
) -> AppResult<Vec<HistoryEntry>> {
    db.list_history(&session_conn_id(&window, &sessions).await?, limit).await
}

/// The window's stored tabs, when it owns its connection's tab state; `None` otherwise.
pub async fn load_tab_state_for(db: &AppDb, sessions: &Sessions, label: &str) -> AppResult<Option<String>> {
    match sessions.tab_state_connection(label).await? {
        Some(connection_id) => db.load_tab_state(&connection_id).await,
        None => Ok(None),
    }
}

/// Saves the window's tabs when it owns its connection's tab state; does nothing otherwise.
pub async fn save_tab_state_for(db: &AppDb, sessions: &Sessions, label: &str, json: &str) -> AppResult<()> {
    match sessions.tab_state_connection(label).await? {
        Some(connection_id) => db.save_tab_state(&connection_id, json).await,
        None => Ok(()),
    }
}

/// Only the first window on a connection restores its tabs; any other starts empty.
#[tauri::command]
pub async fn load_tab_state(
    window: tauri::Window,
    db: State<'_, AppDb>,
    sessions: State<'_, Sessions>,
) -> AppResult<Option<String>> {
    load_tab_state_for(&db, &sessions, window.label()).await
}

/// Only the first window on a connection saves its tabs; for any other this does nothing.
/// When that window disconnects, closes or leaves for the Connections screen, the next
/// one's next save writes.
#[tauri::command]
pub async fn save_tab_state(
    window: tauri::Window,
    db: State<'_, AppDb>,
    sessions: State<'_, Sessions>,
    json: String,
) -> AppResult<()> {
    save_tab_state_for(&db, &sessions, window.label(), &json).await
}

/// The window left the workspace (its tabs were just saved): another window on the same
/// connection becomes the one that saves.
#[tauri::command]
pub fn release_tab_state(window: tauri::Window, sessions: State<'_, Sessions>) {
    sessions.release_tab_state(window.label());
}

/// The run error after the tab's connection went away; `reason` is what ended it, when known.
pub fn lost_connection_message(reason: Option<&str>) -> String {
    match reason {
        Some(r) => format!("The connection to the database was lost ({r}). Run again to reconnect."),
        None => "The connection to the database was lost. Run again to reconnect.".to_string(),
    }
}

/// The parameter values a run actually used: the union of every statement's own params.
pub fn used_params(statements: &[StatementResult]) -> ParamValues {
    let mut used = ParamValues::new();
    for s in statements {
        used.extend(s.params.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    used
}

/// COMMIT/ROLLBACK for a tab without a session: its connection is gone, and with it any
/// transaction. A rollback has nothing left to do; a commit must not look like it succeeded.
pub fn end_without_session(commit: bool) -> AppResult<TxStatus> {
    if commit {
        Err(AppError::Connection("The connection was lost, so nothing was committed.".into()))
    } else {
        Ok(TxStatus::Idle)
    }
}

#[tauri::command]
pub async fn run_query(
    window: tauri::Window,
    db: State<'_, AppDb>,
    sessions: State<'_, Sessions>,
    query_sessions: State<'_, QuerySessions>,
    tab_id: String,
    req: RunRequest,
    on_event: tauri::ipc::Channel<RunEvent>,
) -> AppResult<RunOutcome> {
    let label = window.label();
    let (profile, driver) = sessions.get(label).await?;
    let database = target_database(&profile, req.database.as_deref());
    let tab = query_sessions.get_or_open(label, &tab_id, database, driver.open_query_session(database)).await?;
    let Ok(_running) = tab.run_lock.try_lock() else {
        return Err(AppError::Query(QUERY_RUNNING.into()));
    };
    let result = runner::run_on_tab(&driver.query_language(), &tab, &profile.environment, &req, &|e| {
        if let Err(err) = on_event.send(e) {
            eprintln!("could not send a run event: {err}");
        }
    })
    .await;
    if tab.session.is_closed() {
        let message = lost_connection_message(tab.session.lost_reason().as_deref());
        query_sessions.close(label, &tab_id).await;
        return Err(AppError::Connection(message));
    }
    let report = result?;
    if let RunOutcome::Completed { started_at, .. } = &report.outcome {
        let (h_database, h_schema) = history_target(&profile, &req);
        let entries = runner::history_for_run(&report.statements, *started_at, h_database, h_schema);
        if !entries.is_empty()
            && let Err(e) = db.add_history(&profile.id, &entries).await
        {
            eprintln!("could not save query history: {e}");
        }
        if let Some(id) = &req.saved_query_id
            && let Err(e) = db.set_saved_query_params(id, &used_params(&report.statements)).await
        {
            eprintln!("could not save query parameters: {e}");
        }
    }
    Ok(report.outcome)
}

#[tauri::command]
pub async fn cancel_query(
    window: tauri::Window,
    query_sessions: State<'_, QuerySessions>,
    tab_id: String,
) -> AppResult<()> {
    match query_sessions.get(window.label(), &tab_id).await {
        Some(tab) => tab.session.cancel().await,
        None => Ok(()),
    }
}

#[tauri::command]
pub async fn end_transaction(
    window: tauri::Window,
    query_sessions: State<'_, QuerySessions>,
    tab_id: String,
    commit: bool,
) -> AppResult<TxStatus> {
    let Some(tab) = query_sessions.get(window.label(), &tab_id).await else {
        return end_without_session(commit);
    };
    let Ok(_running) = tab.run_lock.try_lock() else {
        return Err(AppError::Query("A query is running in this tab".into()));
    };
    crate::query::end_tab_transaction(tab.session.as_ref(), commit).await
}

#[tauri::command]
pub async fn close_query_session(
    window: tauri::Window,
    query_sessions: State<'_, QuerySessions>,
    tab_id: String,
) -> AppResult<()> {
    query_sessions.close(window.label(), &tab_id).await;
    Ok(())
}

/// The database and schema a run's history entries record: the database only when it isn't the
/// profile's own (`target_database`), the schema only when named.
pub fn history_target<'a>(profile: &ConnectionProfile, req: &'a RunRequest) -> (Option<&'a str>, Option<&'a str>) {
    let schema = req.schema.as_deref().filter(|s| !s.is_empty());
    (target_database(profile, req.database.as_deref()), schema)
}

/// The database a target names, or `None` for the profile's own (named or not), so a tab
/// on the profile's database never reopens its session just because the name was sent.
/// Only a server has other databases: for a file (SQLite) it is always `None`.
pub fn target_database<'a>(profile: &ConnectionProfile, database: Option<&'a str>) -> Option<&'a str> {
    let own = profile.network().ok()?.database.as_str();
    database.filter(|db| !db.is_empty() && *db != own)
}

#[tauri::command]
pub async fn list_databases(window: tauri::Window, sessions: State<'_, Sessions>) -> AppResult<Vec<String>> {
    let (_, driver) = sessions.get(window.label()).await?;
    driver.list_databases().await
}

/// Schemas of `database` (`None` = the profile's own), for the target chip.
#[tauri::command]
pub async fn target_schemas(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    database: Option<String>,
) -> AppResult<Vec<String>> {
    let (profile, driver) = sessions.get(window.label()).await?;
    driver.list_schemas(target_database(&profile, database.as_deref())).await
}

/// Completion columns for `schema` in `database` (`None` = the profile's own).
#[tauri::command]
pub async fn target_columns(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    database: Option<String>,
    schema: String,
) -> AppResult<BTreeMap<String, Vec<String>>> {
    let (profile, driver) = sessions.get(window.label()).await?;
    driver.schema_columns(target_database(&profile, database.as_deref()), &schema).await
}

/// Completion field paths of `collection` in `schema` (a MongoDB database); none on SQL.
#[tauri::command]
pub async fn collection_fields(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    schema: String,
    collection: String,
) -> AppResult<Vec<String>> {
    let (_, driver) = sessions.get(window.label()).await?;
    driver.field_paths(&schema, &collection).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;
    use crate::secrets::MemoryStore;

    /// A store whose writes always fail, like a locked Keychain.
    struct FailingStore;

    impl SecretStore for FailingStore {
        fn get(&self, _: &str) -> AppResult<Option<String>> {
            Ok(None)
        }
        fn set(&self, _: &str, _: &str) -> AppResult<()> {
            Err(AppError::Secret("keychain locked".into()))
        }
        fn delete(&self, _: &str) -> AppResult<()> {
            Err(AppError::Secret("keychain locked".into()))
        }
    }

    fn profile(id: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: id.into(),
            name: "x".into(),
            kind: DbKind::Postgres,
            default_schema: None,
            environment: Environment::Local,
            connect_timeout_secs: 10,
            read_only: false,
            startup_sql: None,
            statement_timeout_secs: None,
            transport: Transport::Network(NetworkTarget {
                host: "localhost".into(),
                port: 5432,
                user: "u".into(),
                database: "d".into(),
                ssl_mode: SslMode::Prefer,
                ssl_root_cert: None,
                auth_source: None,
            }),
        }
    }

    async fn db() -> (tempfile::TempDir, AppDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = AppDb::open(&dir.path().join("app.db")).await.unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn format_mongo_formats_or_gives_the_lexers_message() {
        assert_eq!(format_mongo("db.a.find( { a:1 } )".into()).await.unwrap(), "db.a.find({ a: 1 })");
        match format_mongo(r#"db.a.find({ a: "x"#.into()).await {
            Err(AppError::Query(m)) => assert_eq!(m, "Unclosed string."),
            other => panic!("expected a query error, got {other:?}"),
        }
    }

    #[test]
    fn apply_password_none_keeps_empty_deletes_some_sets() {
        let s = MemoryStore::default();
        let key = password_key("c1");
        apply_password(&s, "c1", Some("pw".into())).unwrap();
        assert_eq!(s.get(&key).unwrap().as_deref(), Some("pw"));
        apply_password(&s, "c1", None).unwrap();
        assert_eq!(s.get(&key).unwrap().as_deref(), Some("pw"));
        apply_password(&s, "c1", Some(String::new())).unwrap();
        assert_eq!(s.get(&key).unwrap(), None);
        apply_password(&s, "c1", Some(String::new())).unwrap();
    }

    #[tokio::test]
    async fn save_new_profile_stores_password() {
        let (_d, db) = db().await;
        let s = MemoryStore::default();
        let saved = save_profile(&db, &s, profile(""), Some("pw".into())).await.unwrap();
        assert_eq!(s.get(&password_key(&saved.id)).unwrap().as_deref(), Some("pw"));
        assert_eq!(db.list_connections().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn save_new_profile_rolls_back_when_password_fails() {
        let (_d, db) = db().await;
        let err = save_profile(&db, &FailingStore, profile(""), Some("pw".into())).await.unwrap_err();
        assert!(matches!(err, AppError::Secret(_)), "{err:?}");
        assert!(db.list_connections().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_existing_profile_keeps_row_when_password_fails() {
        let (_d, db) = db().await;
        let existing = db.upsert_connection(profile("")).await.unwrap();
        let err = save_profile(&db, &FailingStore, existing.clone(), Some("pw".into())).await.unwrap_err();
        assert!(matches!(err, AppError::Secret(_)), "{err:?}");
        assert_eq!(db.list_connections().await.unwrap(), vec![existing]);
    }

    #[tokio::test]
    async fn delete_removes_secret_then_row() {
        let (_d, db) = db().await;
        let s = MemoryStore::default();
        let saved = save_profile(&db, &s, profile(""), Some("pw".into())).await.unwrap();
        delete_profile(&db, &s, &saved.id).await.unwrap();
        assert_eq!(s.get(&password_key(&saved.id)).unwrap(), None);
        assert!(db.list_connections().await.unwrap().is_empty());
        // A profile without a stored password deletes fine.
        let bare = save_profile(&db, &s, profile(""), None).await.unwrap();
        delete_profile(&db, &s, &bare.id).await.unwrap();
    }

    #[test]
    fn target_database_is_none_for_the_profiles_own() {
        let p = profile("c1"); // database "d"
        assert_eq!(target_database(&p, None), None);
        assert_eq!(target_database(&p, Some("d")), None);
        assert_eq!(target_database(&p, Some("")), None);
        assert_eq!(target_database(&p, Some("other")), Some("other"));
    }

    #[test]
    fn history_records_another_database_and_a_named_schema() {
        let p = profile("c1"); // database "d"
        let req = |database: Option<&str>, schema: Option<&str>| RunRequest {
            sql: "select 1".into(),
            target: crate::query::RunTarget::All,
            params: BTreeMap::new(),
            schema: schema.map(String::from),
            confirmed: false,
            fetch_all: false,
            saved_query_id: None,
            database: database.map(String::from),
        };
        assert_eq!(history_target(&p, &req(None, Some("public"))), (None, Some("public")));
        assert_eq!(history_target(&p, &req(Some("d"), Some("public"))), (None, Some("public")));
        assert_eq!(history_target(&p, &req(Some("other"), Some("sales"))), (Some("other"), Some("sales")));
        assert_eq!(history_target(&p, &req(Some("other"), Some(""))), (Some("other"), None));
        assert_eq!(history_target(&p, &req(Some("other"), None)), (Some("other"), None));
        // A file has no other database.
        assert_eq!(history_target(&file_profile("/tmp/a.sqlite"), &req(Some("other"), Some("main"))), (None, Some("main")));
    }

    fn file_profile(path: &str) -> ConnectionProfile {
        ConnectionProfile {
            kind: DbKind::Sqlite,
            transport: Transport::File { file_path: path.into() },
            ..profile("s1")
        }
    }

    #[test]
    fn target_database_is_none_for_a_file_profile_whatever_is_sent() {
        // A SQLite file is one database: its query tabs never target another one.
        let p = file_profile("/Users/me/data/app.sqlite");
        assert_eq!(target_database(&p, None), None);
        assert_eq!(target_database(&p, Some("")), None);
        assert_eq!(target_database(&p, Some("app.sqlite")), None);
        assert_eq!(target_database(&p, Some("other")), None);
    }

    #[tokio::test]
    async fn describe_lands_a_sqlite_connection_on_main() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sqlite");
        rusqlite::Connection::open(&path).unwrap().execute_batch("create table t(x)").unwrap();
        let p = file_profile(path.to_str().unwrap());
        let driver = driver::connect(&p, None).await.unwrap();
        let info = describe(&p, driver.as_ref()).await;
        driver.close().await;
        let info = info.unwrap();
        assert_eq!(info.schemas, vec!["main".to_string()]);
        assert_eq!(info.schema, "main");
        // The query tab's session opens on the file when the UI names it (an older tab state).
        let session = driver::connect(&p, None).await.unwrap();
        let tab = session.open_query_session(target_database(&p, Some("a.sqlite"))).await;
        assert!(tab.is_ok(), "{:?}", tab.err());
        tab.unwrap().close().await;
        session.close().await;
    }

    #[test]
    fn initial_schema_prefers_a_mysql_profiles_database() {
        let schemas: Vec<String> = ["app", "d", "public"].map(String::from).to_vec();
        // Postgres keeps its order: default schema, `public`, the first.
        let pg = profile("p"); // database "d"
        assert_eq!(initial_schema(&pg, &schemas), "public");
        let mut my = ConnectionProfile { kind: DbKind::Mysql, ..profile("m") };
        assert_eq!(initial_schema(&my, &schemas), "d");
        my.default_schema = Some("app".into());
        assert_eq!(initial_schema(&my, &schemas), "app");
        my.default_schema = None;
        if let Transport::Network(n) = &mut my.transport {
            n.database = "gone".into();
        }
        assert_eq!(initial_schema(&my, &schemas), "public");
        assert_eq!(initial_schema(&my, &["b".to_string(), "a".to_string()]), "b");
        assert_eq!(initial_schema(&my, &[]), "public");
    }

    #[test]
    fn mysql_initial_schema_is_the_connections_database_never_public() {
        let schemas: Vec<String> = ["app", "d", "public"].map(String::from).to_vec();
        let mut my = ConnectionProfile { kind: DbKind::Mysql, ..profile("m") }; // database "d"
        assert_eq!(mysql_initial_schema(&my, Some("d"), &schemas), "d");
        // The server's spelling, or the database the startup SQL chose.
        assert_eq!(mysql_initial_schema(&my, Some("D"), &schemas), "D");
        assert_eq!(mysql_initial_schema(&my, Some("app"), &schemas), "app");
        my.default_schema = Some("public".into());
        assert_eq!(mysql_initial_schema(&my, Some("d"), &schemas), "public");
        my.default_schema = None;
        // No profile database: the first listed, not `public`.
        if let Transport::Network(n) = &mut my.transport {
            n.database = String::new();
        }
        assert_eq!(mysql_initial_schema(&my, None, &schemas), "app");
        assert_eq!(mysql_initial_schema(&my, Some("x"), &schemas), "app");
        assert_eq!(mysql_initial_schema(&my, None, &[]), "");
        assert_eq!(mysql_initial_schema(&my, Some("x"), &[]), "x");
    }

    #[test]
    fn mongo_initial_schema() {
        let schemas: Vec<String> = ["app", "d", "public"].map(String::from).to_vec();
        let mut mg = ConnectionProfile { kind: DbKind::Mongodb, ..profile("g") }; // database "d"
        assert_eq!(super::mongo_initial_schema(&mg, &schemas), "d");
        // Not listed yet (a read user, or a database MongoDB creates on its first write).
        if let Transport::Network(n) = &mut mg.transport {
            n.database = "fresh".into();
        }
        assert_eq!(super::mongo_initial_schema(&mg, &schemas), "fresh");
        // No profile database: the first listed, never `public`; nothing listed: none.
        if let Transport::Network(n) = &mut mg.transport {
            n.database = String::new();
        }
        assert_eq!(super::mongo_initial_schema(&mg, &schemas), "app");
        assert_eq!(super::mongo_initial_schema(&mg, &["public".to_string()]), "public");
        assert_eq!(super::mongo_initial_schema(&mg, &[]), "");
    }

    #[test]
    fn a_chosen_file_is_its_path() {
        let chosen = tauri_plugin_dialog::FilePath::Path("/Users/me/data/app.sqlite".into());
        assert_eq!(chosen_file_path(chosen).unwrap(), "/Users/me/data/app.sqlite");
    }

    #[test]
    fn end_without_session_refuses_commit_and_allows_rollback() {
        assert_eq!(
            end_without_session(true),
            Err(AppError::Connection("The connection was lost, so nothing was committed.".into()))
        );
        assert_eq!(end_without_session(false), Ok(TxStatus::Idle));
    }

    #[test]
    fn lost_connection_message_includes_the_reason_when_known() {
        assert_eq!(
            lost_connection_message(Some("terminating connection due to administrator command")),
            "The connection to the database was lost (terminating connection due to administrator command). Run again to reconnect."
        );
        assert_eq!(lost_connection_message(None), "The connection to the database was lost. Run again to reconnect.");
    }

    #[test]
    fn used_params_is_the_union_of_statement_params() {
        use crate::query::Outcome;
        let stmt = |params: &[(&str, Option<&str>)]| StatementResult {
            index: 0,
            from: 0,
            to: 1,
            verb: "SELECT".into(),
            kind: crate::sqltext::StatementKind::Read,
            sql: String::new(),
            params: params.iter().map(|(k, v)| (k.to_string(), v.map(Into::into))).collect(),
            outcome: Outcome::Done,
            elapsed_ms: 0,
            notices: vec![],
        };
        let used = used_params(&[stmt(&[("a", Some("1"))]), stmt(&[("b", None), ("a", Some("1"))]), stmt(&[])]);
        let want: ParamValues =
            [("a".to_string(), Some("1".to_string())), ("b".to_string(), None)].into_iter().collect();
        assert_eq!(used, want);
    }

    fn fake_driver() -> Arc<dyn Driver> {
        struct NoDriver;
        #[async_trait::async_trait]
        impl Driver for NoDriver {
            fn kind(&self) -> DbKind {
                DbKind::Postgres
            }
            fn dialect(&self) -> &dyn crate::driver::Dialect {
                &crate::driver::PostgresDialect
            }
            fn query_language(&self) -> crate::query::QueryLanguage {
                crate::query::QueryLanguage::Sql(crate::sqltext::SqlDialect::Postgres)
            }
            async fn server_version(&self) -> AppResult<String> {
                unimplemented!()
            }
            async fn list_databases(&self) -> AppResult<Vec<String>> {
                unimplemented!()
            }
            async fn list_schemas(&self, _: Option<&str>) -> AppResult<Vec<String>> {
                unimplemented!()
            }
            async fn schema_tree(&self, _: &str) -> AppResult<SchemaTree> {
                unimplemented!()
            }
            async fn table_structure(&self, _: &TableRef) -> AppResult<TableStructure> {
                unimplemented!()
            }
            async fn fetch_page(&self, _: &PageRequest) -> AppResult<TablePage> {
                unimplemented!()
            }
            async fn export_table(&self, _: &TableQuery, _: &mut dyn crate::export::RowSink) -> AppResult<()> {
                unimplemented!()
            }
            async fn apply_changes(&self, _: &ChangeSet) -> AppResult<ApplyResult> {
                unimplemented!()
            }
            async fn schema_columns(&self, _: Option<&str>, _: &str) -> AppResult<BTreeMap<String, Vec<String>>> {
                unimplemented!()
            }
            async fn open_query_session(&self, _: Option<&str>) -> AppResult<Arc<dyn crate::query::QuerySession>> {
                unimplemented!()
            }
            async fn close(&self) {}
        }
        Arc::new(NoDriver)
    }

    #[tokio::test]
    async fn only_the_owner_saves_and_loads_tab_state() {
        let (_d, db) = db().await;
        let sessions = Sessions::default();
        let p = db.upsert_connection(profile("")).await.unwrap();
        sessions.set("w1", p.clone(), fake_driver()).await;
        sessions.set("w2", p.clone(), fake_driver()).await;

        save_tab_state_for(&db, &sessions, "w1", "{\"owner\":1}").await.unwrap();
        // The second window on the connection neither writes nor reads.
        save_tab_state_for(&db, &sessions, "w2", "{\"owner\":2}").await.unwrap();
        assert_eq!(db.load_tab_state(&p.id).await.unwrap().as_deref(), Some("{\"owner\":1}"));
        assert_eq!(load_tab_state_for(&db, &sessions, "w2").await.unwrap(), None);
        assert_eq!(load_tab_state_for(&db, &sessions, "w1").await.unwrap().as_deref(), Some("{\"owner\":1}"));
        assert!(load_tab_state_for(&db, &sessions, "nobody").await.is_err());

        // w1 leaves for the Connections screen: w2 takes over.
        sessions.release_tab_state("w1");
        save_tab_state_for(&db, &sessions, "w1", "{\"owner\":1,\"late\":true}").await.unwrap();
        assert_eq!(load_tab_state_for(&db, &sessions, "w2").await.unwrap().as_deref(), Some("{\"owner\":1}"));
        save_tab_state_for(&db, &sessions, "w2", "{\"owner\":2}").await.unwrap();
        assert_eq!(db.load_tab_state(&p.id).await.unwrap().as_deref(), Some("{\"owner\":2}"));
    }

    #[tokio::test]
    async fn delete_keeps_row_when_secret_delete_fails() {
        let (_d, db) = db().await;
        let saved = db.upsert_connection(profile("")).await.unwrap();
        assert!(delete_profile(&db, &FailingStore, &saved.id).await.is_err());
        assert_eq!(db.list_connections().await.unwrap().len(), 1);
    }
}

/// Opens the save dialog for `window` and waits for the answer. `None` means it was closed.
async fn ask_save_path(
    app: &tauri::AppHandle,
    window: &tauri::Window,
    default_name: &str,
    format: ExportFormat,
) -> AppResult<Option<std::path::PathBuf>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let label = match format {
        ExportFormat::Csv => "CSV",
        ExportFormat::Json => "JSON",
    };
    app.dialog()
        .file()
        .set_parent(window)
        .set_file_name(default_name)
        .add_filter(label, &[format.extension()])
        .save_file(move |path| {
            let _ = tx.send(path);
        });
    let chosen = rx.await.map_err(|_| AppError::Query("The save dialog closed unexpectedly".into()))?;
    chosen
        .map(|p| p.into_path().map_err(|e| AppError::Query(format!("Couldn't use the chosen path: {e}"))))
        .transpose()
}

/// Opens the open dialog for a SQLite database file and waits for the answer. `None` means it was
/// closed without a choice.
#[tauri::command]
pub async fn pick_database_file(window: tauri::Window) -> AppResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    window
        .app_handle()
        .dialog()
        .file()
        .set_parent(&window)
        .add_filter("SQLite database", &["sqlite", "sqlite3", "db", "db3"])
        .pick_file(move |path| {
            let _ = tx.send(path);
        });
    let chosen = rx.await.map_err(|_| AppError::Query("The file dialog closed unexpectedly".into()))?;
    chosen.map(chosen_file_path).transpose()
}

/// The chosen file as the path a profile stores.
fn chosen_file_path(path: tauri_plugin_dialog::FilePath) -> AppResult<String> {
    let path = path.into_path().map_err(|e| AppError::Query(format!("Couldn't use the chosen file: {e}")))?;
    path.into_os_string()
        .into_string()
        .map_err(|p| AppError::Query(format!("Couldn't use the chosen file: {} isn't valid UTF-8", p.to_string_lossy())))
}

fn export_file_name(base: &str, format: ExportFormat) -> String {
    let safe: String = base.chars().map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c }).collect();
    format!("{safe}.{}", format.extension())
}

fn progress_sender(on_progress: Channel<ExportProgress>) -> impl Fn(u64) + Send + Sync + 'static {
    move |rows| {
        if let Err(e) = on_progress.send(ExportProgress { rows }) {
            eprintln!("could not send export progress: {e}");
        }
    }
}

#[tauri::command]
pub async fn export_table(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    jobs: State<'_, ExportJobs>,
    id: String,
    query: TableQuery,
    format: ExportFormat,
    on_progress: Channel<ExportProgress>,
) -> AppResult<ExportOutcome> {
    let (_, driver) = sessions.get(window.label()).await?;
    let cancel = jobs.start(window.label(), &id);
    let result = async {
        let name = export_file_name(&query.table.name, format);
        let Some(target) = ask_save_path(window.app_handle(), &window, &name, format).await? else {
            return Ok(ExportOutcome::Dismissed);
        };
        let send = progress_sender(on_progress);
        run_table_export(driver.as_ref(), &query, format, &target, &cancel, &send).await
    }
    .await;
    jobs.finish(&id);
    result
}

#[tauri::command]
#[allow(clippy::too_many_arguments)] // one argument per field the UI sends
pub async fn export_rows(
    window: tauri::Window,
    jobs: State<'_, ExportJobs>,
    id: String,
    name: String,
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Option<String>>>,
    format: ExportFormat,
    on_progress: Channel<ExportProgress>,
    // One `CellType` code string per row; the UI sends it only for results that have it.
    cell_types: Option<Vec<String>>,
) -> AppResult<ExportOutcome> {
    let cancel = jobs.start(window.label(), &id);
    let result = async {
        let Some(target) = ask_save_path(window.app_handle(), &window, &export_file_name(&name, format), format).await? else {
            return Ok(ExportOutcome::Dismissed);
        };
        let send = progress_sender(on_progress);
        tokio::task::spawn_blocking(move || {
            run_rows_export(&columns, &rows, cell_types.as_deref(), format, &target, &cancel, &send)
        })
            .await
            .map_err(|e| AppError::Query(format!("The export stopped unexpectedly: {e}")))?
    }
    .await;
    jobs.finish(&id);
    result
}

/// Exports every row of a query result: its statement runs again on the tab's session (the
/// database and schema the run used), so the file isn't limited to the rows the grid holds.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // one argument per field the UI sends
pub async fn export_query(
    window: tauri::Window,
    sessions: State<'_, Sessions>,
    query_sessions: State<'_, QuerySessions>,
    jobs: State<'_, ExportJobs>,
    id: String,
    tab_id: String,
    name: String,
    sql: String,
    params: ParamValues,
    database: Option<String>,
    schema: Option<String>,
    format: ExportFormat,
    on_progress: Channel<ExportProgress>,
) -> AppResult<ExportOutcome> {
    let label = window.label();
    let (profile, driver) = sessions.get(label).await?;
    let cancel = jobs.start(label, &id);
    let result = async {
        enum Statement {
            Sql(crate::sqltext::SqlDialect, crate::sqltext::Rewritten, Vec<Option<String>>),
            Mongo(Box<crate::mongoquery::MongoCall>),
        }
        let statement = match driver.query_language() {
            QueryLanguage::Sql(dialect) => {
                let rewritten = crate::sqltext::rewrite_params_in(dialect, &sql);
                let values = rewritten
                    .bind_order
                    .iter()
                    .map(|n| params.get(n).cloned().ok_or_else(|| AppError::Query(format!("No value for :{n}"))))
                    .collect::<AppResult<Vec<_>>>()?;
                Statement::Sql(dialect, rewritten, values)
            }
            QueryLanguage::Mongo => {
                let params = crate::query::mongo_runner::bind_params(&sql, &params)?;
                Statement::Mongo(Box::new(
                    crate::mongoquery::parse(&sql, &params).map_err(|e| AppError::Query(e.message))?,
                ))
            }
        };
        let Some(target) = ask_save_path(window.app_handle(), &window, &export_file_name(&name, format), format).await? else {
            return Ok(ExportOutcome::Dismissed);
        };
        let database = target_database(&profile, database.as_deref());
        let tab = query_sessions.get_or_open(label, &tab_id, database, driver.open_query_session(database)).await?;
        let Ok(_running) = tab.run_lock.try_lock() else {
            return Err(AppError::Query(EXPORT_WHILE_RUNNING.into()));
        };
        let send = progress_sender(on_progress);
        // Where a failed switch fails the run (MySQL), it fails the export too.
        let outcome = match tab.prepare_schema(schema.as_deref()).await {
            Ok(()) => {
                match &statement {
                    Statement::Sql(dialect, rewritten, values) => {
                        run_query_export(
                            tab.session.as_ref(), *dialect, &rewritten.sql, values, format, &target, &cancel, &send,
                        )
                        .await
                    }
                    Statement::Mongo(call) => {
                        run_mongo_export(tab.session.as_ref(), call, format, &target, &cancel, &send).await
                    }
                }
            }
            Err(e) => Err(e),
        };
        if tab.session.is_closed() {
            let message = lost_connection_message(tab.session.lost_reason().as_deref());
            query_sessions.close(label, &tab_id).await;
            return Err(AppError::Connection(message));
        }
        outcome
    }
    .await;
    jobs.finish(&id);
    result
}

/// A query export needs the tab's session, which a run (or COMMIT/ROLLBACK) holds.
pub const EXPORT_WHILE_RUNNING: &str = "A query is running in this tab. Export again when it finishes.";

#[tauri::command]
pub fn cancel_export(jobs: State<'_, ExportJobs>, id: String) {
    jobs.cancel(&id);
}

/// Shows a finished export in Finder. The UI never picks a path that gets written to.
#[tauri::command]
pub fn reveal_export(app: tauri::AppHandle, path: String) -> AppResult<()> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| AppError::Query(format!("Couldn't show the file: {e}")))
}
