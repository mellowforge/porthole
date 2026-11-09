mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use porthole_lib::driver::mysql::kill::{bounded, kill_query, Bounded, KILL_CONNECT_WAIT};
use porthole_lib::driver::mysql::text::query_text;
use porthole_lib::driver::mysql::{connect_mysql, values, MysqlConfig};
use porthole_lib::driver::Driver;
use porthole_lib::error::AppError;
use porthole_lib::model::*;
use common::mysql::{for_each_mysql, mysql_driver, mysql_exec_script, mysql_profile, mysql_urls};
use common::net;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};

/// Runs for minutes (3.6 billion pairs). The `where` keeps MySQL 8.4 from answering a bare
/// `count(*)` of a cross join instantly from the two tables' row counts.
const HEAVY_COUNT: &str = "select count(*) from fx.big a, fx.big b where a.id + b.id > 0";

/// The server error number of a failed statement.
fn errno<T: std::fmt::Debug>(r: mysql_async::Result<T>) -> u16 {
    match r {
        Err(mysql_async::Error::Server(e)) => e.code,
        other => panic!("expected a server error, got {other:?}"),
    }
}

/// Every row of `sql` run through `query_text`, as the text `values::cell` gives each value.
async fn texts(conn: &mut Conn, sql: &str, vals: &[Option<String>]) -> Vec<Vec<Option<String>>> {
    let mut result = query_text(conn, sql, vals).await.unwrap();
    let rows: Vec<Row> = result.collect().await.unwrap();
    result.drop_result().await.unwrap();
    rows.iter()
        .map(|r| (0..r.len()).map(|i| values::cell(r.as_ref(i).unwrap(), &r.columns_ref()[i]).0).collect())
        .collect()
}

/// The session's `Ssl_cipher`: empty without TLS.
async fn ssl_cipher(conn: &mut Conn) -> String {
    let row: Option<(String, String)> = conn.query_first("SHOW SESSION STATUS LIKE 'Ssl_cipher'").await.unwrap();
    row.unwrap().1
}

