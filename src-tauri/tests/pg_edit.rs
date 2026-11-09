mod common;

use std::collections::BTreeMap;

use porthole_lib::driver::Driver;
use porthole_lib::error::AppError;
use porthole_lib::model::*;
use common::{driver, exec, for_each_pg, url};
use sqlx::{AssertSqlSafe, Connection, PgConnection, Row};

fn tref(schema: &str, name: &str) -> TableRef {
    TableRef { schema: schema.into(), name: name.into() }
}

fn vals(pairs: &[(&str, Option<&str>)]) -> CellValues {
    pairs.iter().map(|(k, v)| (k.to_string(), v.map(str::to_string))).collect()
}

fn keys(k: &[&str]) -> Vec<String> {
    k.iter().map(|s| s.to_string()).collect()
}

fn set(table: TableRef, changes: Vec<RowChange>) -> ChangeSet {
    ChangeSet { table, changes }
}

fn upd(key: &[&str], values: &[(&str, Option<&str>)]) -> RowChange {
    RowChange::Update { key: keys(key), values: vals(values), original_types: BTreeMap::new() }
}

fn msg(r: Result<ApplyResult, AppError>) -> String {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.message().to_string(),
    }
}

/// Reads one text value (or NULL) on a plain connection.
async fn read(p: &ConnectionProfile, pw: &str, sql: &str) -> Option<String> {
    let mut c = PgConnection::connect(&url(p, pw)).await.unwrap();
    let row = sqlx::query(AssertSqlSafe(sql.to_string())).fetch_one(&mut c).await.unwrap();
    row.get::<Option<String>, _>(0)
}

