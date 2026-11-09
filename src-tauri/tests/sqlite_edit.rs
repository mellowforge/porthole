//! SQLite grid saves: one transaction, all or nothing.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use porthole_lib::driver::sqlite::{connect_sqlite, SqliteDialect, SqliteDriver};
use porthole_lib::driver::Driver;
use porthole_lib::error::AppError;
use porthole_lib::model::*;
use common::sqlite::{sqlite_fixture, sqlite_profile};

const LOCKED: &str = "The database is locked by another connection (an open transaction in a query tab?).";
const BLOB: &str = "Blob values can't be edited yet.";

fn main_table(name: &str) -> TableRef {
    TableRef { schema: "main".into(), name: name.into() }
}

fn set(table: &str, changes: Vec<RowChange>) -> ChangeSet {
    ChangeSet { table: main_table(table), changes }
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

/// One text value of the first row of `sql`, read with rusqlite directly; `None` when there is
/// no row or the value is NULL.
fn one(path: &Path, sql: &str) -> Option<String> {
    let c = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = c.prepare(sql).unwrap();
    let mut rows = stmt.query([]).unwrap();
    rows.next().unwrap().and_then(|r| r.get::<_, Option<String>>(0).unwrap())
}

/// Runs SQL on the fixture file before the driver opens it. Test setup only.
fn setup(path: &Path, sql: &str) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(sql).unwrap();
    c.close().map_err(|(_, e)| e).unwrap();
}

async fn open(path: &Path, read_only: bool) -> SqliteDriver {
    connect_sqlite(&sqlite_profile(path, read_only)).await.unwrap()
}

const ID: (&str, CellType) = ("id", CellType::Integer);
const NAME: (&str, CellType) = ("name", CellType::Text);

#[tokio::test]
async fn sqlite_update_delete_insert_round_trip() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    // Users 1 and 2 have orders, and foreign keys are on: the deleted user is 16, who has none.
    let changes = set(
        "users",
        vec![
            update("1", &[("name", Some("Joanna B"))], &[ID, NAME]),
            delete("16", &[ID]),
            insert(&[("name", Some("New"))]),
        ],
    );
    assert_eq!(d.apply_changes(&changes).await.unwrap(), ApplyResult { applied: 3 });

    assert_eq!(one(&path, "select name from users where id = 1").as_deref(), Some("Joanna B"));
    assert_eq!(one(&path, "select count(*) || '' from users where id = 16").as_deref(), Some("0"));
    let new_id: i64 = one(&path, "select id || '' from users where name = 'New'").unwrap().parse().unwrap();
    assert!(new_id > 17, "{new_id}");
    // The grid sees the same.
    let page = d.fetch_page(&PageRequest { table: main_table("users"), filter: None, sort: None, page: 0, page_size: 100 }).await.unwrap();
    assert_eq!(page.rows.len(), 17);
    assert_eq!(page.rows[0][1].as_deref(), Some("Joanna B"));
    d.close().await;
}