#[tokio::test]
async fn mysql_connect_reports_server_version() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        assert_eq!(d.kind(), DbKind::Mysql);
        let v = d.server_version().await.unwrap();
        assert!(v.starts_with("MySQL 8.") || v.starts_with("MariaDB 11."), "{v}");
        if v.starts_with("MariaDB") {
            assert!(!v.contains('-'), "{v}");
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_connect_wrong_password_fails_inline() {
    for_each_mysql(|p, _pw| async move {
        match connect_mysql(&p, Some("wrong".into())).await {
            Err(AppError::Connection(m)) => assert!(m.contains("Access denied"), "{m}"),
            Err(e) => panic!("expected a connection error, got {e:?}"),
            Ok(_) => panic!("connected with a wrong password"),
        }
    })
    .await;
}

#[tokio::test]
async fn mysql_connect_timeout() {
    let mut p = mysql_profile(&mysql_urls()[0], "fx");
    net(&mut p).host = "10.255.255.1".into();
    p.connect_timeout_secs = 1;
    let started = Instant::now();
    let r = connect_mysql(&p, Some("test".into())).await;
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    match r {
        Err(AppError::Connection(m)) => assert!(m.contains("Timed out"), "{m}"),
        Err(e) => panic!("expected a connection error, got {e:?}"),
        Ok(_) => panic!("connected to an unroutable address"),
    }
}

#[tokio::test]
async fn mysql_caching_sha2_user_connects_without_tls() {
    for_each_mysql(|mut p, _pw| async move {
        net(&mut p).user = "ts_app".into();
        net(&mut p).ssl_mode = SslMode::Disable;
        let d = connect_mysql(&p, Some("apppw".into())).await.unwrap();
        let mut c = d.pool().get_conn().await.unwrap();
        let user: Option<String> = c.query_first("select current_user()").await.unwrap();
        assert_eq!(user.unwrap(), "ts_app@%");
        assert_eq!(ssl_cipher(&mut c).await, "");
        drop(c);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_tls_modes() {
    for_each_mysql(|p, pw| async move {
        for mode in [SslMode::Require, SslMode::Prefer] {
            let mut p = p.clone();
            net(&mut p).ssl_mode = mode.clone();
            let d = connect_mysql(&p, Some(pw.clone())).await.unwrap();
            let mut c = d.pool().get_conn().await.unwrap();
            assert_ne!(ssl_cipher(&mut c).await, "", "{mode:?}");
            drop(c);
            d.close().await;
        }
        let mut full = p.clone();
        net(&mut full).ssl_mode = SslMode::VerifyFull;
        match connect_mysql(&full, Some(pw.clone())).await {
            Err(AppError::Connection(m)) => assert!(m.to_lowercase().contains("certificate"), "{m}"),
            Err(e) => panic!("expected a connection error, got {e:?}"),
            Ok(_) => panic!("verify-full accepted a self-signed certificate"),
        }
    })
    .await;
}

/// The pool's reset after each use must not lose read-only mode or the startup SQL.
#[tokio::test]
async fn mysql_read_only_and_startup_sql_survive_pool_reuse() {
    for_each_mysql(|mut p, pw| async move {
        p.read_only = true;
        p.startup_sql = Some("SET @ts_marker = 1".into());
        let d = mysql_driver(&p, &pw).await;
        let mut ids = Vec::new();
        for i in 0..5 {
            let mut c = d.pool().get_conn().await.unwrap();
            ids.push(c.id());
            let marker: Option<i64> = c.query_first("select @ts_marker").await.unwrap();
            assert_eq!(marker, Some(1), "iteration {i}");
            assert_eq!(errno(c.query_drop("insert into fx.nopk values ('x','y')").await), 1792, "iteration {i}");
            drop(c);
            // Let the pool take the connection back (and reset it) before the next use.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let distinct: HashSet<_> = ids.iter().collect();
        assert!(distinct.len() < ids.len(), "no connection was reused: {ids:?}");
        d.close().await;
    })
    .await;
}

/// The startup statements run in order, and before read-only mode, so a startup `CREATE
/// TEMPORARY TABLE` (refused in a read-only transaction) works.
#[tokio::test]
async fn mysql_startup_sql_runs_in_order_before_read_only() {
    for_each_mysql(|mut p, pw| async move {
        p.read_only = true;
        p.startup_sql = Some("SET @a = 1; SET @b = @a + 1;\nCREATE TEMPORARY TABLE ts_tmp (x INT)".into());
        let d = mysql_driver(&p, &pw).await;
        for _ in 0..2 {
            let mut c = d.pool().get_conn().await.unwrap();
            let b: Option<i64> = c.query_first("select @b").await.unwrap();
            assert_eq!(b, Some(2));
            let n: Option<i64> = c.query_first("select count(*) from ts_tmp").await.unwrap();
            assert_eq!(n, Some(0));
            // (A temporary table stays writable in a read-only transaction; other tables don't.)
            assert_eq!(errno(c.query_drop("insert into fx.nopk values ('x','y')").await), 1792);
            drop(c);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_kill_stops_a_running_query() {
    for_each_mysql(|mut p, pw| async move {
        // Read-only, so the KILL also runs on a connection set up read-only.
        p.read_only = true;
        let d = mysql_driver(&p, &pw).await;
        let mut c = d.pool().get_conn().await.unwrap();
        let started = Instant::now();
        let r = bounded(d.config(), &mut c, Duration::from_millis(300), |c| {
            c.query_first::<i64, _>(HEAVY_COUNT)
        })
        .await;
        assert!(matches!(r, Err(Bounded::TimedOut)), "{r:?}");
        assert!(started.elapsed() < Duration::from_millis(2500), "{:?}", started.elapsed());
        let one: Option<i64> = c.query_first("select 1").await.unwrap();
        assert_eq!(one, Some(1));
        drop(c);
        d.close().await;
    })
    .await;
}

/// A statement that fails by itself before the limit is `Failed`, with its error, and nothing is killed.
#[tokio::test]
async fn mysql_bounded_reports_a_statement_error_as_failed() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let mut c = d.pool().get_conn().await.unwrap();
        let r = bounded(d.config(), &mut c, Duration::from_secs(5), |c| {
            c.query_first::<i64, _>("select count(*) from fx.no_such_table")
        })
        .await;
        match r {
            Err(Bounded::Failed(mysql_async::Error::Server(e))) => assert_eq!(e.code, 1146),
            other => panic!("expected Failed with 1146, got {other:?}"),
        }
        let one: Option<i64> = c.query_first("select 1").await.unwrap();
        assert_eq!(one, Some(1));
        drop(c);
        d.close().await;
    })
    .await;
}

/// When the KILL can't be sent (its connection can't be opened), `bounded` gives up within
/// `KILL_CONNECT_WAIT` and reports the statement as abandoned.
#[tokio::test]
async fn mysql_bounded_abandons_when_the_kill_cannot_connect() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        // Not from the pool: an abandoned connection must never go back to it.
        let mut c = d.config().open().await.unwrap();
        let id = c.id();
        let mut unreachable = p.clone();
        net(&mut unreachable).host = "10.255.255.1".into();
        unreachable.connect_timeout_secs = 30;
        let dead_end = MysqlConfig::from_profile(&unreachable, Some(pw.clone())).unwrap();
        let started = Instant::now();
        let r = bounded(&dead_end, &mut c, Duration::from_millis(300), |c| c.query_first::<i64, _>(HEAVY_COUNT)).await;
        assert!(matches!(r, Err(Bounded::Abandoned)), "{r:?}");
        let limit = Duration::from_millis(300) + KILL_CONNECT_WAIT + Duration::from_millis(1000);
        assert!(started.elapsed() < limit, "{:?}", started.elapsed());
        // Clean up: stop the abandoned statement on the server, and drop its connection.
        kill_query(d.config(), id).await.unwrap();
        drop(c);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn query_text_with_values_reads_text() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let mut c = d.pool().get_conn().await.unwrap();
        let expected = vec![vec![Some("1e300".to_string()), Some("1.23457".to_string())]];
        let bound = texts(&mut c, "select x, f from fx.typed where id = ?", &[Some("1".into())]).await;
        assert_eq!(bound, expected);
        let plain = texts(&mut c, "select x, f from fx.typed where id = 1", &[]).await;
        assert_eq!(plain, expected);
        drop(c);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn query_text_limit_binds_an_integer() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let mut c = d.pool().get_conn().await.unwrap();
        let rows = texts(&mut c, "select id from fx.big order by id limit ?", &[Some("3".into())]).await;
        assert_eq!(rows, vec![vec![Some("1".to_string())], vec![Some("2".to_string())], vec![Some("3".to_string())]]);
        drop(c);
        d.close().await;
    })
    .await;
}

// Browsing.

fn fx(name: &str) -> TableRef {
    TableRef { schema: "fx".into(), name: name.into() }
}

fn page_req(name: &str, filter: Option<&str>, sort: Option<(&str, bool)>) -> PageRequest {
    PageRequest {
        table: fx(name),
        filter: filter.map(String::from),
        sort: sort.map(|(c, d)| SortSpec { column: c.into(), descending: d }),
        page: 0,
        page_size: 100,
    }
}

fn column<'a>(s: &'a TableStructure, name: &str) -> &'a ColumnInfo {
    s.columns.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"))
}

/// The URL of the server a `for_each_mysql` profile points at.
fn mysql_urls_for(p: &ConnectionProfile, pw: &str) -> String {
    let n = p.network().unwrap();
    format!("mysql://{}:{pw}@{}:{}/app_test", n.user, n.host, n.port)
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn mysql_schemas_and_tree() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let schemas = d.list_schemas(None).await.unwrap();
        assert!(schemas.contains(&"fx".to_string()) && schemas.contains(&"fx_other".to_string()), "{schemas:?}");
        for system in ["information_schema", "performance_schema", "mysql", "sys"] {
            assert!(!schemas.contains(&system.to_string()), "{schemas:?}");
        }
        let mut sorted = schemas.clone();
        sorted.sort();
        assert_eq!(schemas, sorted);
        assert_eq!(d.list_databases().await.unwrap(), Vec::<String>::new());

        let tree = d.schema_tree("fx").await.unwrap();
        assert_eq!(
            tree.tables,
            strings(&["Order Items", "big", "bigkey", "binkey", "floatkey", "gen", "latin", "nopk", "orders", "typed", "users"])
        );
        assert_eq!(tree.views, strings(&["active_users"]));
        assert_eq!(
            tree.functions,
            vec![
                FunctionInfo { name: "calc_tax".into(), signature: "amount decimal(10,2)".into(), kind: Some(RoutineKind::Function) },
                FunctionInfo { name: "two_sets".into(), signature: "IN n int, OUT total int".into(), kind: Some(RoutineKind::Procedure) },
            ]
        );
        let other = d.schema_tree("fx_other").await.unwrap();
        assert_eq!((other.tables, other.views, other.functions.len()), (strings(&["other_t"]), vec![], 0));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_structure() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let users = d.table_structure(&fx("users")).await.unwrap();
        let names: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "name", "email", "status", "plan", "created_at", "bio", "avatar"]);
        let id = column(&users, "id");
        assert_eq!(id.data_type, "int");
        assert_eq!(id.cast_type, "int");
        assert_eq!(id.default.as_deref(), Some("auto_increment"));
        assert!(id.is_primary_key && !id.read_only && !id.nullable);
        assert_eq!(column(&users, "name").full_type, "varchar(100)");
        assert!(column(&users, "email").nullable);
        assert_eq!(column(&users, "email").default, None);
        assert_eq!(users.primary_key, ["id"]);
        assert_eq!((users.relation, users.editing), (RelationKind::Table, Editing::Allowed));

        let generated = d.table_structure(&fx("gen")).await.unwrap();
        assert!(column(&generated, "b").read_only && column(&generated, "c").read_only);
        assert!(!column(&generated, "ts").read_only && !column(&generated, "a").read_only);
        assert_eq!(column(&generated, "a").default.as_deref(), Some("3"));
        // MariaDB reports a generated column's missing default as the text 'NULL'.
        assert_eq!(column(&generated, "b").default, None);

        assert_eq!(d.table_structure(&fx("nopk")).await.unwrap().editing, Editing::NoPrimaryKey);
        assert_eq!(d.table_structure(&fx("floatkey")).await.unwrap().editing, Editing::FloatKey);
        let view = d.table_structure(&fx("active_users")).await.unwrap();
        assert_eq!((view.relation, view.editing), (RelationKind::View, Editing::View));
        assert_eq!(view.columns.len(), 8);

        let orders = d.table_structure(&fx("orders")).await.unwrap();
        assert_eq!(
            orders.indexes,
            vec![
                IndexInfo { name: "PRIMARY".into(), columns: strings(&["id"]), included: Vec::new(), unique: true, primary: true },
                IndexInfo { name: "orders_user".into(), columns: strings(&["user_id"]), included: Vec::new(), unique: false, primary: false },
            ]
        );
        assert_eq!(
            orders.foreign_keys,
            vec![ForeignKeyInfo {
                name: "orders_user_fk".into(),
                columns: strings(&["user_id"]),
                ref_table: fx("users"),
                ref_columns: strings(&["id"]),
            }]
        );
        match d.table_structure(&fx("no_such_table")).await {
            Err(AppError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_page_filter_sort() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("users", None, None)).await.unwrap();
        assert_eq!(page.rows.len(), 17);
        assert_eq!(page.total, RowCount::Exact(17));
        assert_eq!(page.matches, None);
        assert_eq!(page.primary_key, ["id"]);
        assert_eq!(page.rows[0][0].as_deref(), Some("1"));
        let types = page.cell_types.clone().unwrap();
        assert_eq!(types.len(), 17);
        assert!(types.iter().all(|t| t.len() == 8), "{types:?}");
        // id 4: integer, text, …, NULL bio, and its avatar bytes.
        assert_eq!(types[3], "itttttnb");
        assert_eq!(page.rows[3][7].as_deref(), Some("\\xdeadbeef"));

        let mut c = d.pool().get_conn().await.unwrap();
        let want: Option<u64> = c
            .query_first("select count(*) from fx.users where status = 'active' and name like '%ann%'")
            .await
            .unwrap();
        let want = want.unwrap();
        eprintln!("users filter count on {}: {want}", d.server_version().await.unwrap());
        let filtered = d.fetch_page(&page_req("users", Some(r#"status = "active" and name like "%ann%""#), None)).await.unwrap();
        assert_eq!(filtered.rows.len() as u64, want);
        assert_eq!(filtered.matches, Some(RowCount::Exact(want)));
        assert_eq!(filtered.total, RowCount::Exact(17));

        let plan = page.columns.iter().position(|c| c.name == "plan").unwrap();
        for (desc, dir) in [(false, "ASC"), (true, "DESC")] {
            let sorted = d.fetch_page(&page_req("users", None, Some(("plan", desc)))).await.unwrap();
            let plans: Vec<Option<String>> = sorted.rows.iter().map(|r| r[plan].clone()).collect();
            let first_null = plans.iter().position(Option::is_none).unwrap();
            assert!(plans[first_null..].iter().all(Option::is_none), "NULLs last ({dir}): {plans:?}");
            let first: Option<String> = c
                .query_first(format!("select id from fx.users order by plan is null, plan {dir}, id limit 1"))
                .await
                .unwrap();
            assert_eq!(sorted.rows[0][0], first, "{dir}");
            let second: Vec<String> = c
                .query(format!("select cast(id as char) from fx.users order by plan is null, plan {dir}, id"))
                .await
                .unwrap();
            let ids: Vec<String> = sorted.rows.iter().map(|r| r[0].clone().unwrap()).collect();
            assert_eq!(ids, second, "{dir}");
        }
        drop(c);

        // A second page, and an unknown sort column.
        let mut second = page_req("users", None, None);
        (second.page, second.page_size) = (1, 5);
        let rows: Vec<Option<String>> = d.fetch_page(&second).await.unwrap().rows.iter().map(|r| r[0].clone()).collect();
        assert_eq!(rows, ["6", "7", "8", "9", "10"].map(|s| Some(s.to_string())));
        assert!(d.fetch_page(&page_req("users", None, Some(("nope", false)))).await.is_err());
        d.close().await;
    })
    .await;
}

/// A key above 2^53 is compared as an integer, not as a double.
#[tokio::test]
async fn mysql_filter_by_big_integer_key() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("bigkey", Some("id = 9007199254740993"), None)).await.unwrap();
        assert_eq!(page.rows, vec![vec![Some("9007199254740993".to_string()), Some("b".to_string())]]);
        assert_eq!(page.matches, Some(RowCount::Exact(1)));
        d.close().await;
    })
    .await;
}

