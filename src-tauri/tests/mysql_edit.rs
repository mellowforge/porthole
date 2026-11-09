//! MySQL / MariaDB grid saves: one bounded transaction, all or nothing, on every server in
//! `TEST_MYSQL_URLS`.

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use porthole_lib::driver::mysql::MysqlDialect;
use porthole_lib::driver::Driver;
use porthole_lib::error::{AppError, AppResult};
use porthole_lib::model::*;
use porthole_lib::query::RawOutcome;
use common::mysql::{for_each_mysql, mysql_driver, mysql_one};

const LOCKED: &str = "Another session is holding a lock on this row (an open transaction in a query tab?).";

fn fx(name: &str) -> TableRef {
    TableRef { schema: "fx".into(), name: name.into() }
}

fn set(table: &str, changes: Vec<RowChange>) -> ChangeSet {
    ChangeSet { table: fx(table), changes }
}

fn values(pairs: &[(&str, Option<&str>)]) -> CellValues {
    pairs.iter().map(|(k, v)| (k.to_string(), v.map(str::to_string))).collect()
}

fn types(pairs: &[(&str, CellType)]) -> BTreeMap<String, CellType> {
    pairs.iter().map(|(k, t)| (k.to_string(), *t)).collect()
}

fn update(key: &str, vals: &[(&str, Option<&str>)], original: &[(&str, CellType)]) -> RowChange {
    RowChange::Update { key: vec![key.into()], values: values(vals), original_types: types(original) }
}

fn delete(key: &str, original: &[(&str, CellType)]) -> RowChange {
    RowChange::Delete { key: vec![key.into()], original_types: types(original) }
}

fn insert(vals: &[(&str, Option<&str>)]) -> RowChange {
    RowChange::Insert { values: values(vals) }
}

fn msg(r: AppResult<ApplyResult>) -> String {
    match r {
        Err(AppError::Query(m)) => m,
        other => panic!("expected a query error, got {other:?}"),
    }
}

const ID: (&str, CellType) = ("id", CellType::Integer);
const NAME: (&str, CellType) = ("name", CellType::Text);

fn rename(id: &str, name: Option<&str>) -> RowChange {
    update(id, &[("name", name)], &[ID, NAME])
}

#[tokio::test]
async fn mysql_update_delete_insert_round_trip() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        // User 2 has an order, and `orders.user_id` references `users`: the deleted user is 16,
        // who has none.
        let changes = set("users", vec![rename("1", Some("Joanna B")), delete("16", &[ID]), insert(&[("name", Some("New"))])]);
        assert_eq!(d.apply_changes(&changes).await.unwrap(), ApplyResult { applied: 3 });

        assert_eq!(mysql_one(&p, &pw, "select name from fx.users where id = 1").await.as_deref(), Some("Joanna B"));
        assert_eq!(mysql_one(&p, &pw, "select count(*) from fx.users where id = 16").await.as_deref(), Some("0"));
        let new_id: i64 = mysql_one(&p, &pw, "select id from fx.users where name = 'New'").await.unwrap().parse().unwrap();
        assert!(new_id > 17, "{new_id}");
        // The grid sees the same.
        let page = d
            .fetch_page(&PageRequest { table: fx("users"), filter: None, sort: None, page: 0, page_size: 100 })
            .await
            .unwrap();
        assert_eq!(page.rows.len(), 17);
        assert_eq!(page.rows[0][1].as_deref(), Some("Joanna B"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_insert_default_values() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let structure = d.table_structure(&fx("gen")).await.unwrap();
        let planned = porthole_lib::edit::plan_changes(&MysqlDialect, &structure, &set("gen", vec![insert(&[])])).unwrap();
        assert_eq!(planned[0].sql, "INSERT INTO `fx`.`gen` () VALUES ()");
        assert_eq!(d.apply_changes(&set("gen", vec![insert(&[])])).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(mysql_one(&p, &pw, "select concat(count(*), ':', max(a), ',', max(b)) from fx.gen").await.as_deref(), Some("1:3,6"));
        d.close().await;
    })
    .await;
}

/// An UPDATE that changes nothing still counts its row (`CLIENT_FOUND_ROWS`), so
/// the save isn't taken for a row changed by someone else.
#[tokio::test]
async fn mysql_saving_an_unchanged_value_succeeds() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let same = set("users", vec![rename("1", Some("Joanna"))]);
        assert_eq!(d.apply_changes(&same).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(mysql_one(&p, &pw, "select name from fx.users where id = 1").await.as_deref(), Some("Joanna"));
        d.close().await;
    })
    .await;
}