#[tokio::test]
async fn sqlite_insert_default_values() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    // `nopk` has no primary key, so it can't be edited at all: DEFAULT VALUES is
    // tried on a keyed table whose other columns are nullable.
    let structure = d.table_structure(&main_table("Order Items")).await.unwrap();
    let planned = porthole_lib::edit::plan_changes(&SqliteDialect, &structure, &set("Order Items", vec![insert(&[])])).unwrap();
    assert_eq!(planned[0].sql, r#"INSERT INTO "main"."Order Items" DEFAULT VALUES"#);
    assert_eq!(d.apply_changes(&set("Order Items", vec![insert(&[])])).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(one(&path, r#"select count(*) || ' ' || max(id) from "Order Items""#).as_deref(), Some("3 3"));
    assert_eq!(one(&path, r#"select name from "Order Items" where id = 3"#), None);

    let err = d.apply_changes(&set("nopk", vec![insert(&[])])).await.unwrap_err();
    assert_eq!(err, AppError::Query("This table can't be edited.".into()));
    assert_eq!(one(&path, "select count(*) || '' from nopk").as_deref(), Some("2"));
    d.close().await;
}

#[tokio::test]
async fn sqlite_loose_keys_hit_the_right_row() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    let rows = || one(&path, "select group_concat(typeof(k) || ':' || v, ',') from (select * from loose order by v)");

    // The text key `'5'`: only the text row changes.
    let up = set("loose", vec![update("5", &[("v", Some("text changed"))], &[("k", CellType::Text), ("v", CellType::Text)])]);
    assert_eq!(d.apply_changes(&up).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(rows().as_deref(), Some("integer:int five,text:text changed"));

    // The integer key `5`: only the integer row goes.
    let del = set("loose", vec![delete("5", &[("k", CellType::Integer)])]);
    assert_eq!(d.apply_changes(&del).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(rows().as_deref(), Some("text:text changed"));
    d.close().await;
}

/// The preview shows what runs: the integer key as a number (it binds as `(?1 + 0)`), the text
/// key, and every value of a column with an affinity, as quoted text.
#[tokio::test]
async fn sqlite_preview_shows_typed_keys_as_they_run() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    let structure = d.table_structure(&main_table("loose")).await.unwrap();
    let preview = |change: RowChange| {
        let planned = porthole_lib::edit::plan_changes(&SqliteDialect, &structure, &set("loose", vec![change])).unwrap();
        (planned[0].sql.clone(), planned[0].preview.clone())
    };
    assert_eq!(
        preview(delete("5", &[("k", CellType::Integer)])),
        (r#"DELETE FROM "main"."loose" WHERE "k" = (?1 + 0)"#.to_string(), r#"DELETE FROM "main"."loose" WHERE "k" = 5;"#.to_string())
    );
    assert_eq!(
        preview(delete("5", &[("k", CellType::Text)])),
        (r#"DELETE FROM "main"."loose" WHERE "k" = ?1"#.to_string(), r#"DELETE FROM "main"."loose" WHERE "k" = '5';"#.to_string())
    );
    assert_eq!(
        preview(update("5", &[("v", Some("it's")), ("k", Some("6"))], &[("k", CellType::Integer), ("v", CellType::Text)])).1,
        r#"UPDATE "main"."loose" SET "k" = 6, "v" = 'it''s' WHERE "k" = 5;"#
    );
    assert_eq!(preview(update("5", &[("v", None)], &[("k", CellType::Text)])).1, r#"UPDATE "main"."loose" SET "v" = NULL WHERE "k" = '5';"#);
    // An INTEGER column binds text that SQLite converts: the preview quotes it, as it is bound.
    let users = d.table_structure(&main_table("users")).await.unwrap();
    let planned = porthole_lib::edit::plan_changes(&SqliteDialect, &users, &set("users", vec![delete("16", &[ID])])).unwrap();
    assert_eq!(planned[0].preview, r#"DELETE FROM "main"."users" WHERE "id" = '16';"#);
    d.close().await;
}

/// Both `loose` rows, keyed `5` and `'5'`, in one save: two changes, each on its own row.
#[tokio::test]
async fn sqlite_both_loose_rows_in_one_save() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    let rows = || one(&path, "select group_concat(typeof(k) || ':' || v, ',') from (select * from loose order by typeof(k))");
    let both = set(
        "loose",
        vec![
            update("5", &[("v", Some("int edited"))], &[("k", CellType::Integer), ("v", CellType::Text)]),
            update("5", &[("v", Some("text edited"))], &[("k", CellType::Text), ("v", CellType::Text)]),
        ],
    );
    assert_eq!(d.apply_changes(&both).await.unwrap(), ApplyResult { applied: 2 });
    assert_eq!(rows().as_deref(), Some("integer:int edited,text:text edited"));

    let both = set("loose", vec![delete("5", &[("k", CellType::Integer)]), delete("5", &[("k", CellType::Text)])]);
    assert_eq!(d.apply_changes(&both).await.unwrap(), ApplyResult { applied: 2 });
    assert_eq!(rows(), None);
    d.close().await;
}

/// SQLite allows NULL in a key that isn't INTEGER PRIMARY KEY; such a row can't be named.
#[tokio::test]
async fn sqlite_a_null_key_is_refused() {
    let (_dir, path) = sqlite_fixture();
    setup(&path, "insert into loose values (null, 'null key')");
    let d = open(&path, false).await;
    let empty = AppError::Query("Rows with an empty key can't be edited.".into());
    let del = set("loose", vec![delete("", &[("k", CellType::Null)])]);
    assert_eq!(d.apply_changes(&del).await.unwrap_err(), empty);
    let up = set("loose", vec![update("", &[("v", Some("x"))], &[("k", CellType::Null), ("v", CellType::Text)])]);
    assert_eq!(d.apply_changes(&up).await.unwrap_err(), empty);
    assert_eq!(one(&path, "select v from loose where k is null").as_deref(), Some("null key"));
    d.close().await;
}

/// A composite key: a change names its row by both parts, including a change to one of them.
#[tokio::test]
async fn sqlite_composite_key_hits_the_right_row() {
    let (_dir, path) = sqlite_fixture();
    setup(
        &path,
        "create table pair (a integer, b text, v text, primary key (a, b));
         insert into pair values (1, 'x', 'one x'), (1, 'y', 'one y'), (2, 'x', 'two x');",
    );
    let d = open(&path, false).await;
    let rows = || one(&path, "select group_concat(a || b || ':' || v, ',') from (select * from pair order by a, b)");
    let key_types = [("a", CellType::Integer), ("b", CellType::Text), ("v", CellType::Text)];
    let change = |a: &str, b: &str, vals: &[(&str, Option<&str>)]| RowChange::Update {
        key: vec![a.into(), b.into()],
        values: values(vals),
        original_types: types(&key_types),
    };
    assert_eq!(d.apply_changes(&set("pair", vec![change("1", "y", &[("v", Some("edited"))])])).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(rows().as_deref(), Some("1x:one x,1y:edited,2x:two x"));
    // One key part changes: only that row moves.
    assert_eq!(d.apply_changes(&set("pair", vec![change("2", "x", &[("b", Some("z"))])])).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(rows().as_deref(), Some("1x:one x,1y:edited,2z:two x"));
    let del = RowChange::Delete { key: vec!["1".into(), "x".into()], original_types: types(&key_types[..2]) };
    assert_eq!(d.apply_changes(&set("pair", vec![del])).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(rows().as_deref(), Some("1y:edited,2z:two x"));
    d.close().await;
}

/// `gen.b` is generated: inserts and edits of `a` recompute it, and it is never written.
#[tokio::test]
async fn sqlite_generated_column_is_recomputed_and_never_written() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    let structure = d.table_structure(&main_table("gen")).await.unwrap();
    let b = structure.columns.iter().find(|c| c.name == "b").unwrap();
    assert!(b.read_only);

    let changes = set("gen", vec![update("1", &[("a", Some("5"))], &[ID, ("a", CellType::Integer)]), insert(&[("a", Some("7"))])]);
    let planned = porthole_lib::edit::plan_changes(&SqliteDialect, &structure, &changes).unwrap();
    assert!(planned.iter().all(|st| !st.sql.contains("\"b\"")), "{planned:?}");
    assert_eq!(d.apply_changes(&changes).await.unwrap(), ApplyResult { applied: 2 });
    assert_eq!(one(&path, "select group_concat(id || ':' || a || ':' || b, ',') from gen").as_deref(), Some("1:5:10,2:7:14"));

    let write_b = set("gen", vec![update("1", &[("b", Some("99"))], &[ID])]);
    assert_eq!(d.apply_changes(&write_b).await.unwrap_err(), AppError::Query("b can't be edited".into()));
    assert_eq!(d.apply_changes(&set("gen", vec![insert(&[("b", Some("99"))])])).await.unwrap_err(), AppError::Query("b can't be edited".into()));
    assert_eq!(one(&path, "select group_concat(b, ',') from gen").as_deref(), Some("10,14"));
    d.close().await;
}

/// A `WITHOUT ROWID` table edits like any keyed table.
#[tokio::test]
async fn sqlite_without_rowid_round_trip() {
    let (_dir, path) = sqlite_fixture();
    setup(&path, "create table tags (name text primary key, n integer) without rowid; insert into tags values ('a', 1), ('b', 2);");
    let d = open(&path, false).await;
    let tag = ("name", CellType::Text);
    let changes = set(
        "tags",
        vec![delete("b", &[tag]), update("a", &[("n", Some("10"))], &[tag, ("n", CellType::Integer)]), insert(&[("name", Some("c")), ("n", Some("3"))])],
    );
    assert_eq!(d.apply_changes(&changes).await.unwrap(), ApplyResult { applied: 3 });
    assert_eq!(one(&path, "select group_concat(name || ':' || n, ',') from (select * from tags order by name)").as_deref(), Some("a:10,c:3"));
    let page = d.fetch_page(&PageRequest { table: main_table("tags"), filter: None, sort: None, page: 0, page_size: 100 }).await.unwrap();
    assert_eq!(page.primary_key, vec!["name".to_string()]);
    assert_eq!(page.rows.len(), 2);
    d.close().await;
}

/// A key edited away from a number: SET binds the new text as it is, while WHERE finds the row
/// by the original key with its original type.
#[tokio::test]
async fn sqlite_key_edit_on_a_loose_key() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    let edit = set("loose", vec![update("5", &[("k", Some("abc"))], &[("k", CellType::Integer)])]);
    assert_eq!(d.apply_changes(&edit).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(
        one(&path, "select group_concat(typeof(k) || ':' || k || ':' || v, ',') from (select * from loose order by v)").as_deref(),
        Some("text:abc:int five,text:5:text five")
    );
    d.close().await;
}

#[tokio::test]
async fn sqlite_save_is_all_or_nothing() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    let name1 = || one(&path, "select name from users where id = 1");

    // A constraint fails on the second statement: the first is rolled back.
    let bad = set("users", vec![update("1", &[("name", Some("X"))], &[ID, NAME]), update("3", &[("name", None)], &[ID, NAME])]);
    let err = d.apply_changes(&bad).await.unwrap_err();
    assert!(matches!(&err, AppError::Query(m) if m.starts_with("UPDATE id = 3:") && m.contains("NOT NULL")), "{err:?}");
    assert_eq!(name1().as_deref(), Some("Joanna"));

    // A row that is gone: Postgres's message, and nothing applied.
    let gone = set("users", vec![update("1", &[("name", Some("X"))], &[ID, NAME]), delete("999", &[ID])]);
    let err = d.apply_changes(&gone).await.unwrap_err();
    assert_eq!(err, AppError::Query("The row with id = 999 was changed or deleted by someone else.".into()));
    assert_eq!(name1().as_deref(), Some("Joanna"));

    // A foreign key (on at open): users 1 has orders.
    let fk = set("users", vec![update("2", &[("name", Some("Y"))], &[ID, NAME]), delete("1", &[ID])]);
    let err = d.apply_changes(&fk).await.unwrap_err();
    assert!(matches!(&err, AppError::Query(m) if m.starts_with("DELETE id = 1:") && m.contains("FOREIGN KEY")), "{err:?}");
    assert_eq!(one(&path, "select name from users where id = 2").as_deref(), Some("Hannah"));

    // Both browse connections are out of their transactions: the next saves work.
    for name in ["A", "B"] {
        let ok = set("users", vec![update("1", &[("name", Some(name))], &[ID, NAME])]);
        assert_eq!(d.apply_changes(&ok).await.unwrap(), ApplyResult { applied: 1 });
        assert_eq!(name1().as_deref(), Some(name));
    }
    for w in d.workers() {
        assert!(w.call(|c| c.is_autocommit()).await.unwrap());
    }
    d.close().await;
}

#[tokio::test]
async fn sqlite_save_while_a_tab_holds_a_write_lock_reports_locked() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    let tab = d.open_query_session(None).await.unwrap();
    for sql in ["begin immediate", "update users set plan = plan where id = 1"] {
        let run = tab.run(sql, &[], 100).await;
        assert!(matches!(run.outcome, porthole_lib::query::RawOutcome::Command { .. }), "{sql}: {:?}", run.outcome);
    }

    let save = set("users", vec![update("1", &[("name", Some("Locked out"))], &[ID, NAME])]);
    let started = Instant::now();
    let err = d.apply_changes(&save).await.unwrap_err();
    assert!(started.elapsed() <= Duration::from_secs(6), "{:?}", started.elapsed());
    // BEGIN IMMEDIATE really waited out the busy timeout before giving up.
    assert!(started.elapsed() >= Duration::from_secs(4), "{:?}", started.elapsed());
    assert_eq!(err, AppError::Query(LOCKED.into()));

    let run = tab.run("rollback", &[], 100).await;
    assert!(matches!(run.outcome, porthole_lib::query::RawOutcome::Command { .. }), "{:?}", run.outcome);
    assert_eq!(d.apply_changes(&save).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(one(&path, "select name from users where id = 1").as_deref(), Some("Locked out"));
    tab.close().await;
    d.close().await;
}

/// Another connection holding the file exclusively (a tab's big uncommitted write that spilled
/// its cache, or a commit waiting on readers) blocks even the save's structure read: the save
/// reports the locked copy, not SQLite's own text.
#[tokio::test]
async fn sqlite_save_while_another_connection_holds_an_exclusive_lock_reports_locked() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    let other = rusqlite::Connection::open(&path).unwrap();
    other.execute_batch("begin exclusive").unwrap();

    let save = set("users", vec![update("1", &[("name", Some("Excluded"))], &[ID, NAME])]);
    let started = Instant::now();
    let err = d.apply_changes(&save).await.unwrap_err();
    assert!(started.elapsed() <= Duration::from_secs(6), "{:?}", started.elapsed());
    assert_eq!(err, AppError::Query(LOCKED.into()));

    other.execute_batch("rollback").unwrap();
    assert_eq!(d.apply_changes(&save).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(one(&path, "select name from users where id = 1").as_deref(), Some("Excluded"));
    drop(other);
    d.close().await;
}

/// A tab reading in an open transaction holds its read lock, so the save's COMMIT can't take
/// the file: it fails as locked, and the save is rolled back, not left open.
#[tokio::test]
async fn sqlite_busy_at_commit_rolls_back_the_save() {
    let (_dir, path) = sqlite_fixture();
    let d = open(&path, false).await;
    let tab = d.open_query_session(None).await.unwrap();
    for sql in ["begin", "select count(*) from users"] {
        let run = tab.run(sql, &[], 100).await;
        assert!(!matches!(run.outcome, porthole_lib::query::RawOutcome::Error { .. }), "{sql}: {:?}", run.outcome);
    }

    let save = set("users", vec![update("1", &[("name", Some("Late"))], &[ID, NAME])]);
    let started = Instant::now();
    let err = d.apply_changes(&save).await.unwrap_err();
    assert!(started.elapsed() <= Duration::from_secs(6), "{:?}", started.elapsed());
    assert_eq!(err, AppError::Query(LOCKED.into()));
    for w in d.workers() {
        assert!(w.call(|c| c.is_autocommit()).await.unwrap(), "a browse connection was left in the save's transaction");
    }

    tab.run("rollback", &[], 100).await;
    assert_eq!(one(&path, "select name from users where id = 1").as_deref(), Some("Joanna"));
    assert_eq!(d.apply_changes(&save).await.unwrap(), ApplyResult { applied: 1 });
    assert_eq!(one(&path, "select name from users where id = 1").as_deref(), Some("Late"));
    tab.close().await;
    d.close().await;
}

#[tokio::test]
async fn sqlite_float_key_and_blob_key_refused() {
    let (_dir, path) = sqlite_fixture();
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("insert into loose values (x'01', 'blob one')").unwrap();
    }
    let d = open(&path, false).await;

    let float = set("floatkey", vec![update("1.5", &[("v", Some("x"))], &[("k", CellType::Real), ("v", CellType::Text)])]);
    assert_eq!(d.apply_changes(&float).await.unwrap_err(), AppError::Query("This table can't be edited.".into()));
    assert_eq!(one(&path, "select v from floatkey where k = 1.5").as_deref(), Some("one and a half"));

    // A blob key refuses the whole save, before anything runs.
    let blob_delete = set(
        "loose",
        vec![update("5", &[("v", Some("changed"))], &[("k", CellType::Integer), ("v", CellType::Text)]), delete("\\x01", &[("k", CellType::Blob)])],
    );
    assert_eq!(d.apply_changes(&blob_delete).await.unwrap_err(), AppError::Query(BLOB.into()));
    let blob_update = set("loose", vec![update("\\x01", &[("v", Some("changed"))], &[("k", CellType::Blob), ("v", CellType::Text)])]);
    assert_eq!(d.apply_changes(&blob_update).await.unwrap_err(), AppError::Query(BLOB.into()));
    assert_eq!(
        one(&path, "select group_concat(typeof(k) || ':' || v, ',') from (select * from loose order by v)").as_deref(),
        Some("blob:blob one,integer:int five,text:text five")
    );
    // A blob in an edited column that isn't the key is not this refusal's business.
    let not_key = set("users", vec![update("4", &[("name", Some("Bri"))], &[ID, NAME, ("avatar", CellType::Blob)])]);
    assert_eq!(d.apply_changes(&not_key).await.unwrap(), ApplyResult { applied: 1 });
    d.close().await;
}

#[tokio::test]
async fn sqlite_read_only_save_fails() {
    let (_dir, path) = sqlite_fixture();
    let before = std::fs::read(&path).unwrap();
    let d = open(&path, true).await;
    let save = set("users", vec![update("1", &[("name", Some("X"))], &[ID, NAME])]);
    let err = d.apply_changes(&save).await.unwrap_err();
    assert!(matches!(&err, AppError::Query(m) if m.contains("readonly")), "{err:?}");
    for w in d.workers() {
        assert!(w.call(|c| c.is_autocommit()).await.unwrap());
    }
    d.close().await;
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
