//! MongoDB grid saves on every server in `TEST_MONGO_URLS`: one transaction on the replica set,
//! one change at a time with an exact partial-save report on the standalone servers.

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use porthole_lib::driver::Driver;
use porthole_lib::error::{AppError, AppResult};
use porthole_lib::model::*;
use common::mongo::{for_each_mongo, has_auth, mongo_client, mongo_driver};
use common::net;
use mongodb::bson::{doc, oid::ObjectId, Bson, Document};

const LOCKED: &str = "Another session's transaction is holding this document. Try again in a moment.";
const READ_ONLY: &str = "This connection is read-only. Turn read-only off in the connection settings to edit.";
const PARTIAL_TAIL: &str = " This server has no transactions, so the saved changes stay saved; the rest are still pending.";

const ANN: &str = "650000000000000000000001";
const BOB: &str = "650000000000000000000002";
const CID: &str = "650000000000000000000003";
const FAY: &str = "650000000000000000000006";

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

const OID: (&str, CellType) = ("_id", CellType::ObjectId);

/// An update of the `users` document `id` (an ObjectId), each edited cell with its original code.
fn update_user(id: &str, vals: &[(&str, &str, CellType)]) -> RowChange {
    let mut original = types(&[OID]);
    original.extend(vals.iter().map(|(k, _, t)| (k.to_string(), *t)));
    RowChange::Update {
        key: vec![id.into()],
        values: vals.iter().map(|(k, v, _)| (k.to_string(), Some(v.to_string()))).collect(),
        original_types: original,
    }
}

fn delete_user(id: &str) -> RowChange {
    RowChange::Delete { key: vec![id.into()], original_types: types(&[OID]) }
}

fn insert(vals: &[(&str, Option<&str>)]) -> RowChange {
    RowChange::Insert { values: values(vals) }
}

fn query_msg<T: std::fmt::Debug>(r: AppResult<T>) -> String {
    match r {
        Err(AppError::Query(m)) => m,
        other => panic!("expected a query error, got {other:?}"),
    }
}

fn oid(hex: &str) -> Bson {
    Bson::ObjectId(ObjectId::parse_str(hex).unwrap())
}

/// The stored documents of `fx.{name}`, read on a client of their own, by `_id`.
async fn stored(url: &str, name: &str) -> Vec<Document> {
    use futures_util::TryStreamExt;
    let client = mongo_client(url).await;
    let docs = client
        .database("fx")
        .collection::<Document>(name)
        .find(doc! {})
        .sort(doc! { "_id": 1 })
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    client.shutdown().immediate(true).await;
    docs
}

async fn stored_one(url: &str, name: &str, filter: Document) -> Option<Document> {
    let client = mongo_client(url).await;
    let d = client.database("fx").collection::<Document>(name).find_one(filter).await.unwrap();
    client.shutdown().immediate(true).await;
    d
}

async fn user(url: &str, id: &str) -> Option<Document> {
    stored_one(url, "users", doc! { "_id": oid(id) }).await
}

async fn page(d: &dyn Driver, name: &str) -> TablePage {
    d.fetch_page(&PageRequest { table: fx(name), filter: None, sort: None, page: 0, page_size: 100 }).await.unwrap()
}

/// Each row's `_id` text and code, and its `v` text.
fn keys_and_v(p: &TablePage) -> Vec<(String, char, String)> {
    // An empty collection has only the `_id` column.
    if p.rows.is_empty() {
        return Vec::new();
    }
    let id = p.columns.iter().position(|c| c.name == "_id").unwrap();
    let v = p.columns.iter().position(|c| c.name == "v").unwrap();
    p.rows
        .iter()
        .zip(p.cell_types.as_ref().unwrap())
        .map(|(r, codes)| {
            (r[id].clone().unwrap(), codes.chars().nth(id).unwrap(), r[v].clone().unwrap())
        })
        .collect()
}