/// A number compared with a text column compares as text, so `k = 5` matches `5` only (not
/// `05`), on the page and in its count.
#[tokio::test]
async fn mysql_filter_on_a_text_key_compares_text() {
    for_each_mysql(|p, pw| async move {
        common::mysql::mysql_create_skus(&p, &pw).await;
        let d = mysql_driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("skus", Some("k = 5"), None)).await.unwrap();
        assert_eq!(page.rows, vec![vec![Some("5".to_string()), Some("five".to_string())]]);
        assert_eq!(page.matches, Some(RowCount::Exact(1)));
        let page = d.fetch_page(&page_req("skus", Some("k in (5, 6)"), None)).await.unwrap();
        assert_eq!(page.rows.len(), 1);
        // The table export runs the same filter.
        let mut sink = Collect::default();
        d.export_table(&TableQuery { table: fx("skus"), filter: Some("k = 5".into()), sort: None }, &mut sink).await.unwrap();
        assert_eq!(sink.0, vec![vec![Some("5".to_string()), Some("five".to_string())]]);
        d.close().await;
    })
    .await;
}

/// Collects exported rows.
#[derive(Default)]
struct Collect(Vec<Vec<Option<String>>>);

impl porthole_lib::export::RowSink for Collect {
    fn begin(&mut self, _: &[porthole_lib::export::ExportColumn]) -> porthole_lib::error::AppResult<()> {
        Ok(())
    }
    fn row(&mut self, row: Vec<Option<String>>) -> porthole_lib::error::AppResult<()> {
        self.0.push(row);
        Ok(())
    }
}

