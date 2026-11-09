//! MySQL / MariaDB query tabs: runs, parameters, transactions, Stop, warnings, procedures and
//! export of query results, on every server in `TEST_MYSQL_URLS`.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use porthole_lib::driver::mysql::MysqlDriver;
use porthole_lib::driver::Driver;
use porthole_lib::error::{AppError, AppResult};
use porthole_lib::export::run::{run_query_export, stream_query, ExportOutcome};
use porthole_lib::export::{ExportColumn, ExportFormat, RowSink};
use porthole_lib::model::*;
use porthole_lib::query::{
    runner, Notice, Outcome, QueryLanguage, QuerySession, RawOutcome, RunReport, RunRequest, RunTarget, StatementRun,
    TxStatus,
};
use porthole_lib::sqltext::SqlDialect;
use common::mysql::{for_each_mysql, mysql_driver};
use mysql_async::prelude::Queryable;

const MYSQL: QueryLanguage = QueryLanguage::Sql(SqlDialect::Mysql);
/// Runs for minutes (3.6 billion pairs) and returns no row meanwhile. The `where` keeps MySQL 8.4
/// from answering a bare `count(*)` of a cross join from the tables' row counts.
const HEAVY_COUNT: &str = "select count(*) from big a, big b where a.id + b.id > 0";
/// Returns rows for minutes.
const ENDLESS_ROWS: &str = "select a.id from big a, big b";

struct Fixture {
    driver: MysqlDriver,
    session: Arc<dyn QuerySession>,
}

impl Fixture {
    async fn close(self) {
        self.session.close().await;
        self.driver.close().await;
    }
}

async fn open(p: &ConnectionProfile, pw: &str) -> Fixture {
    let driver = mysql_driver(p, pw).await;
    let session = driver.open_query_session(None).await.unwrap();
    Fixture { driver, session }
}

fn rows(run: &StatementRun) -> &Vec<Vec<Option<String>>> {
    rows_of(&run.outcome)
}

fn rows_of(outcome: &RawOutcome) -> &Vec<Vec<Option<String>>> {
    match outcome {
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

fn error_code(outcome: &RawOutcome) -> Option<String> {
    match outcome {
        RawOutcome::Error { code, .. } => code.clone(),
        other => panic!("expected an error, got {other:?}"),
    }
}

fn run_req(sql: &str, params: &[(&str, &str)]) -> RunRequest {
    RunRequest {
        sql: sql.into(),
        target: RunTarget::All,
        params: params.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect(),
        schema: None,
        confirmed: false,
        fetch_all: false,
        saved_query_id: None,
        database: None,
    }
}

async fn run_all(s: &dyn QuerySession, sql: &str) -> RunReport {
    run_with(s, sql, &[]).await
}

async fn run_with(s: &dyn QuerySession, sql: &str, params: &[(&str, &str)]) -> RunReport {
    runner::run_in(&MYSQL, s, &Environment::Local, &run_req(sql, params), &|_| {}).await.unwrap()
}

fn outcome_rows(report: &RunReport, i: usize) -> &Vec<Vec<Option<String>>> {
    match &report.statements[i].outcome {
        Outcome::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn no_part_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".part"))
}

fn csv_field(v: &Option<String>) -> String {
    match v {
        None => String::new(),
        Some(s) if s.is_empty() => "\"\"".into(),
        Some(s) if s.contains([',', '"', '\r', '\n']) || s.starts_with(' ') || s.ends_with(' ') => {
            format!("\"{}\"", s.replace('"', "\"\""))
        }
        Some(s) => s.clone(),
    }
}

/// The export left no `ts_export` savepoint behind: releasing it fails with 1305.
async fn assert_no_export_savepoint(s: &dyn QuerySession) {
    let run = s.run("release savepoint ts_export", &[], 1).await;
    assert_eq!(error_code(&run.outcome).as_deref(), Some("MYSQL_1305"), "{:?}", run.outcome);
}

#[tokio::test]
async fn mysql_select_returns_typed_rows() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let report = run_all(f.session.as_ref(), "select id, name from users order by id limit 2").await;
        match &report.statements[0].outcome {
            Outcome::Rows { columns, rows, truncated, cell_types } => {
                let names: Vec<(&str, &str)> = columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
                assert_eq!(names, [("id", "int"), ("name", "varchar")]);
                assert_eq!(rows, &text(&[&["1", "Joanna"], &["2", "Hannah"]]));
                assert!(!truncated);
                assert_eq!(cell_types.as_deref(), Some(&["it".to_string(), "it".to_string()][..]));
            }
            other => panic!("expected rows, got {other:?}"),
        }
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_row_cap_truncates_and_the_session_stays_usable() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        // A small result the server has sent in full, and a big one it is still sending.
        for (sql, cap) in [("select id from users order by id", 5), ("select a.id from big a, big b", 1000)] {
            let started = Instant::now();
            match &s.run(sql, &[], cap).await.outcome {
                RawOutcome::Rows { rows, truncated, cell_types, .. } => {
                    assert_eq!(rows.len(), cap, "{sql}");
                    assert!(*truncated, "{sql}");
                    assert_eq!(cell_types.as_ref().map(Vec::len), Some(cap));
                }
                other => panic!("expected rows, got {other:?}"),
            }
            assert!(started.elapsed() < Duration::from_secs(5), "{sql} took {:?}", started.elapsed());
            // The rest was stopped, not left to hit the next statement.
            assert_eq!(rows(&s.run("select 1", &[], 10).await), &text(&[&["1"]]), "{sql}");
            assert_eq!(rows(&s.run("select 2", &[], 10).await), &text(&[&["2"]]), "{sql}");
        }
        f.close().await;
    })
    .await;
}

