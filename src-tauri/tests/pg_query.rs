mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use porthole_lib::driver::pg_session::{session_config, PgQuerySession};
use porthole_lib::driver::postgres::PostgresDriver;
use porthole_lib::driver::Driver;
use porthole_lib::error::AppError;
use porthole_lib::model::*;
use porthole_lib::query::sessions::{QuerySessions, QueryTabSession};
use porthole_lib::query::{Notice, QuerySession, RawOutcome, ResultColumn, StatementRun, TxStatus};
use common::{driver, for_each_pg};

async fn open(p: &ConnectionProfile, pw: &str) -> (PostgresDriver, Arc<dyn QuerySession>) {
    let d = driver(p, pw).await;
    let s = d.open_query_session(None).await.unwrap();
    (d, s)
}

async fn done(d: PostgresDriver, s: Arc<dyn QuerySession>) {
    s.close().await;
    d.close().await;
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

fn col(name: &str, data_type: &str) -> ResultColumn {
    ResultColumn { name: name.into(), data_type: data_type.into() }
}

fn error_code(run: &StatementRun) -> Option<String> {
    match &run.outcome {
        RawOutcome::Error { code, .. } => code.clone(),
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn session_select_returns_text_rows() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let run = s.run("select id, name from fx.users order by id limit 2", &[], 1000).await;
        match &run.outcome {
            RawOutcome::Rows { columns, rows, truncated, .. } => {
                assert_eq!(columns, &vec![col("id", "int4"), col("name", "text")]);
                assert_eq!(rows, &text(&[&["1", "Joanna"], &["2", "Hannah"]]));
                assert!(!truncated);
            }
            other => panic!("expected rows, got {other:?}"),
        }
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_duplicate_column_names() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let run = s.run("select 1 as a, 2 as a", &[], 1000).await;
        match &run.outcome {
            RawOutcome::Rows { columns, rows, .. } => {
                let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, vec!["a", "a"]);
                assert_eq!(rows, &text(&[&["1", "2"]]));
            }
            other => panic!("expected rows, got {other:?}"),
        }
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_null_vs_empty_and_types() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let sql = r#"select null::text, '', '{"a":1}'::jsonb, 1.50::numeric, '\xdead'::bytea, '{1,2}'::int[]"#;
        let run = s.run(sql, &[], 1000).await;
        let expected = vec![
            None,
            Some(String::new()),
            Some(r#"{"a": 1}"#.to_string()),
            Some("1.50".to_string()),
            Some(r"\xdead".to_string()),
            Some("{1,2}".to_string()),
        ];
        assert_eq!(rows(&run), &vec![expected]);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_row_cap_truncates() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let sql = "select g from generate_series(1,5000) g";
        match s.run(sql, &[], 1000).await.outcome {
            RawOutcome::Rows { rows, truncated, .. } => {
                assert_eq!(rows.len(), 1000);
                assert!(truncated);
            }
            other => panic!("expected rows, got {other:?}"),
        }
        match s.run(sql, &[], 10000).await.outcome {
            RawOutcome::Rows { rows, truncated, .. } => {
                assert_eq!(rows.len(), 5000);
                assert!(!truncated);
            }
            other => panic!("expected rows, got {other:?}"),
        }
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_commands_report_rows_affected() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let cases = [
            ("create temp table t(a int)", 0),
            ("insert into t select generate_series(1,37)", 37),
            ("update t set a = a", 37),
        ];
        for (sql, n) in cases {
            assert_eq!(s.run(sql, &[], 1000).await.outcome, RawOutcome::Command { rows_affected: n }, "{sql}");
        }
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_returning_uses_wrapper() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        s.run("create temp table t(a int)", &[], 1000).await;
        s.run("insert into t select generate_series(1,37)", &[], 1000).await;
        let run = s.run("update t set a = a + 100 where a = 1 returning a", &[], 1000).await;
        assert_eq!(rows(&run), &text(&[&["101"]]));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_unwrappable_statements_fall_back() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        s.run("create temp table t(a int)", &[], 1000).await;
        s.run("insert into t select generate_series(1,37)", &[], 1000).await;

        match s.run("explain select 1", &[], 1000).await.outcome {
            RawOutcome::Rows { columns, rows, .. } => {
                assert_eq!(columns, vec![col("QUERY PLAN", "text")]);
                assert!(!rows.is_empty());
            }
            other => panic!("expected rows, got {other:?}"),
        }
        assert_eq!(rows(&s.run("show search_path", &[], 1000).await).len(), 1);
        let run = s.run("with d as (delete from t where a = 2 returning a) select a from d", &[], 1000).await;
        assert_eq!(rows(&run), &text(&[&["2"]]));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_unwrappable_statements_keep_transaction_open() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        s.run("create temp table t(a int)", &[], 1000).await;
        s.run("insert into t select generate_series(1,37)", &[], 1000).await;
        s.run("begin", &[], 1000).await;
        assert_eq!(s.tx_status().await, TxStatus::Open);

        assert!(!rows(&s.run("explain select 1", &[], 1000).await).is_empty());
        assert_eq!(s.tx_status().await, TxStatus::Open, "after explain");
        assert_eq!(rows(&s.run("show search_path", &[], 1000).await).len(), 1);
        assert_eq!(s.tx_status().await, TxStatus::Open, "after show");
        let run = s.run("with d as (delete from t where a = 2 returning a) select a from d", &[], 1000).await;
        assert_eq!(rows(&run), &text(&[&["2"]]));
        assert_eq!(s.tx_status().await, TxStatus::Open, "after data-modifying with");
        // A wrappable statement still goes through the text wrapper.
        match s.run("select 1, null::int", &[], 1000).await.outcome {
            RawOutcome::Rows { columns, rows, .. } => {
                assert_eq!(columns[1], col("int4", "int4"));
                assert_eq!(rows, vec![vec![Some("1".to_string()), None]]);
            }
            other => panic!("expected rows, got {other:?}"),
        }
        assert_eq!(s.tx_status().await, TxStatus::Open, "after select");

        s.run("rollback", &[], 1000).await;
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_params_are_typed_by_server() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let run = s.run("select now() > $1 as later", &[Some("2000-01-01".into())], 1000).await;
        assert_eq!(rows(&run), &text(&[&["true"]]));
        let run = s.run("select g from generate_series(1,10) g limit $1", &[Some("3".into())], 1000).await;
        assert_eq!(rows(&run).len(), 3);
        let run = s.run("select $1::int is null", &[None], 1000).await;
        assert_eq!(rows(&run), &text(&[&["true"]]));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_error_has_code_and_position() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        match s.run("select 'é', nope", &[], 1000).await.outcome {
            RawOutcome::Error { message, code, position } => {
                assert!(message.contains("nope"), "{message}");
                assert_eq!(code.as_deref(), Some("42703"));
                assert_eq!(position, Some(13));
            }
            other => panic!("expected an error, got {other:?}"),
        }
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_notices_are_collected() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let run = s.run("DO $$ BEGIN RAISE NOTICE 'hello %', 42; END $$", &[], 1000).await;
        assert_eq!(run.outcome, RawOutcome::Command { rows_affected: 0 });
        assert_eq!(run.notices, vec![Notice { severity: "NOTICE".into(), message: "hello 42".into() }]);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_startup_notices_are_not_reported_on_first_run() {
    for_each_pg(|mut p, pw| async move {
        p.startup_sql = Some("DO $$ BEGIN RAISE NOTICE 'from startup'; END $$".into());
        let (d, s) = open(&p, &pw).await;
        assert_eq!(s.run("select 1", &[], 1000).await.notices, vec![]);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_cancel_stops_and_session_survives() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let started = Instant::now();
        let runner = s.clone();
        let task = tokio::spawn(async move { runner.run("select pg_sleep(30)", &[], 1000).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        s.cancel().await.unwrap();
        let run = task.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(error_code(&run).as_deref(), Some("57014"));
        assert_eq!(rows(&s.run("select 1", &[], 1000).await), &text(&[&["1"]]));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_tx_status() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        s.run("begin", &[], 1000).await;
        assert_eq!(s.tx_status().await, TxStatus::Open);
        s.run("select 1/0", &[], 1000).await;
        assert_eq!(s.tx_status().await, TxStatus::Failed);
        s.run("rollback", &[], 1000).await;
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_detects_lost_connection() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let pid = rows(&s.run("select pg_backend_pid()", &[], 1000).await)[0][0].clone().unwrap();
        common::exec(&common::url(&p, &pw), &format!("select pg_terminate_backend({pid})")).await;
        let run = s.run("select 1", &[], 1000).await;
        assert!(matches!(run.outcome, RawOutcome::Error { .. }), "{:?}", run.outcome);
        assert!(s.is_closed());
        // The connection task keeps the server's reason for the lost-connection message.
        let started = Instant::now();
        while s.lost_reason().is_none() && started.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let reason = s.lost_reason().expect("a reason for the lost connection");
        assert!(reason.contains("terminating connection"), "{reason}");
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_read_only_profile_blocks_writes() {
    for_each_pg(|mut p, pw| async move {
        p.read_only = true;
        let (d, s) = open(&p, &pw).await;
        let run = s.run("update fx.users set name = name", &[], 1000).await;
        assert_eq!(error_code(&run).as_deref(), Some("25006"));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_startup_sql_applies() {
    for_each_pg(|mut p, pw| async move {
        p.startup_sql = Some("SET search_path TO fx".into());
        let (d, s) = open(&p, &pw).await;
        assert_eq!(rows(&s.run("select count(*) from users", &[], 1000).await), &text(&[&["17"]]));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_set_search_path_handles_mixed_case() {
    for_each_pg(|mut p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        s.set_search_path(Some("Mixed Case")).await.unwrap();
        let run = s.run(r#"select count(*) > 0 from "Order Items""#, &[], 1000).await;
        assert_eq!(rows(&run), &text(&[&["true"]]));
        done(d, s).await;

        p.startup_sql = Some("SET search_path TO fx".into());
        let (d, s) = open(&p, &pw).await;
        s.set_search_path(Some("Mixed Case")).await.unwrap();
        s.set_search_path(None).await.unwrap();
        assert_eq!(rows(&s.run("show search_path", &[], 1000).await), &text(&[&["fx"]]));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn session_tls_modes_on_plain_server() {
    for_each_pg(|mut p, pw| async move {
        common::net(&mut p).ssl_mode = SslMode::Prefer;
        let (d, s) = open(&p, &pw).await;
        assert_eq!(rows(&s.run("select 1", &[], 1000).await), &text(&[&["1"]]));
        done(d, s).await;

        // The pool's own probe already refuses `require` on a plain server, so open
        // the session directly from a `require` profile.
        common::net(&mut p).ssl_mode = SslMode::Require;
        let cfg = session_config(&p, Some(pw.clone())).unwrap();
        let pool = sqlx::PgPool::connect_lazy(&common::url(&p, &pw)).unwrap();
        match PgQuerySession::open(&cfg, pool.clone()).await {
            Err(AppError::Connection(_)) => {}
            Err(other) => panic!("expected Connection, got {other:?}"),
            Ok(_) => panic!("require connected to a server without TLS"),
        }
        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn schema_columns_lists_tables_and_views() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let cols = d.schema_columns(None, "fx").await.unwrap();
        assert_eq!(cols["users"][..4], ["id", "name", "email", "status"]);
        assert_eq!(cols["active_users"], vec!["id", "name"]);
        d.close().await;
    })
    .await;
}

fn run_req(sql: &str) -> porthole_lib::query::RunRequest {
    porthole_lib::query::RunRequest {
        sql: sql.into(),
        target: porthole_lib::query::RunTarget::All,
        params: ParamValues::new(),
        schema: None,
        confirmed: false,
        fetch_all: false,
        saved_query_id: None,
        database: None,
    }
}

async fn run_all(s: &Arc<dyn QuerySession>, sql: &str) -> porthole_lib::query::RunReport {
    porthole_lib::query::runner::run(s.as_ref(), &Environment::Local, &run_req(sql), &|_| {}).await.unwrap()
}

#[tokio::test]
async fn pg_run_all_mixed_script() {
    use porthole_lib::query::Outcome;
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let report = run_all(&s, "create temp table t(a int); insert into t values (1); select a from t;").await;
        let outcomes: Vec<&Outcome> = report.statements.iter().map(|r| &r.outcome).collect();
        assert_eq!(outcomes[0], &Outcome::Done);
        assert_eq!(outcomes[1], &Outcome::Affected { rows: 1 });
        match outcomes[2] {
            Outcome::Rows { rows, .. } => assert_eq!(rows, &text(&[&["1"]])),
            other => panic!("{other:?}"),
        }
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn pg_run_all_commits_before_error_and_begin_holds() {
    use porthole_lib::query::Outcome;
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let report = run_all(
            &s,
            "create table fx.r(a int); insert into fx.r values (1); select 1/0; insert into fx.r values (2)",
        )
        .await;
        assert!(matches!(report.statements[0].outcome, Outcome::Done));
        assert!(matches!(report.statements[1].outcome, Outcome::Affected { rows: 1 }));
        assert!(matches!(report.statements[2].outcome, Outcome::Error { .. }));
        assert!(matches!(report.statements[3].outcome, Outcome::Skipped));
        let fresh = d.open_query_session(None).await.unwrap();
        assert_eq!(rows(&fresh.run("select count(*) from fx.r", &[], 10).await), &text(&[&["1"]]));

        let report = run_all(&s, "begin; insert into fx.r values (3); select 1/0").await;
        match report.outcome {
            porthole_lib::query::RunOutcome::Completed { tx_status, .. } => assert_eq!(tx_status, TxStatus::Failed),
            other => panic!("{other:?}"),
        }
        run_all(&s, "rollback").await;
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        assert_eq!(rows(&fresh.run("select count(*) from fx.r", &[], 10).await), &text(&[&["1"]]));
        fresh.close().await;
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn pg_commit_in_failed_transaction_reports_rollback() {
    use porthole_lib::query::Outcome;
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        run_all(&s, "begin; select 1/0").await;
        assert_eq!(s.tx_status().await, TxStatus::Failed);
        let report = run_all(&s, "commit").await;
        assert_eq!(
            report.statements[0].outcome,
            Outcome::Error {
                message: "The transaction had failed, so COMMIT rolled it back. Nothing was committed.".into(),
                code: None,
                position: None,
            }
        );
        match report.outcome {
            porthole_lib::query::RunOutcome::Completed { tx_status, .. } => assert_eq!(tx_status, TxStatus::Idle),
            other => panic!("{other:?}"),
        }
        done(d, s).await;
    })
    .await;
}

fn tab_for(s: &Arc<dyn QuerySession>) -> porthole_lib::query::sessions::QueryTabSession {
    porthole_lib::query::sessions::QueryTabSession {
        session: s.clone(),
        run_lock: tokio::sync::Mutex::new(()),
        search_path: tokio::sync::Mutex::new(None),
    }
}

#[tokio::test]
async fn pg_schema_is_reapplied_after_rollback() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let tab = tab_for(&s);
        tab.apply_schema(Some("fx")).await;
        run_all(&s, "begin").await;
        tab.apply_schema(Some("Mixed Case")).await; // undone by the rollback below
        run_all(&s, "rollback").await;
        tab.apply_schema(Some("Mixed Case")).await;
        let count = r#"select count(*) > 0 from "Order Items""#;
        assert_eq!(rows(&s.run(count, &[], 10).await), &text(&[&["true"]]));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn pg_rollback_works_in_failed_tx_with_schema_selected() {
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        let tab = tab_for(&s);
        tab.apply_schema(Some("Mixed Case")).await;
        run_all(&s, "begin; select 1/0").await;
        assert_eq!(s.tx_status().await, TxStatus::Failed);
        // The command's path: apply_schema (its SET fails with 25P02, only logged), then run.
        tab.apply_schema(Some("Mixed Case")).await;
        run_all(&s, "rollback").await;
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn pg_end_tab_transaction_never_reports_a_failed_commit() {
    use porthole_lib::query::{end_tab_transaction, COMMIT_IN_FAILED_TX};
    for_each_pg(|p, pw| async move {
        let (d, s) = open(&p, &pw).await;
        s.run("begin", &[], 10).await;
        s.run("select 1/0", &[], 10).await;
        let res = end_tab_transaction(&*s, true).await;
        assert_eq!(res, Err(porthole_lib::error::AppError::Query(COMMIT_IN_FAILED_TX.into())));
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        assert_eq!(rows(&s.run("select 1", &[], 10).await), &text(&[&["1"]]));

        s.run("begin", &[], 10).await;
        s.run("create temp table t(a int)", &[], 10).await;
        assert_eq!(end_tab_transaction(&*s, true).await, Ok(TxStatus::Idle));
        done(d, s).await;
    })
    .await;
}

const OTHER_DB: &str = "app_test_other";
const SWITCH_REFUSED: &str = "Commit or roll back this tab's transaction before switching databases.";

/// A registry tab on `database` (`None` = the profile's own).
async fn tab_on(
    reg: &QuerySessions,
    d: &PostgresDriver,
    database: Option<&str>,
) -> porthole_lib::error::AppResult<Arc<QueryTabSession>> {
    reg.get_or_open("w", "t", database, d.open_query_session(database)).await
}

async fn current_database(s: &Arc<dyn QuerySession>) -> String {
    rows(&s.run("select current_database()", &[], 10).await)[0][0].clone().unwrap()
}

#[tokio::test]
async fn list_databases_includes_both() {
    for_each_pg(|p, pw| async move {
        common::ensure_database(&common::url(&p, &pw), OTHER_DB).await;
        let d = driver(&p, &pw).await;
        let dbs = d.list_databases().await.unwrap();
        assert!(dbs.contains(&p.network().unwrap().database) && dbs.contains(&OTHER_DB.to_string()), "{dbs:?}");
        assert!(!dbs.iter().any(|n| n == "template0" || n == "template1"), "{dbs:?}");
        let mut sorted = dbs.clone();
        sorted.sort();
        assert_eq!(dbs, sorted);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn other_database_session_runs_there() {
    for_each_pg(|p, pw| async move {
        common::ensure_database(&common::url(&p, &pw), OTHER_DB).await;
        let d = driver(&p, &pw).await;
        let reg = QuerySessions::default();
        let tab = tab_on(&reg, &d, Some(OTHER_DB)).await.unwrap();
        assert_eq!(current_database(&tab.session).await, OTHER_DB);
        assert_eq!(rows(&tab.session.run("select label from other_t", &[], 10).await), &text(&[&["one"]]));
        reg.close_window("w").await;
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn other_database_keeps_read_only_and_startup_sql() {
    for_each_pg(|mut p, pw| async move {
        common::ensure_database(&common::url(&p, &pw), OTHER_DB).await;
        p.read_only = true;
        let d = driver(&p, &pw).await;
        let s = d.open_query_session(Some(OTHER_DB)).await.unwrap();
        assert_eq!(current_database(&s).await, OTHER_DB);
        let run = s.run("INSERT INTO other_t VALUES (2,'x')", &[], 10).await;
        assert_eq!(error_code(&run).as_deref(), Some("25006"), "{:?}", run.outcome);
        done(d, s).await;

        p.read_only = false;
        p.startup_sql = Some("SET application_name = 'ts-check'".into());
        let d = driver(&p, &pw).await;
        let s = d.open_query_session(Some(OTHER_DB)).await.unwrap();
        assert_eq!(current_database(&s).await, OTHER_DB);
        assert_eq!(rows(&s.run("show application_name", &[], 10).await), &text(&[&["ts-check"]]));
        done(d, s).await;

        // The catalog connection to the other database runs the startup SQL too: this one
        // only works on app_test, so the lookup fails there.
        p.startup_sql = Some("SELECT 1 FROM fx.users LIMIT 1".into());
        let d = driver(&p, &pw).await;
        assert!(d.list_schemas(None).await.is_ok());
        let err = d.list_schemas(Some(OTHER_DB)).await.unwrap_err();
        assert!(err.message().contains("fx.users"), "{err:?}");
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn other_database_switch_refused_with_open_tx() {
    for_each_pg(|p, pw| async move {
        common::ensure_database(&common::url(&p, &pw), OTHER_DB).await;
        let d = driver(&p, &pw).await;
        let reg = QuerySessions::default();
        let first = tab_on(&reg, &d, None).await.unwrap();
        first.session.run("begin", &[], 10).await;
        let err = tab_on(&reg, &d, Some(OTHER_DB)).await.err();
        assert_eq!(err, Some(AppError::Query(SWITCH_REFUSED.into())));
        assert!(!first.session.is_closed());
        assert_eq!(first.session.tx_status().await, TxStatus::Open);
        // A failed transaction is still a transaction.
        first.session.run("select 1/0", &[], 10).await;
        assert_eq!(tab_on(&reg, &d, Some(OTHER_DB)).await.err(), Some(AppError::Query(SWITCH_REFUSED.into())));
        first.session.run("rollback", &[], 10).await;
        let other = tab_on(&reg, &d, Some(OTHER_DB)).await.unwrap();
        assert!(first.session.is_closed());
        assert_eq!(current_database(&other.session).await, OTHER_DB);
        reg.close_window("w").await;
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn other_database_switch_refused_with_open_tx_under_set_role() {
    for_each_pg(|mut p, pw| async move {
        common::ensure_database(&common::url(&p, &pw), OTHER_DB).await;
        p.startup_sql = Some("SET ROLE ts_limited".into());
        let d = driver(&p, &pw).await;
        let reg = QuerySessions::default();
        let first = tab_on(&reg, &d, None).await.unwrap();
        first.session.run("begin", &[], 10).await;
        // pg_stat_activity hides the state from ts_limited, so tx_status can't tell...
        assert_eq!(first.session.tx_status().await, TxStatus::Unknown);
        // ...and the switch is still refused.
        assert_eq!(tab_on(&reg, &d, Some(OTHER_DB)).await.err(), Some(AppError::Query(SWITCH_REFUSED.into())));
        first.session.run("rollback", &[], 10).await;
        let other = tab_on(&reg, &d, Some(OTHER_DB)).await.unwrap();
        assert_eq!(current_database(&other.session).await, OTHER_DB);
        reg.close_window("w").await;
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn other_database_catalog() {
    for_each_pg(|p, pw| async move {
        common::ensure_database(&common::url(&p, &pw), OTHER_DB).await;
        let d = driver(&p, &pw).await;
        let schemas = d.list_schemas(Some(OTHER_DB)).await.unwrap();
        assert!(schemas.contains(&"public".to_string()), "{schemas:?}");
        assert!(!schemas.contains(&"fx".to_string()), "{schemas:?}");
        let cols = d.schema_columns(Some(OTHER_DB), "public").await.unwrap();
        assert_eq!(cols["other_t"], vec!["id", "label"]);
        // The profile's own database, named, is the same as None.
        assert!(d.schema_columns(Some(&p.network().unwrap().database), "fx").await.unwrap().contains_key("users"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn unknown_database_errors_and_switch_back_works() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let reg = QuerySessions::default();
        tab_on(&reg, &d, None).await.unwrap();
        let err = tab_on(&reg, &d, Some("nope_db")).await.err().expect("nope_db does not exist");
        assert!(err.message().contains("nope_db"), "{err:?}");
        let err = d.list_schemas(Some("nope_db")).await.unwrap_err();
        assert!(err.message().contains("nope_db"), "{err:?}");
        let err = d.schema_columns(Some("nope_db"), "public").await.unwrap_err();
        assert!(err.message().contains("nope_db"), "{err:?}");
        let back = tab_on(&reg, &d, None).await.unwrap();
        assert_eq!(current_database(&back.session).await, p.network().unwrap().database);
        reg.close_window("w").await;
        d.close().await;
    })
    .await;
}
