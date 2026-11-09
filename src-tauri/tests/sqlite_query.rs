//! SQLite query tabs: runs, parameters, transactions, Stop and export of query results.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use porthole_lib::driver::sqlite::{connect_sqlite, SqliteDriver};
use porthole_lib::driver::Driver;
use porthole_lib::error::{AppError, AppResult};
use porthole_lib::export::run::{run_query_export, stream_query, ExportOutcome};
use porthole_lib::export::{ExportColumn, ExportFormat, RowSink};
use porthole_lib::model::*;
use porthole_lib::query::{runner, Outcome, QueryLanguage, QuerySession, RawOutcome, RunReport, RunRequest, RunTarget, StatementRun, TxStatus};
use porthole_lib::sqltext::SqlDialect;
use common::sqlite::{sqlite_fixture, sqlite_profile};

const SQLITE: QueryLanguage = QueryLanguage::Sql(SqlDialect::Sqlite);
/// Runs until stopped, and returns no row meanwhile.
const ENDLESS_COUNT: &str = "with recursive c(x) as (select 1 union all select x+1 from c) select count(*) from c";
/// Returns rows until stopped.
const ENDLESS_ROWS: &str = "with recursive c(x) as (select 1 union all select x+1 from c) select x from c";

struct Fixture {
    _dir: tempfile::TempDir,
    driver: SqliteDriver,
    session: Arc<dyn QuerySession>,
}

impl Fixture {
    async fn close(self) {
        self.session.close().await;
        self.driver.close().await;
    }
}

async fn open(read_only: bool) -> Fixture {
    let (dir, path) = sqlite_fixture();
    let driver = connect_sqlite(&sqlite_profile(&path, read_only)).await.unwrap();
    let session = driver.open_query_session(None).await.unwrap();
    Fixture { _dir: dir, driver, session }
}

fn rows(run: &StatementRun) -> &Vec<Vec<Option<String>>> {
    match &run.outcome {
        RawOutcome::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn text(rows: &[&[&str]]) -> Vec<Vec<Option<String>>> {
    rows.iter().map(|r| r.iter().map(|v| Some(v.to_string())).collect()).collect()
}

fn one(run: &StatementRun) -> String {
    rows(run)[0][0].clone().unwrap()
}

fn param(v: &str) -> Vec<Option<String>> {
    vec![Some(v.to_string())]
}

fn run_req(sql: &str) -> RunRequest {
    RunRequest {
        sql: sql.into(),
        target: RunTarget::All,
        params: ParamValues::new(),
        schema: None,
        confirmed: false,
        fetch_all: false,
        saved_query_id: None,
        database: None,
    }
}

async fn run_all(s: &dyn QuerySession, sql: &str) -> RunReport {
    runner::run_in(&SQLITE, s, &Environment::Local, &run_req(sql), &|_| {}).await.unwrap()
}

/// The export left no `ts_export` savepoint behind.
async fn assert_no_export_savepoint(s: &dyn QuerySession) {
    match s.run("release savepoint ts_export", &[], 1).await.outcome {
        RawOutcome::Error { message, .. } => assert!(message.contains("no such savepoint"), "{message}"),
        other => panic!("expected an error, got {other:?}"),
    }
}

fn no_part_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".part"))
}