/// Whether the session's server is MariaDB.
async fn is_mariadb(s: &dyn QuerySession) -> bool {
    one(&s.run("select version()", &[], 1).await).contains("MariaDB")
}

#[tokio::test]
async fn mysql_row_cap_lets_a_write_run_to_its_end() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        // MySQL has no RETURNING.
        if is_mariadb(s).await {
            s.run("create table scratch as select * from big", &[], 1).await;
            let run = s.run("delete from scratch returning id", &[], 1000).await;
            match &run.outcome {
                RawOutcome::Rows { rows, truncated, .. } => assert_eq!((rows.len(), *truncated), (1000, true)),
                other => panic!("expected rows, got {other:?}"),
            }
            assert_eq!(one(&s.run("select count(*) from scratch", &[], 10).await), "0");
        }
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_row_cap_lets_a_procedure_run_to_its_end() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        let create = "create procedure many_then_mark() begin \
                      select id from big; insert into nopk values ('mark', 'x'); select 2; end";
        assert!(matches!(s.run(create, &[], 1).await.outcome, RawOutcome::Command { .. }));
        let run = s.run("call many_then_mark()", &[], 1000).await;
        match &run.outcome {
            RawOutcome::Rows { rows, truncated, .. } => assert_eq!((rows.len(), *truncated), (1000, true)),
            other => panic!("expected rows, got {other:?}"),
        }
        assert!(
            run.notices.iter().any(|n| n.message == "The procedure returned 1 more result sets; only the first is shown."),
            "{:?}",
            run.notices
        );
        assert_eq!(one(&s.run("select count(*) from nopk where a = 'mark'", &[], 10).await), "1");
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_search_path_none_restores_the_database_after_startup_sql() {
    for_each_mysql(|mut p, pw| async move {
        p.startup_sql = Some("USE fx_other".into());
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        assert_eq!(one(&s.run("select database()", &[], 1).await), "fx_other");
        s.set_search_path(Some("fx")).await.unwrap();
        assert_eq!(one(&s.run("select database()", &[], 1).await), "fx");
        s.set_search_path(None).await.unwrap();
        assert_eq!(one(&s.run("select database()", &[], 1).await), "fx_other");
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_values_read_the_same_on_every_path() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        let page = f
            .driver
            .fetch_page(&PageRequest {
                table: TableRef { schema: "fx".into(), name: "typed".into() },
                filter: None,
                sort: None,
                page: 0,
                page_size: 100,
            })
            .await
            .unwrap();
        assert_eq!(page.rows.len(), 3);
        let expected_first: Vec<Option<String>> = [
            "1",
            "1e300",
            "1.23457",
            "2026-01-02 03:04:05.678",
            "-100:02:03.45",
            "12.30",
            "\\xdead",
            "00101",
            "{\"a\": [1, 2]}",
            "2026",
            "b",
            "x,y",
            "18446744073709551615",
            "1",
            "2026-01-02",
        ]
        .iter()
        .map(|v| Some(v.to_string()))
        .collect();
        assert_eq!(page.rows[0], expected_first);
        assert_eq!(page.cell_types.as_ref().unwrap()[0], "irrttrbbtittiit");

        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        for (i, page_row) in page.rows.iter().enumerate() {
            let id = (i + 1).to_string();
            let page_types = &page.cell_types.as_ref().unwrap()[i];
            let plain_sql = format!("select * from typed where id = {id}");

            let plain = s.run(&plain_sql, &[], 10).await;
            assert_eq!(&rows(&plain)[0], page_row, "run, row {id}");
            match &plain.outcome {
                RawOutcome::Rows { cell_types, .. } => assert_eq!(&cell_types.as_ref().unwrap()[0], page_types, "row {id}"),
                _ => unreachable!(),
            }

            let bound = s.run("select * from typed where id = ?", &[Some(id.clone())], 10).await;
            assert_eq!(&rows(&bound)[0], page_row, "run with a value, row {id}");
            match &bound.outcome {
                RawOutcome::Rows { cell_types, .. } => assert_eq!(&cell_types.as_ref().unwrap()[0], page_types, "row {id}"),
                _ => unreachable!(),
            }

            let simple = s.run_simple(&plain_sql).await;
            assert_eq!(&rows_of(&simple)[0], page_row, "run_simple, row {id}");

            let target = dir.path().join(format!("typed{id}.csv"));
            let out = run_query_export(s, SqlDialect::Mysql, &plain_sql, &[], ExportFormat::Csv, &target, &cancel, &|_| {})
                .await
                .unwrap();
            assert!(matches!(out, ExportOutcome::Done { rows: 1, .. }), "{out:?}");
            let mut expected = page.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(",");
            expected.push_str("\r\n");
            expected.push_str(&page_row.iter().map(csv_field).collect::<Vec<_>>().join(","));
            expected.push_str("\r\n");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), expected, "export, row {id}");
        }
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_params_through_the_runner() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        let report = run_with(s, "select :a, :b, :a", &[("a", "1"), ("b", "x")]).await;
        assert_eq!(outcome_rows(&report, 0), &text(&[&["1", "x", "1"]]));
        let report = run_with(s, "select * from big order by id limit :n", &[("n", "5")]).await;
        assert_eq!(outcome_rows(&report, 0).len(), 5);
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_transaction_across_runs() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        run_all(s, "begin").await;
        assert_eq!(s.tx_status().await, TxStatus::Open);
        assert!(s.in_transaction().await.unwrap());
        let report = run_all(s, "update users set plan = 'z' where id = 1").await;
        assert_eq!(report.statements[0].outcome, Outcome::Affected { rows: 1 });
        let report = run_with(s, "select plan from users where id = :id", &[("id", "1")]).await;
        assert_eq!(outcome_rows(&report, 0), &text(&[&["z"]]));
        assert_eq!(s.tx_status().await, TxStatus::Open);
        // A failed statement leaves the transaction open, and says so.
        let report = run_all(s, "select * from nope").await;
        assert!(matches!(report.statements[0].outcome, Outcome::Error { .. }));
        assert_eq!(s.tx_status().await, TxStatus::Open);
        assert!(!s.in_failed_transaction().await);
        run_all(s, "rollback").await;
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        assert!(!s.in_transaction().await.unwrap());
        assert_eq!(rows(&s.run("select plan from users where id = 1", &[], 10).await), &vec![vec![None]]);
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_error_has_a_position() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        match &s.run("select * frm users", &[], 10).await.outcome {
            RawOutcome::Error { code, position, .. } => {
                assert_eq!(code.as_deref(), Some("MYSQL_1064"));
                assert_eq!(*position, Some(10));
            }
            other => panic!("expected an error, got {other:?}"),
        }
        // On a later line, after a multi-byte character, and through PREPARE.
        let sql = "select 'é',\n  1 from where id = ?";
        match &s.run(sql, &[Some("1".into())], 10).await.outcome {
            RawOutcome::Error { code, position, .. } => {
                assert_eq!(code.as_deref(), Some("MYSQL_1064"));
                assert_eq!(*position, Some(22));
            }
            other => panic!("expected an error, got {other:?}"),
        }
        // Nothing after the error: the statement's length.
        match &s.run("select * from", &[], 10).await.outcome {
            RawOutcome::Error { code, position, .. } => {
                assert_eq!(code.as_deref(), Some("MYSQL_1064"));
                assert_eq!(*position, Some(13));
            }
            other => panic!("expected an error, got {other:?}"),
        }
        // Other errors have a code and no position.
        match &s.run("select * from nope", &[], 10).await.outcome {
            RawOutcome::Error { code, position, .. } => {
                assert_eq!(code.as_deref(), Some("MYSQL_1146"));
                assert_eq!(*position, None);
            }
            other => panic!("expected an error, got {other:?}"),
        }
        // The runner maps the position into the editor's text (0-based UTF-16).
        let report = run_all(s, "select 1;\nselect * frm users").await;
        match &report.statements[1].outcome {
            Outcome::Error { position, .. } => assert_eq!(*position, Some(19)),
            other => panic!("expected an error, got {other:?}"),
        }
        assert!(!s.is_closed());
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_warnings_and_info_are_notices() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        let run = s.run("select cast('abc' as signed)", &[], 10).await;
        assert_eq!(rows(&run), &text(&[&["0"]]));
        assert!(
            run.notices.iter().any(|n| n.severity == "WARNING" && n.message.contains("abc")),
            "{:?}",
            run.notices
        );
        let run = s.run("update users set plan = plan where id = 1", &[], 10).await;
        assert!(matches!(run.outcome, RawOutcome::Command { .. }), "{:?}", run.outcome);
        assert!(
            run.notices.iter().any(|n| n.severity == "INFO" && n.message.starts_with("Rows matched: 1")),
            "{:?}",
            run.notices
        );
        // With a value too; and a statement without either has no notices.
        let run = s.run("select cast(? as signed)", &[Some("abc".into())], 10).await;
        assert!(run.notices.iter().any(|n| n.severity == "WARNING" && n.message.contains("abc")), "{:?}", run.notices);
        assert_eq!(s.run("select 1", &[], 10).await.notices, Vec::<Notice>::new());
        f.close().await;
    })
    .await;
}