#[tokio::test]
async fn apply_round_trips_types() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let r = d
            .apply_changes(&set(
                tref("fx", "typed"),
                vec![upd(
                    &["1"],
                    &[
                        ("ts", Some("2026-02-03 04:05:06+00")),
                        ("j", Some("{\"b\": 2}")),
                        ("b", Some("\\xbeef")),
                        ("arr", Some("{3,4}")),
                        ("n", Some("2.25")),
                        ("u", Some("00000000-0000-0000-0000-000000000002")),
                        ("flag", Some("false")),
                        ("c", Some("xyz")),
                        ("bits", Some("110")),
                    ],
                )],
            ))
            .await
            .unwrap();
        assert_eq!(r.applied, 1);
        for (col, want) in [
            ("ts", "2026-02-03 04:05:06+00"),
            ("j", "{\"b\": 2}"),
            ("b", "\\xbeef"),
            ("arr", "{3,4}"),
            ("n", "2.25"),
            ("u", "00000000-0000-0000-0000-000000000002"),
            ("flag", "false"),
            ("c", "xyz       "),
            ("bits", "110"),
            ("note", "x"),
        ] {
            // `char(n)::text` strips the padding, so read it in its own type.
            let expr = if col == "c" { "c".to_string() } else { format!("{col}::text") };
            let got = read(&p, &pw, &format!("SELECT {expr} FROM fx.typed WHERE id = 1")).await;
            assert_eq!(got.as_deref(), Some(want), "column {col}");
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_null_vs_empty() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        d.apply_changes(&set(tref("fx", "typed"), vec![upd(&["1"], &[("note", None)])])).await.unwrap();
        assert_eq!(read(&p, &pw, "SELECT (note IS NULL)::text FROM fx.typed WHERE id = 1").await.as_deref(), Some("true"));
        d.apply_changes(&set(tref("fx", "typed"), vec![upd(&["1"], &[("note", Some(""))])])).await.unwrap();
        assert_eq!(read(&p, &pw, "SELECT (note = '')::text FROM fx.typed WHERE id = 1").await.as_deref(), Some("true"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_varchar_overflow_errors() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let m = msg(d.apply_changes(&set(tref("fx", "typed"), vec![upd(&["1"], &[("v", Some("abcdefg"))])])).await);
        assert!(m.contains("UPDATE id = 1:"), "{m}");
        assert!(m.contains("value too long"), "{m}");
        assert_eq!(read(&p, &pw, "SELECT v FROM fx.typed WHERE id = 1").await.as_deref(), Some("abc"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_composite_and_awkward_identifiers() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let r = d
            .apply_changes(&set(
                tref("fx", "pair"),
                vec![
                    upd(&["1", "it's"], &[("val", Some("uno"))]),
                    RowChange::Delete { key: keys(&["2", "b"]), original_types: BTreeMap::new() },
                ],
            ))
            .await
            .unwrap();
        assert_eq!(r.applied, 2);
        assert_eq!(read(&p, &pw, "SELECT val FROM fx.pair WHERE \"select\" = 1").await.as_deref(), Some("uno"));
        assert_eq!(read(&p, &pw, "SELECT count(*)::text FROM fx.pair").await.as_deref(), Some("1"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_insert_and_identity() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        d.apply_changes(&set(tref("fx", "gen"), vec![RowChange::Insert { values: vals(&[("name", Some("second"))]) }]))
            .await
            .unwrap();
        assert_eq!(read(&p, &pw, "SELECT id::text FROM fx.gen WHERE name = 'second'").await.as_deref(), Some("2"));
        let m = msg(
            d.apply_changes(&set(
                tref("fx", "gen"),
                vec![RowChange::Insert { values: vals(&[("id", Some("5")), ("name", Some("x"))]) }],
            ))
            .await,
        );
        assert_eq!(m, "id can't be edited");
        assert_eq!(read(&p, &pw, "SELECT count(*)::text FROM fx.gen").await.as_deref(), Some("2"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_is_all_or_nothing() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let m = msg(
            d.apply_changes(&set(
                tref("fx", "users"),
                vec![
                    upd(&["1"], &[("name", Some("Changed"))]),
                    upd(&["2"], &[("name", None)]),
                    RowChange::Delete { key: keys(&["3"]), original_types: BTreeMap::new() },
                ],
            ))
            .await,
        );
        assert!(m.starts_with("UPDATE id = 2:"), "{m}");
        assert_eq!(read(&p, &pw, "SELECT name FROM fx.users WHERE id = 1").await.as_deref(), Some("Joanna"));
        assert_eq!(read(&p, &pw, "SELECT name FROM fx.users WHERE id = 2").await.as_deref(), Some("Hannah"));
        assert_eq!(read(&p, &pw, "SELECT count(*)::text FROM fx.users WHERE id = 3").await.as_deref(), Some("1"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_row_gone_rolls_back() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        exec(&url(&p, &pw), "DELETE FROM fx.pair WHERE \"select\" = 2").await;
        let m = msg(
            d.apply_changes(&set(
                tref("fx", "pair"),
                vec![upd(&["1", "it's"], &[("val", Some("x"))]), upd(&["2", "b"], &[("val", Some("y"))])],
            ))
            .await,
        );
        assert_eq!(m, "The row with select = 2, Key A = b was changed or deleted by someone else.");
        assert_eq!(read(&p, &pw, "SELECT val FROM fx.pair WHERE \"select\" = 1").await.as_deref(), Some("one"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_hostile_value_is_data() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let evil = "x'); drop table fx.users; --";
        d.apply_changes(&set(tref("fx", "users"), vec![upd(&["1"], &[("name", Some(evil))])])).await.unwrap();
        assert_eq!(read(&p, &pw, "SELECT name FROM fx.users WHERE id = 1").await.as_deref(), Some(evil));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_on_read_only_connection_fails() {
    for_each_pg(|mut p, pw| async move {
        p.read_only = true;
        let d = driver(&p, &pw).await;
        let m = msg(d.apply_changes(&set(tref("fx", "users"), vec![upd(&["1"], &[("name", Some("Z"))])])).await);
        assert!(m.contains("read-only transaction"), "{m}");
        assert_eq!(read(&p, &pw, "SELECT name FROM fx.users WHERE id = 1").await.as_deref(), Some("Joanna"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_to_view_is_refused() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let m = msg(d.apply_changes(&set(tref("fx", "active_users"), vec![upd(&["1"], &[("name", Some("Z"))])])).await);
        assert_eq!(m, "This table can't be edited.");
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_waits_at_most_for_the_lock_timeout() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        // Another session (a query tab with an open transaction) holds the row.
        let mut other = PgConnection::connect(&url(&p, &pw)).await.unwrap();
        sqlx::raw_sql("BEGIN; SELECT * FROM fx.users WHERE id = 1 FOR UPDATE").execute(&mut other).await.unwrap();
        let started = std::time::Instant::now();
        let r = d.apply_changes(&set(tref("fx", "users"), vec![upd(&["1"], &[("name", Some("Locked"))])])).await;
        let waited = started.elapsed();
        sqlx::raw_sql("ROLLBACK").execute(&mut other).await.unwrap();
        other.close().await.unwrap();
        assert_eq!(msg(r), "Another session is holding a lock on this row (an open transaction in a query tab?).");
        assert!(waited < std::time::Duration::from_secs(10), "waited {waited:?}");
        assert_eq!(read(&p, &pw, "SELECT name FROM fx.users WHERE id = 1").await.as_deref(), Some("Joanna"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_deletes_before_inserting_the_same_key() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let r = d
            .apply_changes(&set(
                tref("fx", "pair"),
                vec![
                    RowChange::Delete { key: keys(&["2", "b"]), original_types: BTreeMap::new() },
                    RowChange::Insert { values: vals(&[("select", Some("2")), ("Key A", Some("b"))]) },
                ],
            ))
            .await
            .unwrap();
        assert_eq!(r.applied, 2);
        assert_eq!(
            read(&p, &pw, "SELECT coalesce(val, 'NULL') FROM fx.pair WHERE \"select\" = 2 AND \"Key A\" = 'b'").await.as_deref(),
            Some("NULL")
        );
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_edits_the_key_cell_itself() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        d.apply_changes(&set(tref("fx", "pair"), vec![upd(&["1", "it's"], &[("select", Some("9"))])])).await.unwrap();
        assert_eq!(
            read(&p, &pw, "SELECT val FROM fx.pair WHERE \"select\" = 9 AND \"Key A\" = 'it''s'").await.as_deref(),
            Some("one")
        );
        assert_eq!(read(&p, &pw, "SELECT count(*)::text FROM fx.pair WHERE \"select\" = 1").await.as_deref(), Some("0"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_delete_of_a_gone_row_rolls_back() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        exec(&url(&p, &pw), "DELETE FROM fx.pair WHERE \"select\" = 2").await;
        let m = msg(
            d.apply_changes(&set(
                tref("fx", "pair"),
                vec![RowChange::Delete { key: keys(&["1", "it's"]), original_types: BTreeMap::new() }, RowChange::Delete { key: keys(&["2", "b"]), original_types: BTreeMap::new() }],
            ))
            .await,
        );
        assert_eq!(m, "The row with select = 2, Key A = b was changed or deleted by someone else.");
        assert_eq!(read(&p, &pw, "SELECT count(*)::text FROM fx.pair WHERE \"select\" = 1").await.as_deref(), Some("1"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_commit_refused_by_the_server_is_a_definite_failure() {
    for_each_pg(|p, pw| async move {
        // A deferred foreign key is only checked at COMMIT; the server answers, so nothing was applied.
        exec(
            &url(&p, &pw),
            "CREATE TABLE fx.deferred (id int PRIMARY KEY, parent int REFERENCES fx.deferred (id) DEFERRABLE INITIALLY DEFERRED)",
        )
        .await;
        let d = driver(&p, &pw).await;
        let r = d
            .apply_changes(&set(
                tref("fx", "deferred"),
                vec![RowChange::Insert { values: vals(&[("id", Some("1")), ("parent", Some("99"))]) }],
            ))
            .await;
        match r {
            Err(AppError::Query(m)) => assert!(m.contains("foreign key"), "{m}"),
            other => panic!("expected a Query error, got {other:?}"),
        }
        assert_eq!(read(&p, &pw, "SELECT count(*)::text FROM fx.deferred").await.as_deref(), Some("0"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn apply_stops_a_statement_at_the_save_timeout() {
    for_each_pg(|p, pw| async move {
        // A trigger that outlasts the (lowered) statement limit; no lock is involved.
        exec(
            &url(&p, &pw),
            "CREATE TABLE fx.slow (id int PRIMARY KEY, name text);
             INSERT INTO fx.slow VALUES (1, 'before');
             CREATE FUNCTION fx.nap() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$;
             CREATE TRIGGER nap BEFORE UPDATE ON fx.slow FOR EACH ROW EXECUTE PROCEDURE fx.nap();",
        )
        .await;
        let mut d = driver(&p, &pw).await;
        d.set_save_statement_timeout_ms(300);
        let started = std::time::Instant::now();
        let r = d.apply_changes(&set(tref("fx", "slow"), vec![upd(&["1"], &[("name", Some("after"))])])).await;
        let waited = started.elapsed();
        assert_eq!(msg(r), "The save took longer than 30 s and was canceled.");
        assert!(waited < std::time::Duration::from_secs(3), "waited {waited:?}");
        assert_eq!(read(&p, &pw, "SELECT name FROM fx.slow WHERE id = 1").await.as_deref(), Some("before"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn commit_status_asks_the_server_what_became_of_a_transaction() {
    use porthole_lib::driver::postgres::CommitStatus;
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let mut c = PgConnection::connect(&url(&p, &pw)).await.unwrap();
        const MARK: &str = "SELECT txid_current(), pg_postmaster_start_time()::text";

        sqlx::raw_sql("BEGIN").execute(&mut c).await.unwrap();
        let (committed, started): (i64, String) = sqlx::query_as(MARK).fetch_one(&mut c).await.unwrap();
        sqlx::raw_sql("COMMIT").execute(&mut c).await.unwrap();
        assert_eq!(d.commit_status(committed, &started).await, CommitStatus::Committed);

        sqlx::raw_sql("BEGIN").execute(&mut c).await.unwrap();
        let (aborted, _): (i64, String) = sqlx::query_as(MARK).fetch_one(&mut c).await.unwrap();
        sqlx::raw_sql("ROLLBACK").execute(&mut c).await.unwrap();
        assert_eq!(d.commit_status(aborted, &started).await, CommitStatus::Aborted);

        // An id the server has not handed out yet is an error there: the outcome stays unknown.
        assert_eq!(d.commit_status(committed + 1_000_000, &started).await, CommitStatus::Unknown);
        // A server that restarted since (another start time) can't vouch for the id.
        assert_eq!(d.commit_status(committed, "1999-01-01 00:00:00+00").await, CommitStatus::Unknown);
        c.close().await.unwrap();
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn commit_status_waits_while_the_transaction_is_still_in_progress() {
    use porthole_lib::driver::postgres::CommitStatus;
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let mut c = PgConnection::connect(&url(&p, &pw)).await.unwrap();
        sqlx::raw_sql("BEGIN").execute(&mut c).await.unwrap();
        let (xid, started): (i64, String) =
            sqlx::query_as("SELECT txid_current(), pg_postmaster_start_time()::text").fetch_one(&mut c).await.unwrap();
        // Still open when the check starts; it commits a second later, within the 5 s budget.
        let committer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            sqlx::raw_sql("COMMIT").execute(&mut c).await.unwrap();
            c.close().await.unwrap();
        });
        assert_eq!(d.commit_status(xid, &started).await, CommitStatus::Committed);
        committer.await.unwrap();
        d.close().await;
    })
    .await;
}