#[tokio::test]
async fn sqlite_select_returns_typed_rows() {
    let f = open(false).await;
    let report = run_all(f.session.as_ref(), "select id, name from users order by id limit 2").await;
    match &report.statements[0].outcome {
        Outcome::Rows { columns, rows, truncated, cell_types } => {
            let names: Vec<(&str, &str)> = columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
            assert_eq!(names, [("id", "integer"), ("name", "text")]);
            assert_eq!(rows, &text(&[&["1", "Joanna"], &["2", "Hannah"]]));
            assert!(!truncated);
            assert_eq!(cell_types.as_deref(), Some(&["it".to_string(), "it".to_string()][..]));
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // Expressions have no declared type.
    match &f.session.run("select 1 + 1 as two, null", &[], 10).await.outcome {
        RawOutcome::Rows { columns, cell_types, .. } => {
            assert_eq!((columns[0].name.as_str(), columns[0].data_type.as_str()), ("two", ""));
            assert_eq!(cell_types.as_deref(), Some(&["in".to_string()][..]));
        }
        other => panic!("expected rows, got {other:?}"),
    }
    f.close().await;
}

#[tokio::test]
async fn sqlite_row_cap_truncates() {
    let f = open(false).await;
    match &f.session.run("select * from big", &[], 1000).await.outcome {
        RawOutcome::Rows { rows, truncated, cell_types, .. } => {
            assert_eq!(rows.len(), 1000);
            assert!(truncated);
            assert_eq!(cell_types.as_ref().map(Vec::len), Some(1000));
        }
        other => panic!("expected rows, got {other:?}"),
    }
    match &f.session.run("select * from big where id <= 1000", &[], 1000).await.outcome {
        RawOutcome::Rows { rows, truncated, .. } => assert_eq!((rows.len(), *truncated), (1000, false)),
        other => panic!("expected rows, got {other:?}"),
    }
    f.close().await;
}

#[tokio::test]
async fn sqlite_commands_and_returning() {
    let f = open(false).await;
    let run = f.session.run("update users set plan = plan where id = 1", &[], 1000).await;
    assert_eq!(run.outcome, RawOutcome::Command { rows_affected: 1 });
    let run = f.session.run("insert into nopk values ('x','y') returning a", &[], 1000).await;
    assert_eq!(rows(&run), &text(&[&["x"]]));
    assert_eq!(one(&f.session.run("select count(*) from nopk where a = 'x'", &[], 10).await), "1");
    f.close().await;
}

/// `REPLACE` reports its rows like INSERT does (Postgres has no such verb).
#[tokio::test]
async fn sqlite_replace_reports_affected_rows() {
    let f = open(false).await;
    let report = run_all(f.session.as_ref(), "replace into \"Order Items\" (id, name) values (1, 'c'), (9, 'z')").await;
    assert_eq!(report.statements[0].verb, "REPLACE");
    assert_eq!(report.statements[0].outcome, Outcome::Affected { rows: 2 });
    f.close().await;
}

#[tokio::test]
async fn sqlite_params_bind_as_integer_or_text() {
    let f = open(false).await;
    match &f.session.run("select ?1 + 1", &param("41"), 10).await.outcome {
        RawOutcome::Rows { rows, cell_types, .. } => {
            assert_eq!(rows, &text(&[&["42"]]));
            assert_eq!(cell_types.as_deref(), Some(&["i".to_string()][..]));
        }
        other => panic!("expected rows, got {other:?}"),
    }
    assert_eq!(one(&f.session.run("select typeof(?1)", &param("007"), 10).await), "text");
    assert_eq!(rows(&f.session.run("select * from big limit ?1", &param("5"), 1000).await).len(), 5);
    // Only canonical integers bind as INTEGER; NULL binds NULL.
    for (value, expected) in [("0", "integer"), ("-12", "integer"), ("-0", "text"), ("+5", "text"), ("1.5", "text"),
        ("9223372036854775808", "text"), ("-9223372036854775808", "integer"), ("", "text"), (" 1", "text")] {
        assert_eq!(one(&f.session.run("select typeof(?1)", &param(value), 10).await), expected, "{value:?}");
    }
    assert_eq!(one(&f.session.run("select typeof(?1)", &[None], 10).await), "null");
    f.close().await;
}

#[tokio::test]
async fn sqlite_transaction_across_runs() {
    let f = open(false).await;
    let s = f.session.as_ref();
    assert_eq!(s.tx_status().await, TxStatus::Idle);
    assert!(!s.in_transaction().await.unwrap());
    s.run("begin", &[], 1000).await;
    assert_eq!(s.tx_status().await, TxStatus::Open);
    assert!(s.in_transaction().await.unwrap());
    assert!(!s.in_failed_transaction().await);
    let run = s.run("update users set plan = 'z' where id = 1", &[], 1000).await;
    assert_eq!(run.outcome, RawOutcome::Command { rows_affected: 1 });
    // An error doesn't abort a SQLite transaction.
    assert!(matches!(s.run("select * from nope", &[], 10).await.outcome, RawOutcome::Error { .. }));
    assert!(!s.in_failed_transaction().await);
    assert_eq!(s.tx_status().await, TxStatus::Open);
    s.run("rollback", &[], 1000).await;
    assert_eq!(s.tx_status().await, TxStatus::Idle);
    assert_eq!(rows(&s.run("select plan from users where id = 1", &[], 10).await), &vec![vec![None]]);
    s.set_search_path(Some("main")).await.unwrap();
    s.set_search_path(None).await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn sqlite_error_has_a_position() {
    let f = open(false).await;
    // rusqlite 0.39 (bundled, so modern_sqlite) reports SQLite's error offset.
    match f.session.run("select * frm users", &[], 10).await.outcome {
        RawOutcome::Error { message, code, position } => {
            assert_eq!(position, Some(10), "{message}");
            assert_eq!(message, "near \"frm\": syntax error");
            assert_eq!(code.as_deref(), Some("SQLITE_1"));
        }
        other => panic!("expected an error, got {other:?}"),
    }
    // A char position, not a byte offset.
    match f.session.run("select 'é', * frm users", &[], 10).await.outcome {
        RawOutcome::Error { message, position, .. } => assert_eq!(position, Some(15), "{message}"),
        other => panic!("expected an error, got {other:?}"),
    }
    // Other errors carry SQLite's extended code.
    match f.session.run("insert into users (id, name) values (1, 'dup')", &[], 10).await.outcome {
        RawOutcome::Error { message, code, position } => {
            assert!(message.contains("UNIQUE constraint failed"), "{message}");
            assert_eq!(code.as_deref(), Some("SQLITE_1555")); // SQLITE_CONSTRAINT_PRIMARYKEY
            assert_eq!(position, None);
        }
        other => panic!("expected an error, got {other:?}"),
    }
    // Through the runner the position is a UTF-16 offset into the whole text.
    let report = run_all(f.session.as_ref(), "select 1; select * frm users").await;
    match &report.statements[1].outcome {
        Outcome::Error { position, .. } => assert_eq!(*position, Some(19), "{:?}", report.statements[1]),
        other => panic!("expected an error, got {other:?}"),
    }
    f.close().await;
}

#[tokio::test]
async fn sqlite_stop_cancels() {
    let f = open(false).await;
    let session = f.session.clone();
    let task = tokio::spawn(async move { run_all(session.as_ref(), ENDLESS_COUNT).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let canceled_at = Instant::now();
    f.session.cancel().await.unwrap();
    let report = tokio::time::timeout(Duration::from_secs(1), task).await.expect("stopped within 1 s").unwrap();
    assert!(canceled_at.elapsed() < Duration::from_secs(1));
    assert_eq!(report.statements[0].outcome, Outcome::Canceled);
    assert_eq!(rows(&f.session.run("select 1", &[], 10).await), &text(&[&["1"]]));
    f.close().await;
}

#[tokio::test]
async fn sqlite_read_only_session_blocks_writes() {
    let f = open(true).await;
    match f.session.run("delete from users", &[], 10).await.outcome {
        RawOutcome::Error { message, .. } => assert!(message.contains("readonly"), "{message}"),
        other => panic!("expected an error, got {other:?}"),
    }
    assert_eq!(one(&f.session.run("select count(*) from users", &[], 10).await), "17");
    f.close().await;
}

#[tokio::test]
async fn sqlite_run_all_stops_at_first_error() {
    let f = open(false).await;
    let report = run_all(f.session.as_ref(), "select 1; select * from nope; select 2").await;
    let outcomes: Vec<&Outcome> = report.statements.iter().map(|r| &r.outcome).collect();
    assert!(matches!(outcomes[0], Outcome::Rows { .. }), "{outcomes:?}");
    assert!(matches!(outcomes[1], Outcome::Error { message, .. } if message.contains("no such table")), "{outcomes:?}");
    assert_eq!(outcomes[2], &Outcome::Skipped);
    f.close().await;
}

#[tokio::test]
async fn sqlite_open_query_session_has_no_other_databases() {
    let f = open(false).await;
    assert_eq!(
        f.driver.open_query_session(Some("other")).await.err().unwrap(),
        AppError::Unsupported("SQLite has no other databases on this connection.".into())
    );
    f.close().await;
}

#[tokio::test]
async fn sqlite_export_query_writes_every_row() {
    let f = open(false).await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("big.csv");
    let cancel = AtomicBool::new(false);
    let out = run_query_export(f.session.as_ref(), SqlDialect::Sqlite, "select * from big", &[], ExportFormat::Csv, &target, &cancel, &|_| {})
        .await
        .unwrap();
    assert!(matches!(out, ExportOutcome::Done { rows: 60_000, .. }), "{out:?}");
    let text = std::fs::read_to_string(&target).unwrap();
    let lines: Vec<&str> = text.split("\r\n").filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 60_001);
    assert_eq!(lines[0], "id,label");
    assert!(no_part_files(dir.path()));

    // Values keep their storage types in JSON; params bind.
    let target = dir.path().join("typed.json");
    let sql = "select id, label from big where id <= ?1 order by id";
    let out = run_query_export(f.session.as_ref(), SqlDialect::Sqlite, sql, &param("2"), ExportFormat::Json, &target, &cancel, &|_| {})
        .await
        .unwrap();
    assert!(matches!(out, ExportOutcome::Done { rows: 2, .. }), "{out:?}");
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(json[0]["id"], serde_json::json!(1));
    f.close().await;
}

#[tokio::test]
async fn sqlite_export_query_allows_pragma_reads() {
    let f = open(false).await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("cols.csv");
    let cancel = AtomicBool::new(false);
    let sql = "pragma table_info(users)";
    let out = run_query_export(f.session.as_ref(), SqlDialect::Sqlite, sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {})
        .await
        .unwrap();
    assert!(matches!(out, ExportOutcome::Done { rows: 8, .. }), "{out:?}");
    // A write is still refused.
    let out = run_query_export(f.session.as_ref(), SqlDialect::Sqlite, "insert into nopk values ('a','b') returning a", &[], ExportFormat::Csv, &target, &cancel, &|_| {}).await;
    assert!(matches!(&out, Err(AppError::Query(m)) if m.contains("reads rows")), "{out:?}");
    f.close().await;
}

#[tokio::test]
async fn sqlite_export_query_cancel_keeps_the_transaction() {
    let f = open(false).await;
    let s = f.session.as_ref();
    s.run("begin", &[], 1).await;
    assert_eq!(s.run("insert into nopk values ('keep','me')", &[], 1).await.outcome, RawOutcome::Command { rows_affected: 1 });

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("endless.csv");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let out = run_query_export(s, SqlDialect::Sqlite, ENDLESS_ROWS, &[], ExportFormat::Csv, &target, &cancel, &|n| {
        if n >= 1000 {
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .await
    .unwrap();
    assert!(matches!(out, ExportOutcome::Canceled), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert!(!target.exists());
    assert!(no_part_files(dir.path()));

    assert!(s.in_transaction().await.unwrap());
    assert_eq!(one(&s.run("select count(*) from nopk where a = 'keep'", &[], 10).await), "1");
    // The export's savepoint is gone.
    assert_no_export_savepoint(s).await;
    s.run("rollback", &[], 1).await;
    assert_eq!(s.tx_status().await, TxStatus::Idle);
    f.close().await;
}

/// The sibling of `sqlite_export_query_cancel_keeps_the_transaction`: the export over an existing
/// file is canceled, the tab commits, and its work is in the file for every connection.
#[tokio::test]
async fn sqlite_export_query_cancel_then_commit_keeps_the_work() {
    let (_fixture_dir, path) = sqlite_fixture();
    let driver = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let session = driver.open_query_session(None).await.unwrap();
    let s = session.as_ref();
    s.run("begin", &[], 1).await;
    assert_eq!(s.run("insert into nopk values ('keep','me')", &[], 1).await.outcome, RawOutcome::Command { rows_affected: 1 });

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("endless.csv");
    std::fs::write(&target, "old").unwrap();
    let cancel = AtomicBool::new(false);
    let out = run_query_export(s, SqlDialect::Sqlite, ENDLESS_ROWS, &[], ExportFormat::Csv, &target, &cancel, &|n| {
        if n >= 1000 {
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .await
    .unwrap();
    assert!(matches!(out, ExportOutcome::Canceled), "{out:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    assert!(no_part_files(dir.path()));

    assert!(s.in_transaction().await.unwrap());
    let commit = s.run("commit", &[], 1).await;
    assert!(matches!(commit.outcome, RawOutcome::Command { .. }), "{:?}", commit.outcome);
    assert_eq!(s.tx_status().await, TxStatus::Idle);
    let other = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let kept: String = other.query_row("select b from nopk where a = 'keep'", [], |r| r.get(0)).unwrap();
    assert_eq!(kept, "me");
    drop(other);
    session.close().await;
    driver.close().await;
}

/// A query tab that reads, opens a transaction and rolls it back leaves the file byte-identical
/// and adds no file next to it, on a read-write and a read-only connection.
#[tokio::test]
async fn a_query_session_leaves_the_file_byte_identical() {
    for read_only in [false, true] {
        let (dir, path) = sqlite_fixture();
        let listing = || {
            let mut names: Vec<String> =
                std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        };
        let (before, files) = (std::fs::read(&path).unwrap(), listing());
        let driver = connect_sqlite(&sqlite_profile(&path, read_only)).await.unwrap();
        let session = driver.open_query_session(None).await.unwrap();
        let s = session.as_ref();
        assert_eq!(one(&s.run("select count(*) from users", &[], 10).await), "17");
        for sql in ["begin", "select name from users where id = 1", "rollback"] {
            let run = s.run(sql, &[], 10).await;
            assert!(!matches!(run.outcome, RawOutcome::Error { .. }), "{read_only} {sql}: {:?}", run.outcome);
        }
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        session.close().await;
        driver.close().await;
        drop(driver);
        assert_eq!(std::fs::read(&path).unwrap(), before, "read_only: {read_only}");
        assert_eq!(listing(), files, "read_only: {read_only}");
        assert_eq!(files, vec!["fx.sqlite".to_string()]);
    }
}

#[tokio::test]
async fn sqlite_export_query_outside_a_transaction_sets_no_savepoint() {
    let f = open(false).await;
    let s = f.session.as_ref();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("users.csv");
    let cancel = AtomicBool::new(false);
    let out = run_query_export(s, SqlDialect::Sqlite, "select * from users", &[], ExportFormat::Csv, &target, &cancel, &|_| {})
        .await
        .unwrap();
    assert!(matches!(out, ExportOutcome::Done { rows: 17, .. }), "{out:?}");
    assert_no_export_savepoint(s).await;
    assert_eq!(s.tx_status().await, TxStatus::Idle);
    f.close().await;
}

/// Fails at row `fail_at`.
struct FailingSink {
    rows: usize,
    fail_at: usize,
}

impl RowSink for FailingSink {
    fn begin(&mut self, _: &[ExportColumn]) -> AppResult<()> {
        Ok(())
    }
    fn row(&mut self, _: Vec<Option<String>>) -> AppResult<()> {
        self.rows += 1;
        if self.rows == self.fail_at {
            return Err(AppError::Query("disk full".into()));
        }
        Ok(())
    }
}

#[tokio::test]
async fn sqlite_a_failing_sink_stops_the_statement() {
    let f = open(false).await;
    let s = f.session.as_ref();
    let cancel = AtomicBool::new(false);
    for in_tx in [false, true] {
        if in_tx {
            s.run("begin", &[], 1).await;
            s.run("insert into nopk values ('keep','me')", &[], 1).await;
        }
        let mut sink = FailingSink { rows: 0, fail_at: 10 };
        let started = Instant::now();
        let out = stream_query(s, SqlDialect::Sqlite, ENDLESS_ROWS, &[], &mut sink, &cancel).await;
        assert_eq!(out, Err(AppError::Query("disk full".into())));
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
        assert_eq!(sink.rows, 10);
        // Nothing runs on the session any more.
        let probe = tokio::time::timeout(Duration::from_secs(1), s.run("select 1", &[], 10)).await.expect("idle");
        assert_eq!(rows(&probe), &text(&[&["1"]]));
        assert_eq!(s.in_transaction().await.unwrap(), in_tx);
        if in_tx {
            assert_eq!(one(&s.run("select count(*) from nopk where a = 'keep'", &[], 10).await), "1");
            s.run("rollback", &[], 1).await;
        }
    }
    f.close().await;
}

#[tokio::test]
async fn sqlite_export_query_cancel_stops_a_statement_without_rows() {
    let f = open(false).await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("count.csv");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let (out, ()) = tokio::join!(
        run_query_export(f.session.as_ref(), SqlDialect::Sqlite, ENDLESS_COUNT, &[], ExportFormat::Csv, &target, &cancel, &|_| {}),
        async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.store(true, Ordering::SeqCst);
        }
    );
    assert!(matches!(out, Ok(ExportOutcome::Canceled)), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    assert!(!target.exists());
    assert!(no_part_files(dir.path()));
    assert_eq!(rows(&f.session.run("select 1", &[], 10).await), &text(&[&["1"]]));
    f.close().await;
}

#[tokio::test]
async fn sqlite_export_query_inside_a_transaction_releases_its_savepoint() {
    let f = open(false).await;
    let s = f.session.as_ref();
    s.run("begin", &[], 1).await;
    s.run("insert into nopk values ('keep','me')", &[], 1).await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("nopk.csv");
    let cancel = AtomicBool::new(false);
    let out = run_query_export(s, SqlDialect::Sqlite, "select * from nopk", &[], ExportFormat::Csv, &target, &cancel, &|_| {})
        .await
        .unwrap();
    assert!(matches!(out, ExportOutcome::Done { .. }), "{out:?}");
    assert!(std::fs::read_to_string(&target).unwrap().contains("keep,me"));
    assert_eq!(s.tx_status().await, TxStatus::Open);
    assert_eq!(one(&s.run("select count(*) from nopk where a = 'keep'", &[], 10).await), "1");
    assert_no_export_savepoint(s).await;
    s.run("rollback", &[], 1).await;
    assert_eq!(s.tx_status().await, TxStatus::Idle);
    f.close().await;
}

#[tokio::test]
async fn sqlite_export_query_errors_are_sqlites_message() {
    let f = open(false).await;
    let cancel = AtomicBool::new(false);
    let mut sink = FailingSink { rows: 0, fail_at: 0 };
    let out = stream_query(f.session.as_ref(), SqlDialect::Sqlite, "select * frm users", &[], &mut sink, &cancel).await;
    assert_eq!(out, Err(AppError::Query("near \"frm\": syntax error".into())));
    f.close().await;
}

const LOST_TX: &str = "SQLite rolled back the open transaction, so its earlier changes are gone.";

fn has_lost_tx_notice(notices: &[porthole_lib::query::Notice]) -> bool {
    notices.iter().any(|n| n.message == LOST_TX)
}

#[tokio::test]
async fn sqlite_stop_on_a_write_reports_the_lost_transaction() {
    let f = open(false).await;
    let report = run_all(f.session.as_ref(), "begin; insert into nopk values ('lost','tx')").await;
    assert!(report.statements.iter().all(|r| r.notices.is_empty()));
    assert_eq!(f.session.tx_status().await, TxStatus::Open);

    let session = f.session.clone();
    let task = tokio::spawn(async move {
        run_all(session.as_ref(), "insert into nopk select 'x','y' from (with recursive c(x) as (select 1 union all select x+1 from c) select x from c)").await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    f.session.cancel().await.unwrap();
    let report = tokio::time::timeout(Duration::from_secs(2), task).await.expect("stopped").unwrap();
    let stopped = &report.statements[0];
    assert_eq!(stopped.outcome, Outcome::Canceled);
    assert!(has_lost_tx_notice(&stopped.notices), "{:?}", stopped.notices);
    assert_eq!(f.session.tx_status().await, TxStatus::Idle);
    assert_eq!(one(&f.session.run("select count(*) from nopk where a = 'lost'", &[], 10).await), "0");
    f.close().await;
}

#[tokio::test]
async fn sqlite_an_error_that_keeps_the_transaction_adds_no_notice() {
    let f = open(false).await;
    let report = run_all(f.session.as_ref(), "begin; insert into users (id, name) values (1, 'dup')").await;
    assert!(matches!(report.statements[1].outcome, Outcome::Error { .. }), "{:?}", report.statements[1]);
    assert!(report.statements.iter().all(|r| r.notices.is_empty()), "{report:?}");
    assert_eq!(f.session.tx_status().await, TxStatus::Open);
    // Ending the transaction on purpose adds no notice either.
    for end in ["rollback", "begin; commit", "begin; end"] {
        let report = run_all(f.session.as_ref(), end).await;
        assert!(report.statements.iter().all(|r| r.notices.is_empty()), "{end}: {report:?}");
        assert_eq!(f.session.tx_status().await, TxStatus::Idle);
    }
    f.close().await;
}