/// Waits until a statement starting with one of `prefixes` runs on the server (another
/// connection's), for at most 10 s.
async fn wait_until_running(p: &ConnectionProfile, pw: &str, prefixes: &[&str]) {
    let mut c = common::mysql::mysql_conn(&common::mysql::mysql_profile_url(p, pw)).await;
    let sql = format!(
        "select count(*) from information_schema.PROCESSLIST where ID <> CONNECTION_ID() and ({})",
        prefixes.iter().map(|_| "INFO like concat(?, '%')").collect::<Vec<_>>().join(" or ")
    );
    let started = Instant::now();
    loop {
        let n: Option<i64> = c.exec_first(sql.as_str(), prefixes.to_vec()).await.unwrap();
        if n.unwrap_or(0) > 0 {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "{prefixes:?} never showed in the process list");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.disconnect().await.ok();
}

#[tokio::test]
async fn mysql_stop_cancels() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let session = f.session.clone();
        let task = tokio::spawn(async move { run_all(session.as_ref(), HEAVY_COUNT).await });
        wait_until_running(&p, &pw, &[HEAVY_COUNT]).await;
        let canceled_at = Instant::now();
        f.session.cancel().await.unwrap();
        let report = tokio::time::timeout(Duration::from_secs(1), task).await.expect("stopped within 1 s").unwrap();
        assert!(canceled_at.elapsed() < Duration::from_secs(1));
        assert_eq!(report.statements[0].outcome, Outcome::Canceled);
        assert_eq!(rows(&f.session.run("select 1", &[], 10).await), &text(&[&["1"]]));
        assert!(!f.session.is_closed());

        // With a value: the statement runs through EXECUTE.
        let session = f.session.clone();
        let task = tokio::spawn(async move { run_with(session.as_ref(), &format!("{HEAVY_COUNT} and :n = 1"), &[("n", "1")]).await });
        // With a value the server runs `EXECUTE ts_q USING …`; the process list shows either
        // that or the prepared text, by server.
        wait_until_running(&p, &pw, &[HEAVY_COUNT, "EXECUTE ts_q"]).await;
        f.session.cancel().await.unwrap();
        let report = tokio::time::timeout(Duration::from_secs(1), task).await.expect("stopped within 1 s").unwrap();
        assert_eq!(report.statements[0].outcome, Outcome::Canceled);
        assert_eq!(rows(&f.session.run("select 1", &[], 10).await), &text(&[&["1"]]));
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_procedure_with_two_result_sets() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        let run = s.run("call two_sets(1, @t)", &[], 10).await;
        assert_eq!(rows(&run), &text(&[&["1"]]));
        assert!(
            run.notices.iter().any(|n| n.message == "The procedure returned 1 more result sets; only the first is shown."),
            "{:?}",
            run.notices
        );
        assert_eq!(rows(&s.run("select @t", &[], 10).await), &text(&[&["1"]]));
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_compound_statement_runs_whole() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let sql = "drop procedure if exists p2; create procedure p2() begin select 1; select 2; end; call p2()";
        let report = run_all(f.session.as_ref(), sql).await;
        let outcomes: Vec<&Outcome> = report.statements.iter().map(|r| &r.outcome).collect();
        assert_eq!(outcomes.len(), 3, "{outcomes:?}");
        assert!(matches!(outcomes[0], Outcome::Done | Outcome::Affected { .. }), "{outcomes:?}");
        assert!(matches!(outcomes[1], Outcome::Done | Outcome::Affected { .. }), "{outcomes:?}");
        assert_eq!(outcome_rows(&report, 2), &text(&[&["1"]]));
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_use_switches_database() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        s.set_search_path(Some("fx_other")).await.unwrap();
        assert_eq!(rows(&s.run("select * from other_t", &[], 10).await), &text(&[&["1", "one"]]));
        s.set_search_path(None).await.unwrap();
        assert_eq!(one(&s.run("select count(*) from users", &[], 10).await), "17");
        assert_eq!(one(&s.run("select database()", &[], 10).await), "fx");
        // A database that doesn't exist is the server's error.
        assert!(s.set_search_path(Some("nope_db")).await.is_err());
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_open_query_session_on_another_database_is_unsupported() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        match d.open_query_session(Some("fx_other")).await {
            Err(AppError::Unsupported(m)) => assert_eq!(m, "MySQL tabs switch databases with the Database list."),
            Err(e) => panic!("expected Unsupported, got {e:?}"),
            Ok(_) => panic!("expected Unsupported, got a session"),
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_read_only_session_blocks_writes() {
    for_each_mysql(|mut p, pw| async move {
        p.read_only = true;
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        assert_eq!(error_code(&s.run("delete from users", &[], 10).await.outcome).as_deref(), Some("MYSQL_1792"));
        assert_eq!(one(&s.run("select count(*) from users", &[], 10).await), "17");
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_run_all_stops_at_first_error() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let report = run_all(f.session.as_ref(), "select 1; select * from nope; select 2").await;
        let outcomes: Vec<&Outcome> = report.statements.iter().map(|r| &r.outcome).collect();
        assert!(matches!(outcomes[0], Outcome::Rows { .. }), "{outcomes:?}");
        assert!(matches!(outcomes[1], Outcome::Error { code: Some(c), .. } if c == "MYSQL_1146"), "{outcomes:?}");
        assert_eq!(outcomes[2], &Outcome::Skipped);
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_export_query_writes_every_row() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("big.csv");
        let cancel = AtomicBool::new(false);
        let out = run_query_export(f.session.as_ref(), SqlDialect::Mysql, "select * from big", &[], ExportFormat::Csv, &target, &cancel, &|_| {})
            .await
            .unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 60_000, .. }), "{out:?}");
        let text = std::fs::read_to_string(&target).unwrap();
        let lines: Vec<&str> = text.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 60_001);
        assert_eq!(lines[0], "id,label");
        assert!(no_part_files(dir.path()));

        // Values bind; JSON keeps the cell types.
        let target = dir.path().join("typed.json");
        let sql = "select id, label from big where id <= ? order by id";
        let out = run_query_export(f.session.as_ref(), SqlDialect::Mysql, sql, &[Some("2".into())], ExportFormat::Json, &target, &cancel, &|_| {})
            .await
            .unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 2, .. }), "{out:?}");
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(json[0]["id"], serde_json::json!(1));
        assert_eq!(json[1]["label"], serde_json::json!("v2"));
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_export_query_cancel_keeps_the_transaction() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        s.run("begin", &[], 1).await;
        assert_eq!(s.run("insert into nopk values ('keep','me')", &[], 1).await.outcome, RawOutcome::Command { rows_affected: 1 });

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("endless.csv");
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let out = run_query_export(s, SqlDialect::Mysql, ENDLESS_ROWS, &[], ExportFormat::Csv, &target, &cancel, &|n| {
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
        assert_eq!(s.tx_status().await, TxStatus::Open);
        assert_eq!(one(&s.run("select count(*) from nopk where a = 'keep'", &[], 10).await), "1");
        assert_no_export_savepoint(s).await;
        s.run("rollback", &[], 1).await;
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        assert_eq!(one(&s.run("select count(*) from nopk where a = 'keep'", &[], 10).await), "0");
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_export_outside_a_transaction_sets_no_savepoint() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        assert!(!s.export_savepoint().await.unwrap());
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("users.csv");
        let cancel = AtomicBool::new(false);
        let out = run_query_export(s, SqlDialect::Mysql, "select * from users", &[], ExportFormat::Csv, &target, &cancel, &|_| {})
            .await
            .unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 17, .. }), "{out:?}");
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        assert!(!s.in_transaction().await.unwrap());

        // Inside a transaction the export runs under a savepoint, released after.
        s.run("begin", &[], 1).await;
        let out = run_query_export(s, SqlDialect::Mysql, "select * from users", &[], ExportFormat::Csv, &target, &cancel, &|_| {})
            .await
            .unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 17, .. }), "{out:?}");
        assert_eq!(s.tx_status().await, TxStatus::Open);
        assert_no_export_savepoint(s).await;
        s.run("rollback", &[], 1).await;
        f.close().await;
    })
    .await;
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
    fn row(&mut self, row: Vec<Option<String>>) -> AppResult<()> {
        self.typed_row(row, None)
    }
    fn typed_row(&mut self, _: Vec<Option<String>>, _: Option<&str>) -> AppResult<()> {
        self.rows += 1;
        if self.rows == self.fail_at {
            return Err(AppError::Query("disk full".into()));
        }
        Ok(())
    }
}