#[tokio::test]
async fn mysql_filter_on_latin1_column() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("latin", Some(r#"s = "café""#), None)).await.unwrap();
        assert_eq!(page.rows, vec![vec![Some("1".to_string()), Some("café".to_string())]]);
        assert_eq!(page.matches, Some(RowCount::Exact(1)));
        let like = d.fetch_page(&page_req("latin", Some(r#"s like "caf%""#), None)).await.unwrap();
        assert_eq!(like.rows.len(), 1);
        d.close().await;
    })
    .await;
}

/// The statements counting `fx.heavy` that are still running on the server, other than this
/// connection's own, once there are none or after `wait`.
async fn heavy_counts_after(c: &mut Conn, wait: Duration) -> u64 {
    let deadline = Instant::now() + wait;
    loop {
        let running: Option<u64> = c
            .query_first(
                "select count(*) from information_schema.PROCESSLIST \
                 where ID <> CONNECTION_ID() and INFO like 'SELECT count(*) FROM `fx`.`heavy`%'",
            )
            .await
            .unwrap();
        let running = running.unwrap();
        if running == 0 || Instant::now() >= deadline {
            return running;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn mysql_count_timeout_gives_unknown() {
    for_each_mysql(|p, pw| async move {
        // `big` counts in a few ms, before a KILL could reach it; a view over the heavy cross join
        // counts for minutes, while its first page comes at once.
        common::mysql::mysql_exec(
            &mysql_urls_for(&p, &pw),
            "create view fx.heavy as select a.id, a.label from fx.big a, fx.big b where a.id + b.id > 0",
        )
        .await;
        let mut d = mysql_driver(&p, &pw).await;
        d.set_count_timeout_ms(1);
        let started = Instant::now();
        let page = d.fetch_page(&page_req("heavy", Some("id > 0"), None)).await.unwrap();
        assert_eq!(page.matches, Some(RowCount::Unknown));
        assert_eq!(page.total, RowCount::Unknown);
        assert_eq!(page.rows.len(), 100);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        // The counts were stopped by KILL QUERY, not abandoned: an abandoned count would go on
        // running on the server for minutes after its connection closed.
        let mut c = d.config().open().await.unwrap();
        assert_eq!(heavy_counts_after(&mut c, Duration::from_secs(1)).await, 0, "a count of fx.heavy is still running");
        c.disconnect().await.ok();
        // The killed counts left their connections usable.
        d.set_count_timeout_ms(3000);
        for _ in 0..4 {
            let users = d.fetch_page(&page_req("users", Some("id > 0"), None)).await.unwrap();
            assert_eq!((users.total, users.matches), (RowCount::Exact(17), Some(RowCount::Exact(17))));
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_big_total() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("big", None, None)).await.unwrap();
        assert!(
            matches!(page.total, RowCount::Exact(60000)) || matches!(page.total, RowCount::Estimated(n) if n > 0),
            "{:?}",
            page.total
        );
        assert_eq!(page.rows.len(), 100);
        assert_eq!(page.rows[0], vec![Some("1".to_string()), Some("v1".to_string())]);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_filter_error_is_inline() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        match d.fetch_page(&page_req("users", Some("nope = 1"), None)).await {
            Err(AppError::Filter { .. }) => {}
            other => panic!("expected a filter error, got {other:?}"),
        }
        d.close().await;
    })
    .await;
}

/// A value the column's type can't take is a warning on MySQL (the comparison goes on with
/// what the server made of it, matching the wrong rows: MariaDB returns rows for
/// `created_at > 'not a date'`); the filter shows it as its inline error instead.
#[tokio::test]
async fn mysql_filter_conversion_warnings_are_filter_errors() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let filter_error = |filter: &'static str, needle: &'static str| {
            let d = &d;
            async move {
                match d.fetch_page(&page_req("users", Some(filter), None)).await {
                    Err(AppError::Filter { message, .. }) => assert!(message.contains(needle), "{filter}: {message}"),
                    other => panic!("{filter}: expected a filter error, got {other:?}"),
                }
            }
        };
        filter_error(r#"created_at > "not a date""#, "not a date").await;
        if d.server_version().await.unwrap().starts_with("MariaDB") {
            // MariaDB 11.4 compares an INT with 'abc' as 0 without any warning: there is nothing
            // to report, and no row has id 0.
            let page = d.fetch_page(&page_req("users", Some(r#"id = "abc""#), None)).await.unwrap();
            assert_eq!(page.matches, Some(RowCount::Exact(0)));
        } else {
            filter_error(r#"id = "abc""#, "abc").await;
        }
        // Values the column takes still filter as before, and so does the unfiltered page.
        assert_eq!(d.fetch_page(&page_req("users", Some(r#"id = "3""#), None)).await.unwrap().matches, Some(RowCount::Exact(1)));
        assert!(d.fetch_page(&page_req("users", None, None)).await.is_ok());
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_schema_columns() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let cols = d.schema_columns(None, "fx").await.unwrap();
        assert_eq!(cols["users"], strings(&["id", "name", "email", "status", "plan", "created_at", "bio", "avatar"]));
        assert_eq!(cols["Order Items"], strings(&["id", "name"]));
        assert!(cols.contains_key("active_users"));
        assert!(!cols.contains_key("other_t"));
        d.close().await;
    })
    .await;
}

/// The window opens on the database the connection is on, never on `public` (a MySQL
/// server may well have a database by that name). Database names are case-sensitive on the
/// Linux test servers (`lower_case_table_names = 0`), so a profile naming `FX` can't connect
/// there; the probe's `DATABASE()` is tested through startup SQL that chooses another database.
#[tokio::test]
async fn mysql_initial_database_is_the_connections_never_public() {
    for_each_mysql(|mut p, pw| async move {
        let url = mysql_urls_for(&p, &pw);
        common::mysql::mysql_exec(&url, "DROP DATABASE IF EXISTS public; CREATE DATABASE public").await;
        // No profile database: the first listed.
        net(&mut p).database = String::new();
        let d = porthole_lib::driver::connect(&p, Some(pw.clone())).await.unwrap();
        let info = porthole_lib::commands::describe(&p, d.as_ref()).await.unwrap();
        assert!(info.schemas.contains(&"public".to_string()), "{:?}", info.schemas);
        assert_eq!(info.schema, info.schemas[0]);
        assert_ne!(info.schema, "public");
        d.close().await;
        // The profile's database, as the connection reports it after the startup SQL.
        net(&mut p).database = "fx".into();
        p.startup_sql = Some("USE fx_other".into());
        let d = porthole_lib::driver::connect(&p, Some(pw.clone())).await.unwrap();
        assert_eq!(porthole_lib::commands::describe(&p, d.as_ref()).await.unwrap().schema, "fx_other");
        d.close().await;
        common::mysql::mysql_exec(&url, "DROP DATABASE public").await;
    })
    .await;
}

#[tokio::test]
async fn connect_picks_the_profile_database() {
    for_each_mysql(|mut p, pw| async move {
        net(&mut p).database = "fx_other".into();
        let d = porthole_lib::driver::connect(&p, Some(pw.clone())).await.unwrap();
        let info = porthole_lib::commands::describe(&p, d.as_ref()).await.unwrap();
        assert_eq!(info.schema, "fx_other");
        assert!(info.schemas.contains(&"fx".to_string()));
        // A default schema still comes first.
        p.default_schema = Some("fx".into());
        assert_eq!(porthole_lib::commands::describe(&p, d.as_ref()).await.unwrap().schema, "fx");
        d.close().await;
    })
    .await;
}

/// Pages 0–2 (100 rows each) of `ts_scratch.{name}`: their rows in page order, and each page's
/// `unstable_order`.
async fn three_pages(d: &dyn Driver, name: &str, sort: Option<(&str, bool)>) -> (Vec<Vec<Option<String>>>, Vec<bool>) {
    let mut rows = Vec::new();
    let mut unstable = Vec::new();
    for page in 0..3 {
        let req = PageRequest {
            table: TableRef { schema: "ts_scratch".into(), name: name.into() },
            filter: None,
            sort: sort.map(|(c, d)| SortSpec { column: c.into(), descending: d }),
            page,
            page_size: 100,
        };
        let p = d.fetch_page(&req).await.unwrap();
        // Still read-only.
        assert!(matches!(p.editing, Editing::NoPrimaryKey | Editing::View), "{name}: {:?}", p.editing);
        rows.extend(p.rows);
        unstable.push(p.unstable_order);
    }
    (rows, unstable)
}

#[tokio::test]
async fn page_order_without_a_primary_key() {
    for_each_mysql(|p, pw| async move {
        let url = mysql_urls_for(&p, &pw);
        // a goes up, b down: InnoDB clusters `uniq` by `two` (a, b), the first unique NOT NULL
        // index, while the page orders by `one` (b), the one with fewer columns.
        let values: Vec<String> = (1..=250).map(|a| format!("({a}, {}, '{}')", 251 - a, a % 2)).collect();
        let setup = format!(
            "DROP DATABASE IF EXISTS ts_scratch;
             CREATE DATABASE ts_scratch;
             CREATE TABLE ts_scratch.uniq (a INT NOT NULL, b INT NOT NULL, c VARCHAR(1), UNIQUE KEY two (a, b), UNIQUE KEY one (b));
             INSERT INTO ts_scratch.uniq VALUES {};
             CREATE TABLE ts_scratch.loose (a INT NOT NULL, c VARCHAR(1), KEY (a));
             INSERT INTO ts_scratch.loose SELECT a, c FROM ts_scratch.uniq;
             CREATE TABLE ts_scratch.uniq_null (a INT, UNIQUE KEY (a));
             INSERT INTO ts_scratch.uniq_null SELECT a FROM ts_scratch.uniq;
             CREATE VIEW ts_scratch.v AS SELECT * FROM ts_scratch.uniq;",
            values.join(", ")
        );
        mysql_exec_script(&url, &setup).await;
        let d = mysql_driver(&p, &pw).await;
        let uniq = three_pages(&d, "uniq", None).await;
        let uniq_by_c = three_pages(&d, "uniq", Some(("c", true))).await;
        let loose = three_pages(&d, "loose", None).await;
        let uniq_null = three_pages(&d, "uniq_null", None).await;
        let view = three_pages(&d, "v", None).await;
        d.close().await;
        mysql_exec_script(&url, "DROP DATABASE IF EXISTS ts_scratch").await;

        let a = |rows: &[Vec<Option<String>>]| -> Vec<i32> {
            rows.iter().map(|r| r[0].as_deref().unwrap().parse().unwrap()).collect()
        };
        let down: Vec<i32> = (1..=250).rev().collect();
        // By b, the unique NOT NULL index with the fewest columns, each row once.
        assert_eq!(a(&uniq.0), down);
        assert_eq!(uniq.1, vec![false; 3]);
        // Rows equal on the sort follow b, so no row repeats or goes missing across pages.
        let odds_then_evens: Vec<i32> = down.iter().filter(|a| *a % 2 == 1).chain(down.iter().filter(|a| *a % 2 == 0)).copied().collect();
        assert_eq!(a(&uniq_by_c.0), odds_then_evens);
        assert_eq!(uniq_by_c.1, vec![false; 3]);
        // No unique index, a nullable one, and a view: no fixed order, and the page says so.
        assert_eq!(loose.1, vec![true; 3]);
        assert_eq!(uniq_null.1, vec![true; 3]);
        assert_eq!(view.0.len(), 250);
        assert_eq!(view.1, vec![true; 3]);
    })
    .await;
}