#[tokio::test]
async fn mongo_update_insert_delete() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let changes = set(
            "users",
            vec![
                update_user(ANN, &[("age", "31", CellType::Integer)]),
                delete_user(FAY),
                insert(&[("name", Some("Gus")), ("email", Some("gus@example.com")), ("age", Some("52"))]),
            ],
        );
        assert_eq!(d.apply_changes(&changes).await.unwrap(), ApplyResult { applied: 3 }, "{url}");

        // The int32 cell edited to 31 stays int32.
        assert_eq!(user(&url, ANN).await.unwrap().get("age"), Some(&Bson::Int32(31)), "{url}");
        assert!(user(&url, FAY).await.is_none(), "{url}");
        // The new document's `age` takes the column's dominant type (int32: Ann and Eve).
        let gus = stored_one(&url, "users", doc! { "email": "gus@example.com" }).await.expect("Gus inserted");
        assert_eq!(gus.get("age"), Some(&Bson::Int32(52)), "{url}");
        assert_eq!(gus.get("name"), Some(&Bson::String("Gus".into())), "{url}");
        assert!(matches!(gus.get("_id"), Some(Bson::ObjectId(_))), "{url}: {gus:?}");
        // The fields go in column order.
        let keys: Vec<&str> = gus.keys().map(String::as_str).filter(|k| *k != "_id").collect();
        assert_eq!(keys, ["name", "email", "age"], "{url}");
        assert_eq!(stored(&url, "users").await.len(), 6, "{url}");
        d.close().await;
    })
    .await;
}

/// Saving every editable cell of `fx.typed` with its own text leaves the stored
/// document byte-equal (types and key order included).
#[tokio::test]
async fn mongo_unchanged_edits_keep_types() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let before = stored(&url, "typed").await[0].to_vec().unwrap();
        let typed = page(d.as_ref(), "typed").await;
        let codes: Vec<CellType> =
            typed.cell_types.as_ref().unwrap()[0].chars().map(|c| CellType::from_code(c).unwrap()).collect();
        let mut vals = CellValues::new();
        let mut original = BTreeMap::new();
        for (i, c) in typed.columns.iter().enumerate() {
            if c.name == "_id" {
                original.insert(c.name.clone(), codes[i]);
                continue;
            }
            if c.read_only || codes[i] == CellType::Other {
                continue;
            }
            vals.insert(c.name.clone(), typed.rows[0][i].clone());
            original.insert(c.name.clone(), codes[i]);
        }
        // Every type of the fixture but the `x` ones and the read-only names.
        for name in ["int64_big", "double_one", "double_1e300", "double_nan", "decimal", "date_ms", "date_year_10000", "nested_za", "nested_neg", "null"] {
            assert!(vals.contains_key(name), "{url}: {name} not edited");
        }
        assert!(!vals.contains_key("uuid") && !vals.contains_key("a.b") && !vals.contains_key("$c"));
        let key = typed.rows[0][0].clone().unwrap();
        let change = RowChange::Update { key: vec![key], values: vals, original_types: original };
        assert_eq!(d.apply_changes(&set("typed", vec![change])).await.unwrap(), ApplyResult { applied: 1 }, "{url}");
        let after = stored(&url, "typed").await[0].to_vec().unwrap();
        assert!(before == after, "{url}: the document changed:\n{:?}", stored(&url, "typed").await[0]);
        d.close().await;
    })
    .await;
}

