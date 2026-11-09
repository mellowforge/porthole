mod common;

use std::time::Duration;

use porthole_lib::driver::sqlite::{connect_sqlite, SqliteConfig, SqliteWorker};
use porthole_lib::driver::{self, Driver};
use porthole_lib::driver::sqlite::values;
use porthole_lib::error::AppError;
use porthole_lib::export::run::{run_table_export, ExportOutcome};
use porthole_lib::export::ExportFormat;
use porthole_lib::model::*;
use common::sqlite::{sqlite_fixture, sqlite_profile};
use rusqlite::ErrorCode;

const INFINITE_COUNT: &str =
    "with recursive c(x) as (select 1 union all select x + 1 from c) select count(*) from c";
/// Reads the file for as long as it runs (60,000³ rows), so it holds a read lock meanwhile.
const ENDLESS_READ: &str = "select count(*) from big a, big b, big c";

fn config(path: &std::path::Path, read_only: bool) -> SqliteConfig {
    SqliteConfig::from_profile(&sqlite_profile(path, read_only)).unwrap()
}

/// The fixture is what the SQLite tests expect.
#[test]
fn fixture_contents() {
    let (_dir, path) = sqlite_fixture();
    let c = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let one = |sql: &str| -> String { c.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(one("pragma journal_mode"), "delete");
    assert_eq!(one("select count(*) || '' from users"), "17");
    // Manual check step 8: the same 15 matches as fx.users on Postgres.
    assert_eq!(one("select count(*) || '' from users where status = 'active' and name like '%ann%'"), "15");
    assert_eq!(one("select length(bio) || ' ' || substr(bio, 1, 3) from users where id = 3"), "10000 xxx");
    assert_eq!(one("select hex(avatar) from users where id = 4"), "DEADBEEF");
    assert_eq!(one("select group_concat(typeof(i) || typeof(r) || typeof(n) || typeof(t) || typeof(b) || typeof(anyv) || typeof(d), ',') from typed"),
        "integerrealrealtextblobintegertext,integerrealintegertextblobtextnull,nullrealnullnullnullrealnull,integerrealintegertextnullnullnull");
    assert_eq!(one("select r || '' from typed where id = 4"), "Inf");
    assert_eq!(one("select group_concat(typeof(k) || ':' || v, ',') from loose"), "integer:int five,text:text five");
    assert_eq!(one("select count(*) || ' ' || max(id) from big"), "60000 60000");
    assert_eq!(one("select b || '' from gen where id = 1"), "42");
    assert_eq!(one("select group_concat(name, ',') from sqlite_master where type = 'index' and name not like 'sqlite_%'"), "orders_user");
    assert_eq!(one("select count(*) || '' from active_users"), "16");
}

#[tokio::test]
async fn connect_reports_server_version() {
    let (_dir, path) = sqlite_fixture();
    let profile = sqlite_profile(&path, false);

    let d = driver::connect(&profile, None).await.unwrap();
    let version = d.server_version().await.unwrap();
    assert!(version.starts_with("SQLite 3."), "{version}");
    d.close().await;

    let tested = driver::test_connection(&profile, None).await.unwrap();
    assert_eq!(tested.server_version, version);
}

#[tokio::test]
async fn connect_to_a_missing_file_fails_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nope.sqlite");
    for read_only in [false, true] {
        let err = driver::connect(&sqlite_profile(&path, read_only), None).await.err().unwrap();
        assert_eq!(err, AppError::Connection(format!("The file doesn't exist: {}", path.display())));
        assert!(!path.exists());
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn a_profile_without_a_file_is_refused() {
    let mut profile = sqlite_profile(std::path::Path::new("/tmp/x.sqlite"), false);
    profile.transport = porthole_lib::model::Transport::Uri { uri: "file:x".into() };
    assert_eq!(
        SqliteConfig::from_profile(&profile).err().unwrap(),
        AppError::Connection("This connection has no database file.".into())
    );
}

#[tokio::test]
async fn read_only_profile_cannot_write() {
    let (_dir, path) = sqlite_fixture();
    let w = SqliteWorker::open(&config(&path, true)).unwrap();
    let err = w.call(|c| c.execute("delete from users", [])).await.unwrap().unwrap_err();
    assert!(err.to_string().contains("readonly"), "{err}");
    let n: i64 = w.call(|c| c.query_row("select count(*) from users", [], |r| r.get(0))).await.unwrap().unwrap();
    assert_eq!(n, 17);

    // The same profile, writable, can.
    let w = SqliteWorker::open(&config(&path, false)).unwrap();
    assert_eq!(w.call(|c| c.execute("delete from nopk", [])).await.unwrap().unwrap(), 2);
}

#[tokio::test]
async fn startup_sql_runs_on_every_connection() {
    let (_dir, path) = sqlite_fixture();
    let mut profile = sqlite_profile(&path, false);
    // Not SQLite's default (-2000), so only the startup SQL can have set it, on each connection.
    profile.startup_sql = Some("PRAGMA cache_size = -1234".into());
    let d = connect_sqlite(&profile).await.unwrap();
    assert_eq!(d.workers().len(), 2);
    for w in &d.workers() {
        assert_eq!(pragma(w, "cache_size").await, -1234);
    }

    profile.startup_sql = Some("select * from no_such_table".into());
    let err = connect_sqlite(&profile).await.err().unwrap();
    match err {
        AppError::Connection(m) => assert!(m.contains("no_such_table"), "{m}"),
        other => panic!("{other:?}"),
    }
}

async fn pragma(w: &SqliteWorker, name: &'static str) -> i64 {
    w.call(move |c| c.pragma_query_value(None, name, |r| r.get(0))).await.unwrap().unwrap()
}

#[tokio::test]
async fn foreign_keys_are_on_unless_startup_sql_turns_them_off() {
    let (_dir, path) = sqlite_fixture();
    let mut profile = sqlite_profile(&path, false);
    let d = connect_sqlite(&profile).await.unwrap();
    for w in &d.workers() {
        assert_eq!(pragma(w, "foreign_keys").await, 1);
    }
    profile.startup_sql = Some("PRAGMA foreign_keys = OFF".into());
    let d = connect_sqlite(&profile).await.unwrap();
    for w in &d.workers() {
        assert_eq!(pragma(w, "foreign_keys").await, 0);
    }
}

#[tokio::test]
async fn dropping_a_call_interrupts_only_its_own_job() {
    let (_dir, path) = sqlite_fixture();
    let w = SqliteWorker::open(&config(&path, false)).unwrap();
    let gave_up = tokio::time::timeout(
        Duration::from_millis(100),
        w.call(|c| c.query_row(INFINITE_COUNT, [], |r| r.get::<_, i64>(0))),
    )
    .await;
    assert!(gave_up.is_err());
    // The endless statement was stopped, so the next job gets its turn.
    let next = tokio::time::timeout(Duration::from_secs(3), w.call(|c| c.query_row("select 1", [], |r| r.get::<_, i64>(0))))
        .await
        .expect("the abandoned statement was interrupted");
    assert_eq!(next.unwrap().unwrap(), 1);

    // A call that finished leaves nothing behind that could stop a later job.
    w.call(|c| c.query_row("select 1", [], |r| r.get::<_, i64>(0))).await.unwrap().unwrap();
    let n = w.call(|c| c.query_row("select count(*) from big", [], |r| r.get::<_, i64>(0))).await.unwrap().unwrap();
    assert_eq!(n, 60_000);
}

#[tokio::test]
async fn close_waits_for_running_work_but_not_forever() {
    let (_dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let workers = d.workers();
    let (slow, waited) = tokio::join!(
        workers[0].call(|_| std::thread::sleep(Duration::from_secs(4))),
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let started = std::time::Instant::now();
            d.close().await;
            started.elapsed()
        }
    );
    // Running work finishes; close gives up waiting for it after 2 s.
    slow.unwrap();
    assert!(waited >= Duration::from_millis(1900) && waited < Duration::from_millis(3500), "{waited:?}");

    // With nothing running, close returns once the threads have ended.
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let started = std::time::Instant::now();
    d.close().await;
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn interrupt_stops_a_running_statement() {
    let (_dir, path) = sqlite_fixture();
    let w = SqliteWorker::open(&config(&path, false)).unwrap();
    let (result, ()) = tokio::join!(
        w.call(|c| c.query_row(INFINITE_COUNT, [], |r| r.get::<_, i64>(0))),
        async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            w.interrupt_current();
        }
    );
    let err = result.unwrap().unwrap_err();
    assert_eq!(err.sqlite_error_code(), Some(ErrorCode::OperationInterrupted), "{err}");

    let one: i64 = w.call(|c| c.query_row("select 1", [], |r| r.get(0))).await.unwrap().unwrap();
    assert_eq!(one, 1);
    assert!(!w.is_closed());
}

#[tokio::test]
async fn calls_run_in_order_on_one_connection() {
    let (_dir, path) = sqlite_fixture();
    let w = SqliteWorker::open(&config(&path, false)).unwrap();
    w.call(|c| c.execute_batch("create temp table seen (n integer)")).await.unwrap().unwrap();
    let (a, b) = tokio::join!(
        w.call(|c| c.execute("insert into seen values (1)", [])),
        w.call(|c| c.execute("insert into seen values (2)", [])),
    );
    a.unwrap().unwrap();
    b.unwrap().unwrap();
    // A temp table is private to its connection: both inserts ran on the same one.
    let n: i64 = w.call(|c| c.query_row("select count(*) from seen", [], |r| r.get(0))).await.unwrap().unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn a_call_given_up_before_its_turn_never_runs() {
    let (_dir, path) = sqlite_fixture();
    let w = SqliteWorker::open(&config(&path, false)).unwrap();
    w.call(|c| c.execute_batch("create temp table seen (n integer)")).await.unwrap().unwrap();
    let (busy, gave_up) = tokio::join!(
        w.call(|_| std::thread::sleep(Duration::from_millis(300))),
        tokio::time::timeout(Duration::from_millis(100), w.call(|c| c.execute("insert into seen values (1)", []))),
    );
    busy.unwrap();
    assert!(gave_up.is_err());
    let n: i64 = w.call(|c| c.query_row("select count(*) from seen", [], |r| r.get(0))).await.unwrap().unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn a_worker_whose_thread_ended_reports_closed() {
    let (_dir, path) = sqlite_fixture();
    let w = SqliteWorker::open(&config(&path, false)).unwrap();
    let closed = AppError::Connection("The database connection is closed.".into());
    let r = w.call(|_| -> i64 { panic!("the job fails") }).await;
    assert_eq!(r.err().unwrap(), closed);
    assert!(w.is_closed());
    assert_eq!(w.call(|_| 1).await.err().unwrap(), closed);
}

#[tokio::test]
async fn a_closed_driver_refuses_new_work() {
    let (_dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    d.close().await;
    assert!(d.workers().iter().all(|w| w.is_closed()));
    assert_eq!(
        d.server_version().await.err().unwrap(),
        AppError::Connection("The database connection is closed.".into())
    );
}

#[tokio::test]
async fn an_ended_browse_worker_is_replaced_and_the_next_page_loads() {
    let (_dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    for w in d.workers() {
        assert!(w.call(|_| -> i64 { panic!("the job fails") }).await.is_err());
        assert!(w.is_closed());
    }
    for _ in 0..d.workers().len() {
        let page = d.fetch_page(&page_req("users", None, None, 0, 100)).await.unwrap();
        assert_eq!(page.total, RowCount::Exact(17));
    }
    assert!(d.workers().iter().all(|w| !w.is_closed()));
    d.close().await;
}

#[tokio::test]
async fn startup_sql_failures_say_so() {
    let (_dir, path) = sqlite_fixture();
    let mut profile = sqlite_profile(&path, false);
    profile.startup_sql = Some("select * from no_such_table".into());
    let err = connect_sqlite(&profile).await.err().unwrap();
    assert_eq!(err, AppError::Connection("Startup SQL failed: no such table: no_such_table".into()));
}

#[tokio::test]
async fn an_empty_file_path_asks_for_a_file() {
    for path in ["", "  "] {
        let profile = sqlite_profile(std::path::Path::new(path), false);
        assert_eq!(SqliteConfig::from_profile(&profile).err().unwrap(), AppError::Connection("Choose a database file.".into()));
        assert_eq!(
            driver::connect(&profile, None).await.err().unwrap(),
            AppError::Connection("Choose a database file.".into())
        );
    }
}

#[tokio::test]
async fn a_type_that_does_not_match_the_address_is_refused_at_connect() {
    let mismatch = AppError::Connection("This connection's type doesn't match its address. Edit it and save again.".into());
    let (_dir, path) = sqlite_fixture();
    // A PostgreSQL profile pointing at a file, and a SQLite profile with a server address.
    let mut pg_file = sqlite_profile(&path, false);
    pg_file.kind = DbKind::Postgres;
    assert_eq!(driver::connect(&pg_file, None).await.err().unwrap(), mismatch);
    assert_eq!(driver::test_connection(&pg_file, None).await.err().unwrap(), mismatch);
    let mut sqlite_net = sqlite_profile(&path, false);
    sqlite_net.transport = Transport::Network(NetworkTarget {
        host: "localhost".into(),
        port: 5432,
        user: "app".into(),
        database: "app".into(),
        ssl_mode: SslMode::Prefer,
        ssl_root_cert: None,
        auth_source: None,
    });
    assert_eq!(driver::connect(&sqlite_net, None).await.err().unwrap(), mismatch);
    let mut pg_uri = sqlite_profile(&path, false);
    pg_uri.kind = DbKind::Postgres;
    pg_uri.transport = Transport::Uri { uri: "postgres://localhost/app".into() };
    assert_eq!(driver::connect(&pg_uri, None).await.err().unwrap(), mismatch);
}

#[tokio::test]
async fn dropping_a_worker_stops_its_running_statement() {
    let (_dir, path) = sqlite_fixture();
    let w = SqliteWorker::open(&config(&path, false)).unwrap();
    // The caller gives up on a never-ending statement; then the worker goes away.
    let gave_up = tokio::time::timeout(
        Duration::from_millis(200),
        w.call(|c| c.query_row(ENDLESS_READ, [], |r| r.get::<_, i64>(0))),
    )
    .await;
    assert!(gave_up.is_err());
    drop(w);
    // The statement still holds its read lock unless the drop stopped it; then a write would wait
    // out the 5 s busy timeout and fail.
    let w2 = SqliteWorker::open(&config(&path, false)).unwrap();
    let changed = tokio::time::timeout(
        Duration::from_secs(3),
        w2.call(|c| c.execute("update users set plan = plan where id = 1", [])),
    )
    .await
    .expect("not blocked by the dropped worker");
    assert_eq!(changed.unwrap().unwrap(), 1);
}

#[tokio::test]
async fn browsing_a_wal_database_leaves_the_main_file_byte_identical() {
    let (_dir, path) = sqlite_fixture();
    {
        // Setup only: another program has the file in WAL mode, with a committed change still in
        // the -wal file (it closed without a checkpoint).
        let c = rusqlite::Connection::open(&path).unwrap();
        let mode: String = c.query_row("pragma journal_mode = wal", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        c.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).unwrap();
        c.execute("update users set plan = 'wal' where id = 1", []).unwrap();
        c.close().map_err(|(_, e)| e).unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let dir = path.parent().unwrap().to_path_buf();
    let wal = dir.join("fx.sqlite-wal");
    let wal_size = std::fs::metadata(&wal).unwrap().len();
    assert!(wal_size > 0);
    let listing = || {
        let mut names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    };
    let files = listing();
    // Nothing but the database and its -wal and -shm files, before and after.
    let only_wal_files = |names: &[String]| {
        names.iter().all(|n| files.contains(n) || n == "fx.sqlite-wal" || n == "fx.sqlite-shm")
    };
    let read_plan = |c: &mut rusqlite::Connection| -> String {
        c.query_row("select plan from users where id = 1", [], |r| r.get(0)).unwrap()
    };

    // One connection, alone, so its close is certainly the last one (where SQLite would checkpoint).
    let w = SqliteWorker::open(&config(&path, false)).unwrap();
    assert_eq!(w.call(read_plan).await.unwrap(), "wal");
    assert!(w.shutdown(Duration::from_secs(5)).await);
    assert!(std::fs::read(&path).unwrap() == before, "the main file changed");
    assert!(only_wal_files(&listing()), "{:?}", listing());
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), wal_size, "the -wal file changed size");

    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    d.server_version().await.unwrap();
    for w in d.workers() {
        assert_eq!(w.call(read_plan).await.unwrap(), "wal");
    }
    d.close().await;
    drop(d);

    assert!(std::fs::read(&path).unwrap() == before, "the main file changed");
    assert!(only_wal_files(&listing()), "{:?}", listing());
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), wal_size, "the -wal file changed size");
}

#[tokio::test]
async fn browsing_leaves_the_file_byte_identical() {
    let (dir, path) = sqlite_fixture();
    let before = std::fs::read(&path).unwrap();
    let exports = tempfile::tempdir().unwrap();
    let no_cancel = std::sync::atomic::AtomicBool::new(false);

    let d = driver::connect(&sqlite_profile(&path, false), None).await.unwrap();
    d.server_version().await.unwrap();
    for schema in d.list_schemas(None).await.unwrap() {
        let tree = d.schema_tree(&schema).await.unwrap();
        d.schema_columns(None, &schema).await.unwrap();
        for name in tree.tables.iter().chain(&tree.views) {
            let t = TableRef { schema: schema.clone(), name: name.clone() };
            let s = d.table_structure(&t).await.unwrap();
            // A filtered, sorted page (both counts run) and an unfiltered one.
            let first = &s.columns[0].name;
            // Backticks: a column name may hold spaces.
            let filter = format!("`{first}` is not null");
            let sorted = page_req(name, Some(&filter), Some((first, true)), 0, 100);
            let sort = sorted.sort.clone();
            let page = d.fetch_page(&PageRequest { table: t.clone(), ..sorted }).await.unwrap();
            assert!(page.matches.is_some(), "{name}");
            d.fetch_page(&PageRequest { table: t.clone(), ..page_req(name, None, None, 0, 100) }).await.unwrap();
            // A table export with the same filter and sort, on a connection of its own. (The
            // target is in another directory: this one must hold only the database.)
            let q = TableQuery { table: t, filter: Some(filter), sort };
            let target = exports.path().join(format!("{name}.csv"));
            let out = run_table_export(&*d, &q, ExportFormat::Csv, &target, &no_cancel, &|_| {}).await.unwrap();
            assert!(matches!(out, ExportOutcome::Done { .. }), "{name}: {out:?}");
        }
    }
    d.close().await;
    drop(d);

    assert_eq!(std::fs::read(&path).unwrap(), before);
    let names: Vec<String> =
        std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["fx.sqlite".to_string()], "no -journal or -wal file");
}

fn main_table(name: &str) -> TableRef {
    TableRef { schema: "main".into(), name: name.into() }
}

fn page_req(table: &str, filter: Option<&str>, sort: Option<(&str, bool)>, page: u32, page_size: u32) -> PageRequest {
    PageRequest {
        table: main_table(table),
        filter: filter.map(str::to_string),
        sort: sort.map(|(column, descending)| SortSpec { column: column.into(), descending }),
        page,
        page_size,
    }
}

/// Runs SQL on the fixture file before the driver opens it. Test setup only.
fn setup(path: &std::path::Path, sql: &str) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(sql).unwrap();
    c.close().map_err(|(_, e)| e).unwrap();
}

/// Every row of `sql` as the grid's text, read with rusqlite directly.
fn direct_rows(path: &std::path::Path, sql: &str) -> Vec<Vec<Option<String>>> {
    let c = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = c.prepare(sql).unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| (0..n).map(|i| Ok(values::cell(r.get_ref(i)?).0)).collect::<rusqlite::Result<Vec<_>>>())
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn texts(row: &[&str]) -> Vec<Option<String>> {
    row.iter().map(|s| Some(s.to_string())).collect()
}

#[tokio::test]
async fn sqlite_schemas_and_tree() {
    let (_dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    assert_eq!(d.list_schemas(None).await.unwrap(), vec!["main".to_string()]);
    assert_eq!(d.list_databases().await.unwrap(), Vec::<String>::new());
    let tree = d.schema_tree("main").await.unwrap();
    assert_eq!(
        tree.tables,
        ["Order Items", "big", "floatkey", "gen", "loose", "nopk", "orders", "typed", "users"].map(String::from)
    );
    assert_eq!(tree.views, vec!["active_users".to_string()]);
    assert!(tree.functions.is_empty());
    d.close().await;
}

#[tokio::test]
async fn sqlite_structure() {
    let (_dir, path) = sqlite_fixture();
    // Beyond the shared fixture: a declared type with a length, a foreign key without its
    // parent columns, and an expression index.
    setup(
        &path,
        "create table child (pid references users, v VARCHAR(20), w Double Precision);
         create index child_expr on child(lower(v), pid);",
    );
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let s = |name: &'static str| {
        let d = &d;
        async move { d.table_structure(&main_table(name)).await.unwrap() }
    };

    let users = s("users").await;
    assert_eq!(users.primary_key, vec!["id".to_string()]);
    assert_eq!(users.relation, RelationKind::Table);
    assert_eq!(users.editing, Editing::Allowed);
    let names: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "email", "status", "plan", "created_at", "bio", "avatar"]);
    let id = &users.columns[0];
    assert_eq!((id.data_type.as_str(), id.cast_type.as_str(), id.full_type.as_str()), ("integer", "integer", "INTEGER"));
    assert!(id.is_primary_key && !id.read_only);
    let name = &users.columns[1];
    assert!(!name.nullable && !name.is_primary_key);
    assert!(users.columns[2].nullable);
    assert_eq!(users.columns[5].default.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(users.columns[7].cast_type, "blob");

    let loose = s("loose").await;
    let k = &loose.columns[0];
    assert_eq!((k.data_type.as_str(), k.full_type.as_str(), k.cast_type.as_str()), ("", "", "blob"));
    assert_eq!(loose.editing, Editing::Allowed);

    let generated = s("gen").await;
    let read_only: Vec<(&str, bool)> = generated.columns.iter().map(|c| (c.name.as_str(), c.read_only)).collect();
    assert_eq!(read_only, [("id", false), ("a", false), ("b", true)]);

    assert_eq!(s("nopk").await.editing, Editing::NoPrimaryKey);
    assert_eq!(s("floatkey").await.editing, Editing::FloatKey);
    let view = s("active_users").await;
    assert_eq!((view.relation, view.editing), (RelationKind::View, Editing::View));
    assert_eq!(view.columns.len(), 8);

    let orders = s("orders").await;
    let idx = orders.indexes.iter().find(|i| i.name == "orders_user").expect("orders_user");
    assert_eq!(idx.columns, vec!["user_id".to_string()]);
    assert!(!idx.unique && !idx.primary);
    assert_eq!(
        orders.foreign_keys,
        vec![ForeignKeyInfo {
            name: "fk0".into(),
            columns: vec!["user_id".into()],
            ref_table: main_table("users"),
            ref_columns: vec!["id".into()],
        }]
    );

    let child = s("child").await;
    let types: Vec<(&str, &str, &str)> =
        child.columns.iter().map(|c| (c.data_type.as_str(), c.full_type.as_str(), c.cast_type.as_str())).collect();
    assert_eq!(types, [("", "", "blob"), ("varchar(20)", "VARCHAR(20)", "text"), ("double precision", "Double Precision", "real")]);
    // No parent columns given: the parent's primary key.
    assert_eq!(child.foreign_keys[0].ref_columns, vec!["id".to_string()]);
    assert_eq!(child.foreign_keys[0].columns, vec!["pid".to_string()]);
    let expr = child.indexes.iter().find(|i| i.name == "child_expr").expect("child_expr");
    assert_eq!(expr.columns, vec!["pid".to_string()]);

    // A key from the `pk` declaration of a rowid-less table's index is primary.
    assert!(s("floatkey").await.indexes.iter().any(|i| i.primary && i.columns == ["k"]));

    assert_eq!(
        d.table_structure(&main_table("no_such_table")).await.err().unwrap(),
        AppError::NotFound("main.no_such_table".into())
    );
    d.close().await;
}

