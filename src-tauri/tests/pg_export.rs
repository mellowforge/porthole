mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use porthole_lib::error::AppError;
use porthole_lib::driver::Driver;
use porthole_lib::export::run::{run_query_export, run_rows_export, run_table_export, stream_query, ExportOutcome};
use porthole_lib::export::ExportFormat;
use porthole_lib::model::*;
use porthole_lib::query::{QuerySession, RawOutcome, ResultColumn};
use porthole_lib::sqltext::SqlDialect;
use common::{driver, exec, for_each_pg, url};
use sqlx::{AssertSqlSafe, Connection, PgConnection, Row};

fn tq(name: &str, filter: Option<&str>, sort: Option<SortSpec>) -> TableQuery {
    TableQuery {
        table: TableRef { schema: "fx".into(), name: name.into() },
        filter: filter.map(str::to_string),
        sort,
    }
}

fn no_part_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .unwrap()
        .all(|e| !e.unwrap().file_name().to_string_lossy().contains(".part"))
}

fn csv_field(v: Option<String>) -> String {
    match v {
        None => String::new(),
        Some(s) if s.is_empty() => "\"\"".into(),
        Some(s) if s.contains([',', '"', '\r', '\n']) || s.starts_with(' ') || s.ends_with(' ') => {
            format!("\"{}\"", s.replace('"', "\"\""))
        }
        Some(s) => s,
    }
}

#[tokio::test]
async fn export_table_respects_filter_and_sort() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("users.csv");
        let cancel = AtomicBool::new(false);
        let q = tq("users", Some("status = 'active'"), Some(SortSpec { column: "name".into(), descending: true }));
        let out = run_table_export(&d, &q, ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        let cols = ["id", "name", "email", "status", "plan", "created_at", "bio", "data", "tags", "avatar"];
        let select = cols.iter().map(|c| format!("{c}::text")).collect::<Vec<_>>().join(", ");
        let mut conn = PgConnection::connect(&url(&p, &pw)).await.unwrap();
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {select} FROM fx.users WHERE status = 'active' ORDER BY name DESC NULLS LAST, id"
        )))
        .fetch_all(&mut conn)
        .await
        .unwrap();
        let mut expected = format!("{}\r\n", cols.join(","));
        for r in &rows {
            let line: Vec<String> =
                (0..cols.len()).map(|i| csv_field(r.get::<Option<String>, _>(i))).collect();
            expected.push_str(&line.join(","));
            expected.push_str("\r\n");
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
        match out {
            ExportOutcome::Done { rows: n, file_name, .. } => {
                assert_eq!(n as usize, rows.len());
                assert_eq!(file_name, "users.csv");
            }
            other => panic!("expected done, got {other:?}"),
        }
        assert!(no_part_files(dir.path()));
    })
    .await;
}

#[tokio::test]
async fn export_table_round_trips_fx_typed() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("typed.json");
        let cancel = AtomicBool::new(false);
        run_table_export(&d, &tq("typed", None, None), ExportFormat::Json, &target, &cancel, &|_| {})
            .await
            .unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let row = &v[0];
        assert_eq!(row["id"], serde_json::json!(1));
        assert!(text.contains("\"n\":1.50"), "raw numeric text kept: {text}");
        assert_eq!(row["j"], serde_json::json!({"a": 1}));
        assert_eq!(row["flag"], serde_json::json!(true));
        assert_eq!(row["b"], serde_json::json!("\\xdead"));
        assert_eq!(row["arr"], serde_json::json!("{1,2}"));
        assert_eq!(row["c"], serde_json::json!("abc       "));
    })
    .await;
}

#[tokio::test]
async fn export_rows_suffixes_duplicate_names() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("r.csv");
    let cancel = AtomicBool::new(false);
    let cols = vec![
        ResultColumn { name: "id".into(), data_type: "int4".into() },
        ResultColumn { name: "id".into(), data_type: "int4".into() },
    ];
    let rows = vec![vec![Some("1".into()), None]];
    let out = run_rows_export(&cols, &rows, None, ExportFormat::Csv, &target, &cancel, &|_| {}).unwrap();
    assert!(matches!(out, ExportOutcome::Done { rows: 1, .. }));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "id,id_2\r\n1,\r\n");
}