/// An ObjectId, the string with the same hex, int64 2^53 and 2^53 + 1, a UUID
/// and a document key each find exactly their own document, for an update and for a delete.
#[tokio::test]
async fn mongo_keys_of_every_type_hit_one_document() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let rows = keys_and_v(&page(d.as_ref(), "keys").await);
        assert_eq!(rows.len(), 6, "{url}");
        let codes: Vec<char> = rows.iter().map(|r| r.1).collect();
        for code in ['o', 't', 'l', 'x', 'j'] {
            assert!(codes.contains(&code), "{url}: no key with code {code}: {codes:?}");
        }
        let id_type = |code: char| [("_id".to_string(), CellType::from_code(code).unwrap())];

        for (i, (id, code, _)) in rows.iter().enumerate() {
            let mut original: BTreeMap<String, CellType> = id_type(*code).into_iter().collect();
            original.insert("v".into(), CellType::Integer);
            let v = (i + 1).to_string();
            let change = RowChange::Update { key: vec![id.clone()], values: values(&[("v", Some(&v))]), original_types: original };
            assert_eq!(d.apply_changes(&set("keys", vec![change])).await.unwrap(), ApplyResult { applied: 1 }, "{url}: {id}");
            let now = keys_and_v(&page(d.as_ref(), "keys").await);
            let want: Vec<String> = (0..rows.len()).map(|j| if j <= i { (j + 1).to_string() } else { "0".into() }).collect();
            let got: Vec<String> = now.iter().map(|r| r.2.clone()).collect();
            assert_eq!(got, want, "{url}: after updating {id} ({code})");
        }

        for (i, (id, code, _)) in rows.iter().enumerate() {
            let change = RowChange::Delete { key: vec![id.clone()], original_types: id_type(*code).into_iter().collect() };
            assert_eq!(d.apply_changes(&set("keys", vec![change])).await.unwrap(), ApplyResult { applied: 1 }, "{url}: {id}");
            let left: Vec<(String, char)> = keys_and_v(&page(d.as_ref(), "keys").await).into_iter().map(|r| (r.0, r.1)).collect();
            let want: Vec<(String, char)> = rows[i + 1..].iter().map(|r| (r.0.clone(), r.1)).collect();
            assert_eq!(left, want, "{url}: after deleting {id} ({code})");
        }
        d.close().await;
    })
    .await;
}

/// Matched 1 / modified 0 is a success, not a changed row.
#[tokio::test]
async fn mongo_saving_an_unchanged_value_succeeds() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let same = set("users", vec![update_user(ANN, &[("name", "Ann", CellType::Text)])]);
        assert_eq!(d.apply_changes(&same).await.unwrap(), ApplyResult { applied: 1 }, "{url}");
        assert_eq!(user(&url, ANN).await.unwrap().get_str("name").ok(), Some("Ann"));
        d.close().await;
    })
    .await;
}

/// A document deleted behind the save: its update or delete names it.
#[tokio::test]
async fn mongo_row_changed_is_reported() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let client = mongo_client(&url).await;
        client.database("fx").collection::<Document>("users").delete_one(doc! { "_id": oid(ANN) }).await.unwrap();
        client.shutdown().immediate(true).await;

        let gone = format!(r#"The row with _id ObjectId("{ANN}") was changed or deleted by someone else."#);
        let update = set("users", vec![update_user(ANN, &[("name", "Annie", CellType::Text)])]);
        assert_eq!(query_msg(d.apply_changes(&update).await), gone, "{url}");
        let delete = set("users", vec![delete_user(ANN)]);
        assert_eq!(query_msg(d.apply_changes(&delete).await), gone, "{url}");
        assert!(user(&url, ANN).await.is_none());

        // After a saved change: all or nothing in a transaction, one at a time without.
        let after = set("users", vec![update_user(BOB, &[("name", "Bobby", CellType::Text)]), delete_user(ANN)]);
        let bob = user(&url, BOB).await.unwrap();
        if d.one_by_one_saves() {
            let want = format!("Saved 1 of 2 changes, then: {gone}{PARTIAL_TAIL}");
            assert_eq!(d.apply_changes(&after).await, Err(AppError::PartlySaved { message: want, applied: 1, uncertain: false }), "{url}");
            assert_eq!(user(&url, BOB).await.unwrap().get_str("name").ok(), Some("Bobby"), "{url}");
        } else {
            assert_eq!(query_msg(d.apply_changes(&after).await), gone, "{url}");
            assert_eq!(user(&url, BOB).await.unwrap(), bob, "{url}");
        }
        d.close().await;
    })
    .await;
}