#[tokio::test]
async fn mysql_a_failing_sink_stops_the_statement() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        let cancel = AtomicBool::new(false);
        let mut sink = FailingSink { rows: 0, fail_at: 10 };
        let started = Instant::now();
        let out = stream_query(s, SqlDialect::Mysql, ENDLESS_ROWS, &[], &mut sink, &cancel).await;
        assert!(matches!(&out, Err(AppError::Query(m)) if m == "disk full"), "{out:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
        assert_eq!(rows(&s.run("select 1", &[], 10).await), &text(&[&["1"]]));
        f.close().await;
    })
    .await;
}

/// The session itself stops the statement when the sink fails: `stream_rows` without
/// `stream_query`, whose `settle` would cancel it too.
#[tokio::test]
async fn mysql_stream_rows_stops_the_statement_when_the_sink_fails() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let s = f.session.as_ref();
        let mut sink = FailingSink { rows: 0, fail_at: 10 };
        let started = Instant::now();
        let out = s.stream_rows(ENDLESS_ROWS, &[], &mut sink).await;
        assert!(matches!(&out, Err(AppError::Query(m)) if m == "disk full"), "{out:?}");
        // Reading 3.6 billion rows takes minutes.
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
        assert_eq!(rows(&s.run("select 1", &[], 10).await), &text(&[&["1"]]));
        assert!(!s.is_closed());
        f.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_export_query_errors_are_the_servers_message() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nope.csv");
        let cancel = AtomicBool::new(false);
        let out = run_query_export(f.session.as_ref(), SqlDialect::Mysql, "select * from nope", &[], ExportFormat::Csv, &target, &cancel, &|_| {})
            .await;
        assert!(matches!(&out, Err(AppError::Query(m)) if m.contains("nope")), "{out:?}");
        assert!(!target.exists());
        assert!(no_part_files(dir.path()));
        f.close().await;
    })
    .await;
}