#[tokio::test]
async fn export_cancel_leaves_no_file_and_keeps_existing() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("users.csv");
        std::fs::write(&target, "old").unwrap();
        let cancel = AtomicBool::new(true);
        let out = run_table_export(&d, &tq("users", None, None), ExportFormat::Csv, &target, &cancel, &|_| {})
            .await
            .unwrap();
        assert!(matches!(out, ExportOutcome::Canceled));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(no_part_files(dir.path()));

        // A cancel raised mid-stream, via progress on a bigger table.
        let big = dir.path().join("big.csv");
        let cancel = AtomicBool::new(false);
        let out = run_table_export(&d, &tq("big", None, None), ExportFormat::Csv, &big, &cancel, &|n| {
            if n >= 1000 {
                cancel.store(true, Ordering::SeqCst);
            }
        })
        .await
        .unwrap();
        assert!(matches!(out, ExportOutcome::Canceled));
        assert!(!big.exists());
        assert!(no_part_files(dir.path()));
    })
    .await;
}

#[tokio::test]
async fn export_stream_error_aborts() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.csv");
        let cancel = AtomicBool::new(false);
        let q = tq("users", Some("nope = 1"), None);
        let out = run_table_export(&d, &q, ExportFormat::Csv, &target, &cancel, &|_| {}).await;
        assert!(matches!(out, Err(AppError::Filter { .. }) | Err(AppError::Query(_))), "{out:?}");
        assert!(!target.exists());
        assert!(no_part_files(dir.path()));
        exec(&url(&p, &pw), "SELECT 1").await;
    })
    .await;
}

async fn four_concurrent_pages_work(d: &porthole_lib::driver::postgres::PostgresDriver) {
    use porthole_lib::driver::Driver;
    let req = || PageRequest {
        table: TableRef { schema: "fx".into(), name: "users".into() },
        filter: None,
        sort: None,
        page: 0,
        page_size: 10,
    };
    let (a, b, c, e) = (req(), req(), req(), req());
    let results = futures_util::future::join_all([
        d.fetch_page(&a),
        d.fetch_page(&b),
        d.fetch_page(&c),
        d.fetch_page(&e),
    ])
    .await;
    for r in results {
        r.expect("fetch_page after an abandoned export");
    }
}

#[tokio::test]
async fn export_cancel_stops_the_server_query_and_frees_the_connection() {
    for_each_pg(|p, pw| async move {
        let u = url(&p, &pw);
        // A view whose rows take 1 ms each to produce: draining it would take minutes.
        exec(&u, "CREATE VIEW fx.huge AS SELECT g AS id, pg_sleep(0.001)::text AS slept FROM generate_series(1, 1000000) g").await;
        let d = driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("huge.csv");
        let cancel = AtomicBool::new(false);
        let out = run_table_export(&d, &tq("huge", None, None), ExportFormat::Csv, &target, &cancel, &|_| {
            cancel.store(true, Ordering::SeqCst);
        })
        .await
        .unwrap();
        assert!(matches!(out, ExportOutcome::Canceled), "{out:?}");
        assert!(!target.exists());
        assert!(no_part_files(dir.path()));

        let mut conn = PgConnection::connect(&u).await.unwrap();
        let mut active = 1;
        for _ in 0..20 {
            active = sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND state = 'active' AND query LIKE '%\"huge\"%'",
            )
            .fetch_one(&mut conn)
            .await
            .unwrap();
            if active == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(active, 0, "the export query is still running on the server");
        four_concurrent_pages_work(&d).await;
    })
    .await;
}

struct FailingSink {
    target: std::path::PathBuf,
    writer: Option<porthole_lib::export::ExportWriter>,
    seen: u64,
}

impl porthole_lib::export::RowSink for FailingSink {
    fn begin(&mut self, columns: &[porthole_lib::export::ExportColumn]) -> Result<(), AppError> {
        self.writer = Some(porthole_lib::export::ExportWriter::create(&self.target, ExportFormat::Csv, columns)?);
        Ok(())
    }
    fn row(&mut self, row: Vec<Option<String>>) -> Result<(), AppError> {
        self.seen += 1;
        if self.seen > 500 {
            return Err(AppError::Query("disk full".into()));
        }
        self.writer.as_mut().unwrap().write_row(&row)
    }
}