/// The second of three changes is rejected by the unique index on `email`.
/// Without transactions the first stays saved and is reported exactly; on the replica set
/// nothing is applied. The third is never applied.
#[tokio::test]
async fn mongo_partial_save_is_reported_exactly() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let changes = set(
            "users",
            vec![
                update_user(ANN, &[("age", "31", CellType::Integer)]),
                insert(&[("name", Some("Dup")), ("email", Some("bob@example.com"))]),
                update_user(CID, &[("name", "Cyd", CellType::Text)]),
            ],
        );
        let result = d.apply_changes(&changes).await;
        let ann_age = user(&url, ANN).await.unwrap().get("age").cloned();
        if d.one_by_one_saves() {
            let Err(AppError::PartlySaved { message, applied, uncertain: false }) = result else {
                panic!("{url}: expected a partial save, got {result:?}")
            };
            assert_eq!(applied, 1, "{url}");
            assert!(message.starts_with("Saved 1 of 3 changes, then: E11000 duplicate key error"), "{url}: {message}");
            assert!(message.ends_with(PARTIAL_TAIL), "{url}: {message}");
            assert!(!message.contains("MONGO_"), "{url}: {message}");
            assert_eq!(ann_age, Some(Bson::Int32(31)), "{url}");
        } else {
            let m = query_msg(result);
            assert!(m.starts_with("E11000 duplicate key error"), "{url}: {m}");
            assert_eq!(ann_age, Some(Bson::Int32(30)), "{url}");
        }
        assert!(stored_one(&url, "users", doc! { "name": "Dup" }).await.is_none(), "{url}");
        assert_eq!(user(&url, CID).await.unwrap().get_str("name").ok(), Some("Cid"), "{url}");
        d.close().await;
    })
    .await;
}

/// On the replica set, a document held by another session's open transaction fails the save at
/// once with the lock copy (the server reports a write conflict).
#[tokio::test]
async fn mongo_write_conflict_is_save_locked() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        if d.one_by_one_saves() {
            d.close().await;
            return;
        }
        let client = mongo_client(&url).await;
        let users = client.database("fx").collection::<Document>("users");
        let mut holder = client.start_session().await.unwrap();
        holder.start_transaction().await.unwrap();
        users.update_one(doc! { "_id": oid(ANN) }, doc! { "$set": { "name": "Held" } }).session(&mut holder).await.unwrap();

        let started = Instant::now();
        let save = set("users", vec![update_user(ANN, &[("name", "Annie", CellType::Text)])]);
        assert_eq!(query_msg(d.apply_changes(&save).await), LOCKED, "{url}");
        assert!(started.elapsed() < Duration::from_secs(1), "{url}: took {:?}", started.elapsed());

        holder.abort_transaction().await.unwrap();
        drop(holder);
        client.shutdown().immediate(true).await;
        assert_eq!(user(&url, ANN).await.unwrap().get_str("name").ok(), Some("Ann"), "{url}");
        d.close().await;
    })
    .await;
}

/// Root with a read-only profile is refused before anything is sent; pages still
/// load.
#[tokio::test]
async fn mongo_read_only_profile_refuses_saves() {
    for_each_mongo(|mut p, pw, url| async move {
        p.read_only = true;
        let d = mongo_driver(&p, pw).await;
        let changes = set("users", vec![update_user(ANN, &[("name", "Annie", CellType::Text)]), delete_user(BOB)]);
        assert_eq!(query_msg(d.apply_changes(&changes).await), READ_ONLY, "{url}");
        // Refused before the collection is even read.
        assert_eq!(query_msg(d.apply_changes(&set("nope", vec![delete_user(BOB)])).await), READ_ONLY, "{url}");
        assert_eq!(user(&url, ANN).await.unwrap().get_str("name").ok(), Some("Ann"), "{url}");
        assert!(user(&url, BOB).await.is_some(), "{url}");
        assert_eq!(page(d.as_ref(), "users").await.rows.len(), 6, "{url}");
        d.close().await;
    })
    .await;
}