/// The server's count of connection attempts so far.
async fn connections_so_far(c: &mut mysql_async::Conn) -> u64 {
    let row: Option<(String, String)> = c.query_first("SHOW GLOBAL STATUS LIKE 'Connections'").await.unwrap();
    row.unwrap().1.parse().unwrap()
}

/// I3: closing an idle tab opens no connection to stop a statement (none runs); a busy tab's
/// close still stops what runs.
#[tokio::test]
async fn mysql_closing_an_idle_session_opens_no_connection() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        assert_eq!(rows(&f.session.run("select 1", &[], 10).await), &text(&[&["1"]]));
        let mut watch = common::mysql::mysql_conn(&common::mysql::mysql_profile_url(&p, &pw)).await;
        let before = connections_so_far(&mut watch).await;
        f.session.close().await;
        assert_eq!(connections_so_far(&mut watch).await, before, "close opened a connection");
        assert!(f.session.is_closed());

        // Busy: the statement is stopped, and the close doesn't wait for it to run out.
        let busy = f.driver.open_query_session(None).await.unwrap();
        let s = busy.clone();
        let task = tokio::spawn(async move { s.run(HEAVY_COUNT, &[], 10).await });
        wait_until_running(&p, &pw, &[HEAVY_COUNT]).await;
        let started = Instant::now();
        busy.close().await;
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        let run = tokio::time::timeout(Duration::from_secs(3), task).await.expect("the statement was stopped").unwrap();
        assert_eq!(error_code(&run.outcome).as_deref(), Some(porthole_lib::query::CANCELED_CODE));
        watch.disconnect().await.ok();
        f.driver.close().await;
    })
    .await;
}