#[tokio::test]
async fn sqlite_page_filter_sort() {
    let (_dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();

    let page = d.fetch_page(&page_req("users", None, None, 0, 100)).await.unwrap();
    assert_eq!(page.rows.len(), 17);
    assert_eq!(page.total, RowCount::Exact(17));
    assert_eq!(page.matches, None);
    assert_eq!(page.primary_key, vec!["id".to_string()]);
    assert_eq!(page.editing, Editing::Allowed);
    let types = page.cell_types.as_ref().unwrap();
    assert_eq!(types.len(), 17);
    assert!(types.iter().all(|t| t.chars().count() == 8), "{types:?}");
    assert_eq!(page.rows, direct_rows(&path, "select * from users order by id"));

    // The manual check's step 8 filter: 15 matches, as fx.users on Postgres (pg_integration.rs,
    // fetch_filter_spec_example_matches_15).
    let f = r#"status = "active" and name like "%ann%""#;
    let page = d.fetch_page(&page_req("users", Some(f), None, 0, 100)).await.unwrap();
    assert_eq!(page.matches, Some(RowCount::Exact(15)));
    assert_eq!(page.total, RowCount::Exact(17));
    assert_eq!(page.rows.len(), 15);

    let sorted = d.fetch_page(&page_req("users", None, Some(("name", true)), 0, 100)).await.unwrap();
    let direct = direct_rows(&path, "select * from users order by name desc nulls last, id");
    assert_eq!(sorted.rows[0], direct[0]);
    assert_eq!(sorted.rows, direct);

    // Paging follows the same order.
    let second = d.fetch_page(&page_req("users", None, Some(("name", true)), 1, 5)).await.unwrap();
    assert_eq!(second.rows, direct[5..10].to_vec());

    // NULLS LAST, ascending too.
    let by_plan = d.fetch_page(&page_req("users", None, Some(("plan", false)), 0, 100)).await.unwrap();
    assert_eq!(by_plan.rows, direct_rows(&path, "select * from users order by plan asc nulls last, id"));
    assert_eq!(by_plan.rows[16][0].as_deref(), Some("1"));

    // A name with a space and quotes in the SQL text.
    let items = d.fetch_page(&page_req("Order Items", None, Some(("name", false)), 0, 100)).await.unwrap();
    assert_eq!(items.rows, vec![texts(&["2", "a"]), texts(&["1", "b"])]);

    // Filter errors: SQLite's own, and the filter compiler's.
    let too_complex = format!("name like \"{}\"", "x".repeat(50_001));
    let err = d.fetch_page(&page_req("users", Some(&too_complex), None, 0, 100)).await.unwrap_err();
    assert!(matches!(&err, AppError::Filter { message, .. } if message.contains("too complex")), "{err:?}");
    let err = d.fetch_page(&page_req("users", Some("nope = 1"), None, 0, 100)).await.unwrap_err();
    assert_eq!(err, AppError::Filter { message: "Unknown column \"nope\"".into(), position: 0 });
    let err = d.fetch_page(&page_req("users", None, Some(("nope", false)), 0, 100)).await.unwrap_err();
    assert_eq!(err, AppError::Query("Unknown column nope".into()));
    d.close().await;
}

#[tokio::test]
async fn sqlite_typed_values() {
    let (_dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let page = d.fetch_page(&page_req("typed", None, None, 0, 100)).await.unwrap();
    let types = page.cell_types.unwrap();

    assert_eq!(page.rows[0], texts(&["1", "42", "1.0", "1.5", "x", "\\xdead", "5", "2026-01-01"]));
    assert_eq!(types[0], "iirrtbit");
    let c = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let stored: String = c
        .query_row(
            "select typeof(id) || ',' || typeof(i) || ',' || typeof(r) || ',' || typeof(n) || ',' || typeof(t)
                    || ',' || typeof(b) || ',' || typeof(anyv) || ',' || typeof(d) from typed where id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let codes: String = stored
        .split(',')
        .map(|t| match t {
            "integer" => 'i',
            "real" => 'r',
            "text" => 't',
            "blob" => 'b',
            other => panic!("{other}"),
        })
        .collect();
    assert_eq!(codes, types[0]);

    let mut row2 = texts(&["2", "-7", "0.1", "10", "", "\\x", "5"]);
    row2.push(None);
    assert_eq!(page.rows[1], row2);
    assert_eq!(types[1], "iiritbtn");
    assert_eq!(page.rows[3][2].as_deref(), Some("Inf"));
    d.close().await;
}

/// `like` on a blob column matches the text the grid shows (`\\xdeadbeef`).
#[tokio::test]
async fn sqlite_like_on_a_blob_matches_its_grid_text() {
    let (_dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    for filter in [r#"avatar like "%dead%""#, r#"avatar like "\xdead%""#] {
        let page = d.fetch_page(&page_req("users", Some(filter), None, 0, 100)).await.unwrap();
        assert_eq!(page.matches, Some(RowCount::Exact(1)), "{filter}");
        assert_eq!(page.rows[0][0].as_deref(), Some("4"));
        assert_eq!(page.rows[0][7].as_deref(), Some("\\xdeadbeef"));
    }
    // Text cells are matched as before.
    let page = d.fetch_page(&page_req("users", Some(r#"name like "%ann%""#), None, 0, 100)).await.unwrap();
    assert_eq!(page.matches, Some(RowCount::Exact(15)));
    d.close().await;
}

#[tokio::test]
async fn sqlite_loose_filter_compares_by_literal_type() {
    let (_dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let int = d.fetch_page(&page_req("loose", Some("k = 5"), None, 0, 100)).await.unwrap();
    assert_eq!(int.rows, vec![texts(&["5", "int five"])]);
    assert_eq!(int.cell_types, Some(vec!["it".to_string()]));
    assert_eq!(int.matches, Some(RowCount::Exact(1)));
    let text = d.fetch_page(&page_req("loose", Some(r#"k = "5""#), None, 0, 100)).await.unwrap();
    assert_eq!(text.rows, vec![texts(&["5", "text five"])]);
    assert_eq!(text.cell_types, Some(vec!["tt".to_string()]));
    assert_eq!(text.matches, Some(RowCount::Exact(1)));
    d.close().await;
}

#[tokio::test]
async fn sqlite_count_timeout_gives_unknown() {
    let (_dir, path) = sqlite_fixture();
    // SQLite counts `big` in well under 1 ms, so the test counts a view that never ends; its
    // first page is quick.
    setup(&path, "create view endless as with recursive c(x) as (select 1 union all select x + 1 from c) select x from c");
    let mut d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    d.set_count_timeout_ms(1);
    let started = std::time::Instant::now();
    let page = d.fetch_page(&page_req("endless", None, None, 0, 100)).await.unwrap();
    assert_eq!(page.total, RowCount::Unknown);
    assert_eq!(page.rows.len(), 100);
    let filtered = d.fetch_page(&page_req("endless", Some("x > 0"), None, 0, 100)).await.unwrap();
    assert_eq!((filtered.total, filtered.matches), (RowCount::Unknown, Some(RowCount::Unknown)));
    assert_eq!(filtered.rows.len(), 100);
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());

    let big = d.fetch_page(&page_req("big", None, None, 0, 100)).await.unwrap();
    assert_eq!(big.rows.len(), 100);

    // The abandoned counts were stopped: both connections count again.
    d.set_count_timeout_ms(3000);
    for _ in 0..d.workers().len() {
        let page = tokio::time::timeout(Duration::from_secs(2), d.fetch_page(&page_req("users", None, None, 0, 100)))
            .await
            .expect("not blocked by an abandoned count")
            .unwrap();
        assert_eq!(page.total, RowCount::Exact(17));
    }
    d.close().await;
}

#[tokio::test]
async fn sqlite_pages_overlap_on_the_two_connections() {
    let (_dir, path) = sqlite_fixture();
    setup(&path, "create view endless as with recursive c(x) as (select 1 union all select x + 1 from c) select x from c");
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    // A page whose count runs for the whole 3 s timeout, and a quick page started meanwhile.
    let endless = page_req("endless", None, None, 0, 10);
    let (slow, quick) = tokio::join!(d.fetch_page(&endless), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        let page = d.fetch_page(&page_req("users", None, None, 0, 100)).await.unwrap();
        (page, started.elapsed())
    });
    assert_eq!(slow.unwrap().total, RowCount::Unknown);
    assert_eq!(quick.0.total, RowCount::Exact(17));
    assert!(quick.1 < Duration::from_secs(1), "{:?}", quick.1);
    d.close().await;
}

#[tokio::test]
async fn sqlite_schema_columns() {
    let (_dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let cols = d.schema_columns(None, "main").await.unwrap();
    assert_eq!(cols["users"], ["id", "name", "email", "status", "plan", "created_at", "bio", "avatar"].map(String::from));
    assert_eq!(cols["Order Items"], ["id", "name"].map(String::from));
    assert_eq!(cols["active_users"].len(), 8);
    assert!(!cols.keys().any(|k| k.starts_with("sqlite_")));
    assert_eq!(cols.len(), 10);
    d.close().await;
}

#[tokio::test]
async fn sqlite_broken_view_is_not_blamed_on_the_filter() {
    let (_dir, path) = sqlite_fixture();
    setup(
        &path,
        "create table t (a, b);
         insert into t values (1, 'not json'), (2, '{}');
         -- Compiles (so its structure reads), but fails on its first row at run time.
         create view v as select a, json_extract(b, '$.x') as x from t;
         create table t2 (a, b);
         create view v2 as select a, b from t2;
         -- The table is replaced without b; the view then fails to compile.
         drop table t2;
         create table t2 (a);
         -- Its first row is fine, its second fails.
         create table t3 (a, b);
         insert into t3 values (1, '{}'), (2, 'not json');
         create view v3 as select a, json_extract(b, '$.x') as x from t3;",
    );
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();

    let unfiltered = d.fetch_page(&page_req("v", None, None, 0, 100)).await.unwrap_err();
    assert_eq!(unfiltered, AppError::Query("malformed JSON".into()));
    // The same failure with a filter is the view's, not the filter's.
    let filtered = d.fetch_page(&page_req("v", Some("a = 1"), None, 0, 100)).await.unwrap_err();
    assert_eq!(filtered, unfiltered);
    // A filter that keeps only the good row works.
    let good = d.fetch_page(&page_req("v", Some("a = 2"), None, 0, 100)).await.unwrap();
    assert_eq!((good.rows.len(), good.matches), (1, Some(RowCount::Exact(1))));
    // The page stops at its first row; the filtered count reads on into the broken one.
    let err = d.fetch_page(&page_req("v3", Some("x is null"), None, 0, 1)).await.unwrap_err();
    assert_eq!(err, AppError::Query("malformed JSON".into()));

    let err = d.fetch_page(&page_req("v2", Some("a = 1"), None, 0, 100)).await.unwrap_err();
    assert_eq!(err, AppError::Query("no such column: b".into()));

    // A filter that is itself the problem still is.
    let too_complex = format!("name like \"{}\"", "x".repeat(50_001));
    let err = d.fetch_page(&page_req("users", Some(&too_complex), None, 0, 100)).await.unwrap_err();
    assert_eq!(err, AppError::Filter { message: "LIKE or GLOB pattern too complex".into(), position: 0 });
    d.close().await;
}

/// Pages 0–2 (100 rows each) of `main.{name}`: their rows in page order, and each page's
/// `unstable_order`.
async fn three_pages(d: &dyn Driver, name: &str, sort: Option<(&str, bool)>) -> (Vec<Vec<Option<String>>>, Vec<bool>) {
    let mut rows = Vec::new();
    let mut unstable = Vec::new();
    for page in 0..3 {
        let p = d.fetch_page(&page_req(name, None, sort, page, 100)).await.unwrap();
        // Still read-only.
        assert!(matches!(p.editing, Editing::NoPrimaryKey | Editing::View), "{name}: {:?}", p.editing);
        rows.extend(p.rows);
        unstable.push(p.unstable_order);
    }
    (rows, unstable)
}

#[tokio::test]
async fn page_order_without_a_primary_key() {
    let (_dir, path) = sqlite_fixture();
    // The rowids run against a, so the rowid order is a descending.
    setup(
        &path,
        "CREATE TABLE nokey (a INTEGER, b TEXT);
         WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 250)
         INSERT INTO nokey (rowid, a, b) SELECT 251 - n, n, n % 2 FROM g;
         CREATE TABLE taken (Rowid INTEGER, _ROWID_ INTEGER, oid INTEGER, a INTEGER);
         INSERT INTO taken SELECT a, a, a, a FROM nokey;
         CREATE VIEW v AS SELECT * FROM nokey;",
    );
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let nokey = three_pages(&d, "nokey", None).await;
    let nokey_by_b = three_pages(&d, "nokey", Some(("b", true))).await;
    let taken = three_pages(&d, "taken", None).await;
    let view = three_pages(&d, "v", None).await;
    d.close().await;

    // By rowid, each row once.
    assert_eq!(nokey.0, direct_rows(&path, "select * from nokey order by rowid"));
    assert_eq!(nokey.0[0], texts(&["250", "0"]));
    assert_eq!(nokey.1, vec![false; 3]);
    // Rows equal on the sort follow the rowid, so no row repeats or goes missing across pages.
    assert_eq!(nokey_by_b.0, direct_rows(&path, "select * from nokey order by b desc nulls last, rowid"));
    assert_eq!(nokey_by_b.1, vec![false; 3]);
    // All three rowid names taken by columns, and a view: no fixed order, and the page says so.
    assert_eq!(taken.0.len(), 250);
    assert_eq!(taken.1, vec![true; 3]);
    assert_eq!(view.0.len(), 250);
    assert_eq!(view.1, vec![true; 3]);
}