/// A user with the `read` role writing through a profile that isn't read-only: the server's
/// code-13 text repeats the command with its values, so it is never shown.
#[tokio::test]
async fn mongo_read_user_write_is_not_echoed() {
    for_each_mongo(|mut p, _pw, url| async move {
        if !has_auth(&url) {
            return;
        }
        net(&mut p).user = "ro".into();
        let d = mongo_driver(&p, Some("ropw".into())).await;
        let want = "Not authorized to write to fx.users.";
        let update = set("users", vec![update_user(ANN, &[("name", "s3cret-value", CellType::Text)])]);
        let m = query_msg(d.apply_changes(&update).await);
        assert_eq!(m, want, "{url}");
        assert!(!m.contains("s3cret"), "{url}: {m}");
        let add = set("users", vec![insert(&[("name", Some("s3cret-value"))])]);
        assert_eq!(query_msg(d.apply_changes(&add).await), want, "{url}");
        assert_eq!(user(&url, ANN).await.unwrap().get_str("name").ok(), Some("Ann"), "{url}");
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mongo_preview_lines() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let changes = set(
            "users",
            vec![
                update_user(ANN, &[("age", "31", CellType::Integer), ("name", "Annie", CellType::Text)]),
                delete_user(FAY),
                insert(&[("name", Some("Gus")), ("email", Some("gus@example.com")), ("age", Some("52"))]),
                insert(&[]),
            ],
        );
        assert_eq!(
            d.preview_changes(&changes).await.unwrap(),
            [
                r#"db.getCollection("users").updateOne({ _id: ObjectId("650000000000000000000001") }, { $set: { name: "Annie", age: 31 } })"#,
                r#"db.getCollection("users").deleteOne({ _id: ObjectId("650000000000000000000006") })"#,
                r#"db.getCollection("users").insertOne({ name: "Gus", email: "gus@example.com", age: 52 })"#,
                r#"db.getCollection("users").insertOne({})"#,
            ],
            "{url}"
        );
        let big = RowChange::Update {
            key: vec!["9007199254740993".into()],
            values: values(&[("v", Some("2"))]),
            original_types: types(&[("_id", CellType::Long), ("v", CellType::Integer)]),
        };
        assert_eq!(
            d.preview_changes(&set("keys", vec![big])).await.unwrap(),
            [r#"db.getCollection("keys").updateOne({ _id: NumberLong("9007199254740993") }, { $set: { v: 2 } })"#],
            "{url}"
        );
        // A preview saves nothing.
        assert_eq!(user(&url, ANN).await.unwrap().get_str("name").ok(), Some("Ann"), "{url}");
        d.close().await;
    })
    .await;
}

/// A value that doesn't parse as its cell's type names the field, and nothing of the save is
/// sent.
#[tokio::test]
async fn mongo_bad_value_names_the_field() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let want = r#"age: expected a whole number (int32), got "abc"."#;
        let changes = set(
            "users",
            vec![update_user(BOB, &[("name", "Bobby", CellType::Text)]), update_user(ANN, &[("age", "abc", CellType::Integer)])],
        );
        assert_eq!(query_msg(d.apply_changes(&changes).await), want, "{url}");
        assert_eq!(query_msg(d.preview_changes(&changes).await), want, "{url}");
        assert_eq!(user(&url, BOB).await.unwrap().get_str("name").ok(), Some("Bob"), "{url}");
        assert_eq!(user(&url, ANN).await.unwrap().get("age"), Some(&Bson::Int32(30)), "{url}");
        d.close().await;
    })
    .await;
}