/// A command whose future is dropped mid-statement leaves the connection in the middle of
/// the protocol, so the session counts as lost (the app opens a new one) instead of being reused.
#[tokio::test]
async fn mysql_a_dropped_run_marks_the_session_lost() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        tokio::select! {
            _ = f.session.run(HEAVY_COUNT, &[], 10) => panic!("the heavy count finished"),
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
        assert!(f.session.is_closed());
        assert!(f.session.lost_reason().is_some());
        // Closing it stops the statement still running on the server.
        f.session.close().await;
        let mut watch = common::mysql::mysql_conn(&common::mysql::mysql_profile_url(&p, &pw)).await;
        let started = Instant::now();
        loop {
            let n: Option<i64> = watch
                .exec_first(
                    "select count(*) from information_schema.PROCESSLIST where ID <> CONNECTION_ID() and INFO like concat(?, '%')",
                    (HEAVY_COUNT,),
                )
                .await
                .unwrap();
            if n == Some(0) {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(2), "the dropped statement still runs");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        watch.disconnect().await.ok();

        // A command dropped while it waits for the connection (another one holds it) never
        // touched it: the session stays usable.
        let s = f.driver.open_query_session(None).await.unwrap();
        let busy = s.clone();
        let task = tokio::spawn(async move { busy.run(HEAVY_COUNT, &[], 10).await });
        wait_until_running(&p, &pw, &[HEAVY_COUNT]).await;
        tokio::select! {
            _ = s.run_simple("select 1") => panic!("ran while another statement held the connection"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        assert!(!s.is_closed());
        s.cancel().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
        assert_eq!(rows(&s.run("select 1", &[], 10).await), &text(&[&["1"]]));
        s.close().await;
        f.driver.close().await;
    })
    .await;
}

/// A MySQL tab whose `USE` fails doesn't run its statement on the database it was on: the
/// failed switch is the run's error.
#[tokio::test]
async fn mysql_a_failed_database_switch_is_the_runs_error() {
    for_each_mysql(|p, pw| async move {
        let f = open(&p, &pw).await;
        let tab = porthole_lib::query::sessions::QueryTabSession {
            session: f.session.clone(),
            run_lock: tokio::sync::Mutex::new(()),
            search_path: tokio::sync::Mutex::new(None),
        };
        let mut req = run_req("create table r21 (i int)", &[]);
        req.schema = Some("no_such_db".into());
        let ran = runner::run_on_tab(&MYSQL, &tab, &Environment::Local, &req, &|_| {}).await;
        assert!(matches!(&ran, Err(AppError::Query(m)) if m.contains("no_such_db")), "{:?}", ran.as_ref().map(|r| &r.statements));
        assert_eq!(rows(&f.session.run("select count(*) from information_schema.TABLES where TABLE_NAME = 'r21'", &[], 10).await), &text(&[&["0"]]));
        // The session is still usable, on its database.
        assert!(!f.session.is_closed());
        assert_eq!(rows(&f.session.run("select database()", &[], 10).await), &text(&[&["fx"]]));
        f.close().await;
    })
    .await;
}