#[tokio::test]
async fn export_sink_error_mid_stream_aborts_and_keeps_the_pool_healthy() {
    use porthole_lib::driver::Driver;
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("big.csv");
        std::fs::write(&target, "old").unwrap();
        let mut sink = FailingSink { target: target.clone(), writer: None, seen: 0 };
        let err = d.export_table(&tq("big", None, None), &mut sink).await.unwrap_err();
        assert_eq!(err, AppError::Query("disk full".into()));
        assert!(sink.seen > 500, "rows had started");
        drop(sink);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(no_part_files(dir.path()));
        four_concurrent_pages_work(&d).await;
    })
    .await;
}

/// `SELECT d::text, r::text` from `fx.floats` by id on a plain connection with `extra_float_digits` set.
async fn float_texts(u: &str, digits: i32) -> Vec<(Option<String>, Option<String>)> {
    let mut conn = PgConnection::connect(u).await.unwrap();
    sqlx::query("SELECT set_config('extra_float_digits', $1, false)")
        .bind(digits.to_string())
        .execute(&mut conn)
        .await
        .unwrap();
    let rows = sqlx::query("SELECT d::text, r::text FROM fx.floats ORDER BY id").fetch_all(&mut conn).await.unwrap();
    rows.iter().map(|r| (r.get(0), r.get(1))).collect()
}

#[tokio::test]
async fn export_round_trips_floats_exactly_and_leaves_the_grid_alone() {
    use porthole_lib::driver::Driver;
    for_each_pg(|p, pw| async move {
        let u = url(&p, &pw);
        exec(
            &u,
            "CREATE TABLE fx.floats (id int PRIMARY KEY, d float8, r float4);
             INSERT INTO fx.floats VALUES
               (1, 0.1::float8 + 0.2::float8, 0.1),
               (2, 1 / 3.0::float8, 1 / 3.0::float4),
               (3, 'NaN', 'NaN'),
               (4, 'Infinity', '-Infinity');",
        )
        .await;
        let exact = float_texts(&u, 3).await;
        let d = driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("floats.csv");
        let cancel = AtomicBool::new(false);
        run_table_export(&d, &tq("floats", None, None), ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        let mut expected = String::from("id,d,r\r\n");
        for (i, (dv, rv)) in exact.iter().enumerate() {
            expected.push_str(&format!("{},{},{}\r\n", i + 1, dv.as_deref().unwrap(), rv.as_deref().unwrap()));
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
        // Spot check of the text itself (PG 11 prints 18 digits at this setting, PG 12+ the shortest exact).
        let third = &exact[1].0.as_deref().unwrap()[..17];
        assert_eq!(third, "0.333333333333333", "{exact:?}");
        assert!(exact[0].0.as_deref().unwrap().starts_with("0.3000000000000000"), "{exact:?}");

        // The grid still shows what it showed before: every pooled connection (the export's one
        // went back to the pool) keeps the driver's own setting.
        let grid = float_texts(&u, 2).await;
        let req = || PageRequest {
            table: TableRef { schema: "fx".into(), name: "floats".into() },
            filter: None,
            sort: None,
            page: 0,
            page_size: 10,
        };
        let (a, b, c, e) = (req(), req(), req(), req());
        let pages = futures_util::future::join_all([d.fetch_page(&a), d.fetch_page(&b), d.fetch_page(&c), d.fetch_page(&e)]).await;
        for page in pages {
            let page = page.unwrap();
            let shown: Vec<(Option<String>, Option<String>)> =
                page.rows.iter().map(|r| (r[1].clone(), r[2].clone())).collect();
            assert_eq!(shown, grid);
        }
    })
    .await;
}

#[tokio::test]
async fn export_csv_end_to_end_null_empty_emoji_and_quoting() {
    for_each_pg(|p, pw| async move {
        let u = url(&p, &pw);
        exec(
            &u,
            "CREATE TABLE fx.csv_edge (id int PRIMARY KEY, t text);
             INSERT INTO fx.csv_edge VALUES (1, NULL), (2, ''), (3, '🐘'), (4, E'a,\"b\"\\nc');",
        )
        .await;
        let d = driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("edge.csv");
        let cancel = AtomicBool::new(false);
        run_table_export(&d, &tq("csv_edge", None, None), ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        let expected: &[u8] = b"id,t\r\n1,\r\n2,\"\"\r\n3,\xF0\x9F\x90\x98\r\n4,\"a,\"\"b\"\"\nc\"\r\n";
        assert_eq!(std::fs::read(&target).unwrap(), expected);
    })
    .await;
}

// ---- Query results: the statement runs again on the tab's session ----

async fn query_session(d: &porthole_lib::driver::postgres::PostgresDriver) -> std::sync::Arc<dyn QuerySession> {
    d.open_query_session(None).await.unwrap()
}

fn session_rows(run: &porthole_lib::query::StatementRun) -> Vec<Vec<Option<String>>> {
    match &run.outcome {
        RawOutcome::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[tokio::test]
async fn export_query_writes_every_row_past_the_grid_caps() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("series.csv");
        let cancel = AtomicBool::new(false);
        let sql = "select g from generate_series(1, 250000) g";
        let out = run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 250_000, .. }), "{out:?}");
        let text = std::fs::read_to_string(&target).unwrap();
        let lines: Vec<&str> = text.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 250_001);
        assert_eq!((lines[0], lines[1], lines[250_000]), ("g", "1", "250000"));
        assert!(no_part_files(dir.path()));
        s.close().await;
    })
    .await;
}