/// Keys above 2^53 bind as integers. Bound as text, the server would compare
/// them as doubles, and 9007199254740993 would hit 9007199254740992 too.
#[tokio::test]
async fn mysql_big_integer_keys_hit_one_row() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let rows = || mysql_one(&p, &pw, "select group_concat(id, ':', v order by id) from fx.bigkey");
        let original = [ID, ("v", CellType::Text)];
        let up = set("bigkey", vec![update("9007199254740993", &[("v", Some("changed"))], &original)]);
        assert_eq!(d.apply_changes(&up).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(rows().await.as_deref(), Some("9007199254740992:a,9007199254740993:changed"));

        let del = set("bigkey", vec![delete("9007199254740993", &[ID])]);
        assert_eq!(d.apply_changes(&del).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(rows().await.as_deref(), Some("9007199254740992:a"));
        d.close().await;
    })
    .await;
}

/// A key in a text column binds as text, even when it reads as a number. Bound as the
/// integer 5, `WHERE k = 5` would compare as numbers: it would hit `05` too, and fail on `x`.
#[tokio::test]
async fn mysql_text_keys_that_read_as_numbers_hit_one_row() {
    for_each_mysql(|p, pw| async move {
        common::mysql::mysql_create_skus(&p, &pw).await;
        let d = mysql_driver(&p, &pw).await;
        let rows = || mysql_one(&p, &pw, "select group_concat(k, ':', v order by k) from fx.skus");
        let original = [("k", CellType::Text), ("v", CellType::Text)];
        let key = |k: &str| vec![k.to_string()];
        let up = set("skus", vec![RowChange::Update { key: key("5"), values: values(&[("v", Some("FIVE"))]), original_types: types(&original) }]);
        assert_eq!(d.apply_changes(&up).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(rows().await.as_deref(), Some("05:oh-five,5:FIVE,x:ex"));

        let del = set("skus", vec![RowChange::Delete { key: key("5"), original_types: types(&original) }]);
        assert_eq!(d.apply_changes(&del).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(rows().await.as_deref(), Some("05:oh-five,x:ex"));
        // A number typed into a text cell is stored as that text.
        let typed = set("skus", vec![update("05", &[("v", Some("7"))], &original)]);
        assert_eq!(d.apply_changes(&typed).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(rows().await.as_deref(), Some("05:7,x:ex"));
        d.close().await;
    })
    .await;
}

/// A binary key comes from the grid as `\x` and hex; the UI says so with its cell type, and the
/// key binds as the bytes. The preview shows the hex literal that matches what runs.
#[tokio::test]
async fn mysql_binary_key_rows_can_be_edited() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let change = set("binkey", vec![update("\\x0002", &[("v", Some("TWO"))], &[("id", CellType::Blob), ("v", CellType::Text)])]);
        let structure = d.table_structure(&fx("binkey")).await.unwrap();
        let planned = porthole_lib::edit::plan_changes(&MysqlDialect, &structure, &change).unwrap();
        assert_eq!(planned[0].sql, "UPDATE `fx`.`binkey` SET `v` = ? WHERE `id` = UNHEX(SUBSTRING(?, 3))");
        assert_eq!(planned[0].preview, "UPDATE `fx`.`binkey` SET `v` = 'TWO' WHERE `id` = X'0002';");

        assert_eq!(d.apply_changes(&change).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(mysql_one(&p, &pw, "select group_concat(v order by id) from fx.binkey").await.as_deref(), Some("one,TWO"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_save_is_all_or_nothing() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        // `users.name` is NOT NULL: the second change fails, and the first is rolled back.
        let changes = set("users", vec![rename("1", Some("Changed")), rename("3", None)]);
        let m = msg(d.apply_changes(&changes).await);
        assert!(m.starts_with("UPDATE id = 3: "), "{m}");
        assert_eq!(mysql_one(&p, &pw, "select name from fx.users where id = 1").await.as_deref(), Some("Joanna"));
        // The pooled connection is clean: the next save works.
        assert_eq!(d.apply_changes(&set("users", vec![rename("1", Some("After"))])).await.unwrap(), ApplyResult { applied: 1 });
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_save_while_a_tab_holds_a_lock_reports_locked() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let s = d.open_query_session(None).await.unwrap();
        for sql in ["begin", "update users set plan = plan where id = 1"] {
            let run = s.run(sql, &[], 10).await;
            assert!(!matches!(run.outcome, RawOutcome::Error { .. }), "{sql}: {:?}", run.outcome);
        }

        let save = set("users", vec![rename("1", Some("Locked"))]);
        let started = Instant::now();
        let m = msg(d.apply_changes(&save).await);
        let waited = started.elapsed();
        assert_eq!(m, LOCKED);
        assert!(waited <= Duration::from_secs(7), "{waited:?}");
        assert_eq!(mysql_one(&p, &pw, "select name from fx.users where id = 1").await.as_deref(), Some("Joanna"));

        let run = s.run("rollback", &[], 10).await;
        assert!(!matches!(run.outcome, RawOutcome::Error { .. }), "{:?}", run.outcome);
        assert_eq!(d.apply_changes(&save).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(mysql_one(&p, &pw, "select name from fx.users where id = 1").await.as_deref(), Some("Locked"));
        s.close().await;
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_read_only_save_fails() {
    for_each_mysql(|mut p, pw| async move {
        p.read_only = true;
        let d = mysql_driver(&p, &pw).await;
        let m = msg(d.apply_changes(&set("users", vec![rename("1", Some("Nope"))])).await);
        assert!(m.contains("READ ONLY"), "{m}");
        assert_eq!(mysql_one(&p, &pw, "select name from fx.users where id = 1").await.as_deref(), Some("Joanna"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mysql_generated_column_edit_refused() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let r = d.apply_changes(&set("gen", vec![update("1", &[("b", Some("9"))], &[ID, ("b", CellType::Integer)])])).await;
        assert_eq!(msg(r), "b can't be edited");
        d.close().await;
    })
    .await;
}

/// A save runs in strict mode whatever the session's `sql_mode`, so a value the server
/// would coerce (`abc` into an INT becomes 0 with a warning) fails the save, and nothing is saved.
#[tokio::test]
async fn mysql_save_is_strict_whatever_the_sql_mode() {
    for_each_mysql(|mut p, pw| async move {
        p.startup_sql = Some("SET SESSION sql_mode = ''".into());
        let d = mysql_driver(&p, &pw).await;
        let original = [ID, ("y", CellType::Integer), ("ti", CellType::Integer)];
        let changes = set("typed", vec![update("2", &[("y", Some("2001"))], &original), update("1", &[("ti", Some("abc"))], &original)]);
        let m = msg(d.apply_changes(&changes).await);
        assert!(m.starts_with("UPDATE id = 1: "), "{m}");
        assert_eq!(mysql_one(&p, &pw, "select concat(y, ',', ti) from fx.typed where id = 2").await.as_deref(), Some("1999,0"));
        assert_eq!(mysql_one(&p, &pw, "select ti from fx.typed where id = 1").await.as_deref(), Some("1"));
        // The pool's reset put the session's own sql_mode back on the save's connection.
        let mut c = d.pool().get_conn().await.unwrap();
        let mode: Option<String> = mysql_async::prelude::Queryable::query_first(&mut c, "SELECT @@SESSION.sql_mode").await.unwrap();
        assert_eq!(mode.as_deref(), Some(""));
        drop(c);
        d.close().await;
    })
    .await;
}

/// A table without transactions keeps the changes made before a failure; the save says so
/// instead of "nothing was saved".
#[tokio::test]
async fn mysql_save_on_a_non_transactional_table_is_uncertain() {
    for_each_mysql(|p, pw| async move {
        common::mysql::mysql_exec(
            &common::mysql::mysql_profile_url(&p, &pw),
            "CREATE TABLE fx.myi (id INT PRIMARY KEY, n INT) ENGINE=MyISAM; INSERT INTO fx.myi VALUES (1, 1), (2, 2);",
        )
        .await;
        let d = mysql_driver(&p, &pw).await;
        let original = [ID, ("n", CellType::Integer)];
        let changes = set("myi", vec![update("1", &[("n", Some("10"))], &original), update("2", &[("n", Some("abc"))], &original)]);
        assert_eq!(
            d.apply_changes(&changes).await,
            Err(AppError::Uncertain(
                "Some changes may have been saved: this table doesn't support transactions. Reload before saving again.".into()
            ))
        );
        assert_eq!(mysql_one(&p, &pw, "select group_concat(id, ':', n order by id) from fx.myi").await.as_deref(), Some("1:10,2:2"));
        // A save that fails on its first statement changed nothing, and says so as before.
        let m = msg(d.apply_changes(&set("myi", vec![update("2", &[("n", Some("abc"))], &original)])).await);
        assert!(m.starts_with("UPDATE id = 2: "), "{m}");
        d.close().await;
    })
    .await;
}