#[tokio::test]
async fn export_query_values_match_the_grid_and_params_bind() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        // The grid's own rows for the same statement, then the export's.
        let sql = "select * from fx.typed order by 1";
        let grid = session_rows(&s.run(sql, &[], 1000).await);
        let target = dir.path().join("typed.csv");
        run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        let mut expected = String::new();
        for r in &grid {
            expected.push_str(&r.iter().cloned().map(csv_field).collect::<Vec<_>>().join(","));
            expected.push_str("\r\n");
        }
        let body = text.split_once("\r\n").unwrap().1;
        assert_eq!(body, expected);

        let target = dir.path().join("params.csv");
        let sql = "select g from generate_series(1, $1) g where g > $2";
        let params = [Some("5".to_string()), Some("2".to_string())];
        let out = run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &params, ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 3, .. }), "{out:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "g\r\n3\r\n4\r\n5\r\n");
        s.close().await;
    })
    .await;
}

#[tokio::test]
async fn export_query_refuses_a_statement_that_writes() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("w.csv");
        let cancel = AtomicBool::new(false);
        s.run("create temp table export_w(x int)", &[], 1).await;
        let sql = "insert into export_w values (1) returning x";
        let out = run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await;
        assert!(matches!(&out, Err(AppError::Query(m)) if m.contains("reads rows")), "{out:?}");
        assert_eq!(session_rows(&s.run("select count(*) from export_w", &[], 10).await), [[Some("0".to_string())]]);
        assert!(!target.exists());
        s.close().await;
    })
    .await;
}

#[tokio::test]
async fn export_query_cancel_stops_on_the_server_and_keeps_the_transaction() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let dir = tempfile::tempdir().unwrap();
        let sql = "select g from generate_series(1, 50000000) g";
        for in_tx in [false, true] {
            if in_tx {
                s.run("begin", &[], 1).await;
                s.run("create temp table export_keep(x int)", &[], 1).await;
                s.run("insert into export_keep values (7)", &[], 1).await;
            }
            let target = dir.path().join("big.csv");
            std::fs::write(&target, "old").unwrap();
            let cancel = AtomicBool::new(false);
            let started = std::time::Instant::now();
            let out = run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|n| {
                if n >= 1000 {
                    cancel.store(true, Ordering::SeqCst);
                }
            })
            .await
            .unwrap();
            assert!(matches!(out, ExportOutcome::Canceled), "{out:?}");
            assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
            assert!(no_part_files(dir.path()));
            // The session is still usable, and an open transaction is neither aborted nor ended.
            assert_eq!(session_rows(&s.run("select 1", &[], 1).await), [[Some("1".to_string())]]);
            if in_tx {
                assert!(porthole_lib::query::in_transaction(s.as_ref()).await.unwrap());
                let kept = session_rows(&s.run("select x from export_keep", &[], 10).await);
                assert_eq!(kept, [[Some("7".to_string())]]);
                s.run("rollback", &[], 1).await;
            }
        }
        s.close().await;
    })
    .await;
}

#[tokio::test]
async fn export_query_error_inside_a_transaction_keeps_it_usable() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("e.csv");
        let cancel = AtomicBool::new(false);
        s.run("begin", &[], 1).await;
        let sql = "select 10 / (g - 3) from generate_series(1, 5) g";
        let out = run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await;
        assert!(matches!(&out, Err(AppError::Query(m)) if m.contains("division by zero")), "{out:?}");
        assert!(!target.exists());
        assert!(no_part_files(dir.path()));
        assert!(porthole_lib::query::in_transaction(s.as_ref()).await.unwrap());
        assert_eq!(session_rows(&s.run("select 1", &[], 1).await), [[Some("1".to_string())]]);
        s.run("rollback", &[], 1).await;
        s.close().await;
    })
    .await;
}

fn error_code(run: &porthole_lib::query::StatementRun) -> Option<String> {
    match &run.outcome {
        RawOutcome::Error { code, .. } => code.clone(),
        _ => None,
    }
}

async fn begin_with_keep(s: &std::sync::Arc<dyn QuerySession>) {
    s.run("begin", &[], 1).await;
    s.run("create temp table export_keep(x int)", &[], 1).await;
    s.run("insert into export_keep values (7)", &[], 1).await;
}

/// The transaction is open, not aborted, and still has its own work.
async fn assert_transaction_intact(s: &std::sync::Arc<dyn QuerySession>) {
    assert!(porthole_lib::query::in_transaction(s.as_ref()).await.unwrap());
    assert_eq!(session_rows(&s.run("select x from export_keep", &[], 10).await), [[Some("7".to_string())]]);
}

#[tokio::test]
async fn export_query_inside_a_transaction_releases_its_savepoint() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t.csv");
        let cancel = AtomicBool::new(false);
        begin_with_keep(&s).await;
        let sql = "select x from export_keep";
        let out = run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 1, .. }), "{out:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x\r\n7\r\n");
        assert_transaction_intact(&s).await;
        // 3B001: no such savepoint, so it was released.
        let probe = s.run("rollback to savepoint ts_export", &[], 1).await;
        assert_eq!(error_code(&probe).as_deref(), Some("3B001"));
        s.run("rollback", &[], 1).await;
        s.close().await;
    })
    .await;
}

#[tokio::test]
async fn export_query_cancel_lands_even_before_the_first_row() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("slow.csv");
        begin_with_keep(&s).await;

        // Already canceled when it starts (quit, window closed): nothing runs.
        let cancel = AtomicBool::new(true);
        let started = std::time::Instant::now();
        let sql = "select g from generate_series(1, 50000000) g";
        let out = run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        assert!(matches!(out, ExportOutcome::Canceled), "{out:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "took {:?}", started.elapsed());

        // Canceled while the server works on a statement that sends no rows yet.
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let sql = "select 1 as x from pg_sleep(30)";
        let (out, ()) = tokio::join!(
            run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {}),
            async {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                cancel.store(true, Ordering::SeqCst);
            }
        );
        assert!(matches!(out, Ok(ExportOutcome::Canceled)), "{out:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
        assert!(!target.exists());
        assert!(no_part_files(dir.path()));
        assert_transaction_intact(&s).await;
        s.run("rollback", &[], 1).await;
        s.close().await;
    })
    .await;
}

/// Fails after `after` rows, like a full disk.
struct FullDiskSink {
    after: usize,
    seen: usize,
}

impl porthole_lib::export::RowSink for FullDiskSink {
    fn begin(&mut self, _: &[porthole_lib::export::ExportColumn]) -> porthole_lib::error::AppResult<()> {
        Ok(())
    }

    fn row(&mut self, _: Vec<Option<String>>) -> porthole_lib::error::AppResult<()> {
        self.seen += 1;
        if self.seen > self.after {
            return Err(AppError::Query("Couldn't write the file: disk full".into()));
        }
        Ok(())
    }
}

#[tokio::test]
async fn export_query_sink_failure_stops_the_statement_and_keeps_the_transaction() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let cancel = AtomicBool::new(false);
        for in_tx in [false, true] {
            if in_tx {
                begin_with_keep(&s).await;
            }
            let mut sink = FullDiskSink { after: 1000, seen: 0 };
            let started = std::time::Instant::now();
            let sql = "select g from generate_series(1, 50000000) g";
            let out = stream_query(s.as_ref(), SqlDialect::Postgres, sql, &[], &mut sink, &cancel).await;
            assert!(matches!(&out, Err(AppError::Query(m)) if m.contains("disk full")), "{out:?}");
            // The rest of the 50M rows is not drained: the statement was stopped.
            let probe_started = std::time::Instant::now();
            assert_eq!(session_rows(&s.run("select 1", &[], 1).await), [[Some("1".to_string())]]);
            assert!(probe_started.elapsed() < std::time::Duration::from_millis(500));
            assert!(started.elapsed() < std::time::Duration::from_secs(3), "took {:?}", started.elapsed());
            if in_tx {
                assert_transaction_intact(&s).await;
                s.run("rollback", &[], 1).await;
            }
        }
        s.close().await;
    })
    .await;
}

#[tokio::test]
async fn export_query_reports_a_failed_transaction_and_server_timeouts_plainly() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f.csv");
        let cancel = AtomicBool::new(false);

        s.run("begin", &[], 1).await;
        s.run("select 1/0", &[], 1).await;
        let out = run_query_export(s.as_ref(), SqlDialect::Postgres, "select 1", &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await;
        assert!(matches!(&out, Err(AppError::Query(m)) if m.contains("transaction has failed")), "{out:?}");
        s.run("rollback", &[], 1).await;

        // A statement timeout is the server's error, not a cancel the user asked for.
        s.run("set statement_timeout = 300", &[], 1).await;
        let sql = "select 1 as x from pg_sleep(3)";
        let out = run_query_export(s.as_ref(), SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await;
        assert!(matches!(&out, Err(AppError::Query(m)) if m.contains("statement timeout")), "{out:?}");
        s.run("reset statement_timeout", &[], 1).await;
        assert!(!target.exists());
        assert!(no_part_files(dir.path()));
        s.close().await;
    })
    .await;
}

/// Raises the cancel flag while the server is idle between the export's SAVEPOINT and its
/// statement, where a single cancel request would be ignored.
struct CancelBetweenMessages {
    inner: std::sync::Arc<dyn QuerySession>,
    cancel: std::sync::Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl QuerySession for CancelBetweenMessages {
    async fn run(&self, sql: &str, params: &[Option<String>], cap: usize) -> porthole_lib::query::StatementRun {
        self.inner.run(sql, params, cap).await
    }
    async fn run_simple(&self, sql: &str) -> RawOutcome {
        self.inner.run_simple(sql).await
    }
    async fn stream_rows(
        &self,
        sql: &str,
        params: &[Option<String>],
        sink: &mut dyn porthole_lib::export::RowSink,
    ) -> porthole_lib::error::AppResult<()> {
        self.cancel.store(true, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        self.inner.stream_rows(sql, params, sink).await
    }
    async fn cancel(&self) -> porthole_lib::error::AppResult<()> {
        self.inner.cancel().await
    }
    async fn tx_status(&self) -> porthole_lib::query::TxStatus {
        self.inner.tx_status().await
    }
    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
    async fn set_search_path(&self, schema: Option<&str>) -> porthole_lib::error::AppResult<()> {
        self.inner.set_search_path(schema).await
    }
    async fn close(&self) {
        self.inner.close().await
    }
}

#[tokio::test]
async fn export_query_cancel_between_messages_is_sent_again_until_it_lands() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = query_session(&d).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("gap.csv");
        begin_with_keep(&s).await;
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let gap = CancelBetweenMessages { inner: s.clone(), cancel: cancel.clone() };
        let started = std::time::Instant::now();
        let sql = "select g from generate_series(1, 50000000) g";
        let out = run_query_export(&gap, SqlDialect::Postgres, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        assert!(matches!(out, ExportOutcome::Canceled), "{out:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "took {:?}", started.elapsed());
        assert!(!target.exists());
        assert!(no_part_files(dir.path()));
        assert_transaction_intact(&s).await;
        let probe = s.run("rollback to savepoint ts_export", &[], 1).await;
        assert_eq!(error_code(&probe).as_deref(), Some("3B001"));
        s.run("rollback", &[], 1).await;
        s.close().await;
    })
    .await;
}
