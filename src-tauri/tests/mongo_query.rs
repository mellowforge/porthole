//! MongoDB query tabs: reads, the cap, result shapes, errors, the database, the tab's (absent)
//! transaction, writes and Stop, on every server in `TEST_MONGO_URLS`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use porthole_lib::driver::mongodb::query::{kill_by_comment, EXPORT_COMMENT, MongoQuerySession, TIMEOUT_CODE};
use porthole_lib::driver::mongodb::MongoConfig;
use porthole_lib::driver::mongodb::values::parse_json;
use porthole_lib::driver::Driver;
use porthole_lib::export::run::{run_mongo_export, ExportOutcome};
use porthole_lib::export::ExportFormat;
use porthole_lib::model::*;
use porthole_lib::mongoquery::parse;
use porthole_lib::query::sessions::QuerySessions;
use porthole_lib::query::{
    end_tab_transaction, runner, Outcome, PendingWrite, QueryLanguage, QuerySession, RawOutcome, ResultColumn,
    RunOutcome, RunRequest, RunTarget, StatementRun, TxStatus, CANCELED_CODE,
};
use common::mongo::{for_each_mongo, has_auth, mongo_client, mongo_driver, BIG_ROWS};
use common::net;
use futures_util::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};

const CAP: usize = 1_000;

async fn open(p: &ConnectionProfile, pw: Option<String>) -> (Arc<dyn Driver>, Arc<dyn QuerySession>) {
    let d = mongo_driver(p, pw).await;
    let s = d.open_query_session(None).await.unwrap();
    (d, s)
}

async fn done(d: Arc<dyn Driver>, s: Arc<dyn QuerySession>) {
    s.close().await;
    d.close().await;
}

async fn run(s: &dyn QuerySession, stmt: &str, cap: usize) -> StatementRun {
    let call = parse(stmt, &ParamValues::new()).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    s.run_mongo(&call, cap).await
}

struct Rows {
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Option<String>>>,
    truncated: bool,
    cell_types: Vec<String>,
}

fn rows(run: StatementRun) -> Rows {
    match run.outcome {
        RawOutcome::Rows { columns, rows, truncated, cell_types } => {
            let cell_types = cell_types.expect("MongoDB rows carry their cell types");
            assert_eq!(cell_types.len(), rows.len());
            Rows { columns, rows, truncated, cell_types }
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

fn col(name: &str, data_type: &str) -> ResultColumn {
    ResultColumn { name: name.into(), data_type: data_type.into() }
}

/// Column `name` of every row.
fn column(r: &Rows, name: &str) -> Vec<Option<String>> {
    let at = r.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    r.rows.iter().map(|row| row[at].clone()).collect()
}

fn texts(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|s| Some(s.to_string())).collect()
}

fn run_req(text: &str, schema: Option<&str>) -> RunRequest {
    RunRequest {
        sql: text.into(),
        target: RunTarget::All,
        params: ParamValues::new(),
        schema: schema.map(String::from),
        confirmed: false,
        fetch_all: false,
        saved_query_id: None,
        database: None,
    }
}

#[tokio::test]
async fn find_sort_limit_returns_typed_rows() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let r = rows(run(&*s, "db.users.find({}, { name: 1 }).sort({ name: 1 }).limit(3)", CAP).await);
        assert_eq!(r.columns, [col("_id", "objectId"), col("name", "string")]);
        assert_eq!(column(&r, "name"), texts(&["Ann", "Bob", "Cid"]));
        assert_eq!(column(&r, "_id")[0].as_deref(), Some("650000000000000000000001"));
        assert_eq!(r.cell_types, ["ot", "ot", "ot"]);
        assert!(!r.truncated);

        // Whole documents: fields in the server's order, a missing one is NULL.
        let r = rows(run(&*s, "db.users.find({ name: { $in: [\"Eve\", \"Fay\"] } }).sort({ name: 1 })", CAP).await);
        let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_id", "name", "email", "active", "created", "tags", "age", "score", "address"]);
        assert_eq!(column(&r, "age"), [Some("41".to_string()), None]);
        assert_eq!(column(&r, "tags"), texts(&[r#"["ops"]"#, r#"["qa"]"#]));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn cap_truncates_big_find() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let r = rows(run(&*s, "db.big.find()", CAP).await);
        assert_eq!((r.rows.len(), r.truncated), (CAP, true));
        for stmt in ["db.big.find().limit(5)", "db.big.find().limit(-5)"] {
            let r = rows(run(&*s, stmt, CAP).await);
            assert_eq!((r.rows.len(), r.truncated), (5, false), "{stmt}");
        }
        // No user limit, a limit above the cap, and exactly the cap.
        for (stmt, want) in [
            ("db.big.find().limit(0)", (CAP, true)),
            ("db.big.find().limit(5000)", (CAP, true)),
            ("db.big.find().limit(1000)", (CAP, false)),
            ("db.big.find().sort({ _id: 1 }).skip(19998)", (2, false)),
        ] {
            let r = rows(run(&*s, stmt, CAP).await);
            assert_eq!((r.rows.len(), r.truncated), want, "{stmt}");
        }
        // Fetch all's cap reads the whole collection.
        let r = rows(run(&*s, "db.big.find()", 100_000).await);
        assert_eq!((r.rows.len(), r.truncated), (BIG_ROWS as usize, false));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn aggregate_appends_limit_for_the_cap() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let r = rows(run(&*s, "db.big.aggregate([{ $match: { n: { $gte: 0 } } }])", CAP).await);
        assert_eq!((r.rows.len(), r.truncated), (CAP, true));
        let r = rows(run(&*s, "db.big.aggregate([{ $sort: { _id: 1 } }, { $limit: 10 }])", CAP).await);
        assert_eq!((r.rows.len(), r.truncated), (10, false));
        assert_eq!(column(&r, "_id")[9].as_deref(), Some("9"));
        // Computed fields keep the stage's order; `$group` output too.
        let projected = "db.big.aggregate([{ $sort: { _id: 1 } }, { $limit: 2 }, { $project: { _id: 0, zz: \"$tag\", aa: \"$n\" } }])";
        let r = rows(run(&*s, projected, CAP).await);
        assert_eq!(r.columns, [col("zz", "string"), col("aa", "int32")]);
        assert_eq!(r.rows, [texts(&["t0", "0"]), texts(&["t1", "1"])]);
        let r = rows(run(&*s, "db.big.aggregate([{ $group: { _id: \"$tag\", c: { $sum: 1 } } }])", CAP).await);
        assert_eq!((r.rows.len(), r.truncated), (7, false));
        assert_eq!(r.columns, [col("_id", "string"), col("c", "int32")]);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn count_and_distinct_shapes() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        for (stmt, n) in [
            ("db.users.countDocuments({ active: true })", "4"),
            ("db.users.countDocuments()", "6"),
            ("db.users.estimatedDocumentCount()", "6"),
        ] {
            let r = rows(run(&*s, stmt, CAP).await);
            assert_eq!(r.columns, [col("count", "int64")], "{stmt}");
            assert_eq!(r.rows, [texts(&[n])], "{stmt}");
            assert_eq!(r.cell_types, ["l"], "{stmt}");
            assert!(!r.truncated);
        }
        let r = rows(run(&*s, "db.users.distinct(\"address.city\")", CAP).await);
        assert_eq!(r.columns, [col("address.city", "string")]);
        assert_eq!(column(&r, "address.city"), texts(&["Berlin", "Oslo", "Paris", "Rome"]));
        assert_eq!(r.cell_types, ["t", "t", "t", "t"]);
        let r = rows(run(&*s, "db.users.distinct(\"tags\", { active: true })", CAP).await);
        assert_eq!(column(&r, "tags"), texts(&["admin", "dev", "ops", "qa"]));
        let r = rows(run(&*s, "db.users.distinct(\"nope\")", CAP).await);
        assert_eq!((r.columns, r.rows.len()), (vec![col("nope", "null")], 0));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn find_one_on_a_view() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let r = rows(run(&*s, "db.active_users.findOne({ name: \"Bob\" }, { email: 1, _id: 0 })", CAP).await);
        assert_eq!(r.columns, [col("email", "string")]);
        assert_eq!(r.rows, [texts(&["bob@example.com"])]);
        assert!(!r.truncated);
        let r = rows(run(&*s, "db.active_users.findOne()", CAP).await);
        assert_eq!((r.rows.len(), r.truncated), (1, false));
        // Fay isn't active: no document, no columns.
        let r = rows(run(&*s, "db.active_users.findOne({ name: \"Fay\" })", CAP).await);
        assert!(r.rows.is_empty() && r.columns.is_empty());
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn server_error_keeps_message_and_code() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        for stmt in ["db.users.find({ $foo: 1 })", "db.users.countDocuments({ $foo: 1 })"] {
            match run(&*s, stmt, CAP).await.outcome {
                RawOutcome::Error { message, code, position } => {
                    assert_eq!(code.as_deref(), Some("MONGO_2"), "{stmt}");
                    assert!(message.starts_with("unknown top level operator: $foo"), "{stmt}: {message}");
                    assert_eq!(position, None);
                }
                other => panic!("{stmt}: {other:?}"),
            }
        }
        match run(&*s, "db.users.aggregate([{ $nope: {} }])", CAP).await.outcome {
            RawOutcome::Error { message, code, .. } => {
                assert_eq!((code.as_deref(), message.as_str()), (Some("MONGO_40324"), "Unrecognized pipeline stage name: '$nope'"));
            }
            other => panic!("{other:?}"),
        }
        // Through the runner it is the statement's error, and the rest are skipped.
        let req = run_req("db.users.find({ $foo: 1 })\ndb.users.find()", None);
        let report = runner::run_in(&QueryLanguage::Mongo, &*s, &Environment::Local, &req, &|_| {})
            .await
            .unwrap();
        assert!(matches!(&report.statements[0].outcome, Outcome::Error { code: Some(c), .. } if c == "MONGO_2"));
        assert_eq!(report.statements[1].outcome, Outcome::Skipped);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn database_follows_set_search_path() {
    for_each_mongo(|p, pw, url| async move {
        let (d, s) = open(&p, pw).await;
        assert!(!s.schema_switch_fails_run());
        s.set_search_path(Some("fx_other")).await.unwrap();
        assert_eq!(rows(run(&*s, "db.orders.find()", CAP).await).rows.len(), 3);
        // `None` is the database the session was opened on: the profile's.
        s.set_search_path(None).await.unwrap();
        assert_eq!(rows(run(&*s, "db.orders.find()", CAP).await).rows.len(), 0);
        assert_eq!(rows(run(&*s, "db.users.find()", CAP).await).rows.len(), 6);

        // The tab's schema reaches the session before a run.
        let reg = QuerySessions::default();
        let tab = reg.get_or_open("w", "t", None, d.open_query_session(None)).await.unwrap();
        let req = run_req("db.orders.countDocuments()", Some("fx_other"));
        let report = runner::run_on_tab(&QueryLanguage::Mongo, &tab, &Environment::Local, &req, &|_| {})
            .await
            .unwrap();
        match &report.statements[0].outcome {
            Outcome::Rows { rows, .. } => assert_eq!(rows, &[texts(&["3"])]),
            o => panic!("{o:?}"),
        }
        reg.close_window("w").await;
        done(d, s).await;

        // A profile without a database starts on `admin`.
        let mut no_db = p.clone();
        if let Transport::Network(n) = &mut no_db.transport {
            n.database = String::new();
        }
        let d = mongo_driver(&no_db, common::mongo::mongo_password(&url)).await;
        let s = d.open_query_session(None).await.unwrap();
        assert_eq!(rows(run(&*s, "db.users.find()", CAP).await).rows.len(), 0);
        s.set_search_path(Some("fx")).await.unwrap();
        assert_eq!(rows(run(&*s, "db.users.find()", CAP).await).rows.len(), 6);
        s.set_search_path(None).await.unwrap();
        assert_eq!(rows(run(&*s, "db.users.find()", CAP).await).rows.len(), 0);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn end_transaction_is_idle() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        assert_eq!(end_tab_transaction(&*s, true).await, Ok(TxStatus::Idle));
        assert_eq!(end_tab_transaction(&*s, false).await, Ok(TxStatus::Idle));
        assert_eq!(s.tx_status().await, TxStatus::Idle);
        assert_eq!(s.in_transaction().await, Ok(false));
        assert!(!s.in_failed_transaction().await);
        assert_eq!(s.export_savepoint().await, Ok(false));
        assert!(!s.is_closed());
        // Any other text is refused: the session runs no SQL.
        assert!(matches!(s.run("SELECT 1", &[], 1).await.outcome, RawOutcome::Error { .. }));
        assert!(matches!(s.run_simple("COMMIT").await, RawOutcome::Error { .. }));
        // Stop with nothing running is a no-op.
        assert_eq!(s.cancel().await, Ok(()));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn get_or_open_switches_database() {
    for_each_mongo(|p, pw, _url| async move {
        let d = mongo_driver(&p, pw).await;
        let reg = QuerySessions::default();
        let on = |db: Option<&'static str>| reg.get_or_open("w", "t", db, d.open_query_session(db));
        let tab = on(Some("fx_other")).await.unwrap();
        assert_eq!(rows(run(&*tab.session, "db.orders.find()", CAP).await).rows.len(), 3);
        // The in-transaction probe asks the session, which never is: the switch goes through.
        let tab = on(None).await.unwrap();
        assert_eq!(rows(run(&*tab.session, "db.orders.find()", CAP).await).rows.len(), 0);
        assert_eq!(rows(run(&*tab.session, "db.users.find()", CAP).await).rows.len(), 6);
        let same = on(None).await.unwrap();
        assert!(Arc::ptr_eq(&tab, &same));
        reg.close_window("w").await;
        d.close().await;
    })
    .await;
}

fn affected(run: StatementRun) -> (u64, Vec<String>) {
    match run.outcome {
        RawOutcome::Command { rows_affected } => (rows_affected, run.notices.into_iter().map(|n| n.message).collect()),
        other => panic!("expected a command, got {other:?}"),
    }
}

/// The error's message and code, and the run's notices.
fn failed(run: StatementRun) -> (String, Option<String>, Vec<String>) {
    match run.outcome {
        RawOutcome::Error { message, code, position } => {
            assert_eq!(position, None);
            (message, code, run.notices.into_iter().map(|n| n.message).collect())
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

async fn count(s: &dyn QuerySession, coll: &str) -> String {
    rows(run(s, &format!("db.{coll}.countDocuments()"), CAP).await).rows[0][0].clone().unwrap()
}

#[tokio::test]
async fn insert_one_and_many_report_counts_and_ids() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let (n, notices) =
            affected(run(&*s, "db.users.insertOne({ _id: 100, name: \"Zed\" })", CAP).await);
        assert_eq!(
            (n, notices),
            (1, vec!["Inserted 1 document (_id: 100).".to_string()])
        );
        // Without an _id the generated ObjectId is named.
        let (n, notices) = affected(run(&*s, "db.users.insertOne({ name: \"Yan\", email: \"y@x\" })", CAP).await);
        assert_eq!(n, 1);
        assert!(
            notices[0].starts_with("Inserted 1 document (_id: ObjectId(\"")
                && notices[0].ends_with("\"))."),
            "{notices:?}"
        );
        let (n, notices) = affected(
            run(
                &*s,
                "db.users.insertMany([{ name: \"A\", email: \"a@x\" }, { _id: 7, name: \"B\", email: \"b@x\" }, { name: \"C\", email: \"c@x\" }])",
                CAP,
            )
            .await,
        );
        assert_eq!((n, notices), (3, vec!["Inserted 3 documents.".to_string()]));
        let (n, notices) = affected(run(&*s, "db.users.insertMany([{ name: \"D\", email: \"d@x\" }])", CAP).await);
        assert_eq!((n, notices), (1, vec!["Inserted 1 document.".to_string()]));
        assert_eq!(count(&*s, "users").await, "12");
        let r = rows(run(&*s, "db.users.find({ name: \"B\" })", CAP).await);
        assert_eq!(column(&r, "_id"), texts(&["7"]));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn insert_many_partial_failure_ordered_and_unordered() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let docs = "[{ _id: 1 }, { _id: 2 }, { _id: 1 }, { _id: 4 }, { x: 5 }]";
        let (message, code, notices) = failed(run(&*s, &format!("db.t1.insertMany({docs})"), CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_11000"));
        assert!(message.starts_with("E11000 duplicate key error"), "{message}");
        assert_eq!(notices, ["Inserted 2 of 5 documents before the error."]);
        assert_eq!(count(&*s, "t1").await, "2");

        let (message, code, notices) =
            failed(run(&*s, &format!("db.t2.insertMany({docs}, {{ ordered: false }})"), CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_11000"));
        assert!(message.starts_with("E11000 duplicate key error"), "{message}");
        assert_eq!(notices, ["Inserted 4 of 5 documents before the error."]);
        assert_eq!(count(&*s, "t2").await, "4");
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn update_and_replace_report_matched_modified_upserted() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let one = affected(run(&*s, "db.users.updateOne({ name: \"Ann\" }, { $set: { age: 99 } })", CAP).await);
        assert_eq!(one, (1, vec!["Matched 1, modified 1.".to_string()]));
        // Two match, one already has the value.
        let many = affected(
            run(&*s, "db.users.updateMany({ name: { $in: [\"Ann\", \"Bob\"] } }, { $set: { age: 99 } })", CAP).await,
        );
        assert_eq!(many.0, 1);
        assert_eq!(many.1, ["Matched 2, modified 1."]);
        let none = affected(run(&*s, "db.users.updateOne({ name: \"Nobody\" }, { $set: { age: 1 } })", CAP).await);
        assert_eq!(none, (0, vec!["Matched 0, modified 0.".to_string()]));
        let up = affected(
            run(&*s, "db.users.updateOne({ _id: 77 }, { $set: { age: 1, email: \"u@x\" } }, { upsert: true })", CAP)
                .await,
        );
        assert_eq!(up, (1, vec!["Matched 0, modified 0. Upserted _id: 77.".to_string()]));
        let rep =
            affected(run(&*s, "db.users.replaceOne({ name: \"Cid\" }, { name: \"Cid\", replaced: true })", CAP).await);
        assert_eq!(rep, (1, vec!["Matched 1, modified 1.".to_string()]));
        let rup = affected(
            run(&*s, "db.users.replaceOne({ _id: 88 }, { name: \"New\", email: \"n@x\" }, { upsert: true })", CAP)
                .await,
        );
        assert_eq!(rup, (1, vec!["Matched 0, modified 0. Upserted _id: 88.".to_string()]));
        let r = rows(run(&*s, "db.users.find({ name: \"Cid\" })", CAP).await);
        assert_eq!(column(&r, "replaced"), texts(&["true"]));
        // A replacement document as the update is refused by the parser, before anything is sent.
        assert!(parse("db.users.updateOne({}, { age: 5 })", &ParamValues::new()).is_err());
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn pipeline_update() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let u = "db.users.updateMany({ active: true }, [{ $set: { shout: { $concat: [\"$name\", \"!\"] } } }, { $unset: \"tags\" }])";
        let (n, notices) = affected(run(&*s, u, CAP).await);
        assert_eq!(n, 4);
        assert_eq!(notices, ["Matched 4, modified 4."]);
        let r = rows(run(&*s, "db.users.find({ name: \"Ann\" })", CAP).await);
        assert!(r.columns.iter().any(|c| c.name == "shout"));
        assert!(!r.columns.iter().any(|c| c.name == "tags"));
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn delete_many_counts() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        assert_eq!(affected(run(&*s, "db.users.deleteOne({ name: \"Ann\" })", CAP).await), (1, vec![]));
        assert_eq!(affected(run(&*s, "db.users.deleteMany({ active: true })", CAP).await).0, 3);
        assert_eq!(affected(run(&*s, "db.users.deleteMany({ name: \"zzz\" })", CAP).await), (0, vec![]));
        assert_eq!(affected(run(&*s, "db.users.deleteMany({})", CAP).await).0, 2);
        assert_eq!(count(&*s, "users").await, "0");
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn writes_to_a_view_fail_with_166() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        for stmt in [
            "db.active_users.insertOne({ name: \"x\" })",
            "db.active_users.insertMany([{ name: \"x\" }])",
            "db.active_users.updateMany({}, { $set: { a: 1 } })",
            "db.active_users.deleteMany({})",
        ] {
            let (message, code, _) = failed(run(&*s, stmt, CAP).await);
            assert_eq!(code.as_deref(), Some("MONGO_166"), "{stmt}");
            assert!(message.contains("is a view, not a collection"), "{stmt}: {message}");
        }
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn validation_failure_maps_121() {
    for_each_mongo(|p, pw, url| async move {
        let client = mongo_client(&url).await;
        client
            .database("fx")
            .create_collection("vtest")
            .validator(doc! { "$jsonSchema": { "bsonType": "object", "required": ["name", "age"] } })
            .await
            .unwrap();
        let (d, s) = open(&p, pw).await;
        for stmt in ["db.vtest.insertOne({ name: \"x\" })", "db.vtest.insertMany([{ name: \"x\" }])"] {
            let (message, code, notices) = failed(run(&*s, stmt, CAP).await);
            assert_eq!(message, "The document doesn't match the collection's validation rules.", "{stmt}");
            assert_eq!(code.as_deref(), Some("MONGO_121"), "{stmt}");
            let details =
                notices.iter().find(|n| n.starts_with("Details: ")).unwrap_or_else(|| panic!("{stmt}: {notices:?}"));
            assert!(details.contains("missingProperties") && details.contains("age"), "{details}");
        }
        done(d, s).await;
    })
    .await;
}

async fn count_in(url: &str, db: &str, coll: &str) -> u64 {
    mongo_client(url).await.database(db).collection::<Document>(coll).count_documents(doc! {}).await.unwrap()
}

/// The output stage writes, nothing is appended to it, and the notice names the
/// target.
#[tokio::test]
async fn out_stage_writes_and_names_its_target() {
    for_each_mongo(|p, pw, url| async move {
        let (d, s) = open(&p, pw).await;
        let (n, notices) =
            affected(run(&*s, "db.users.aggregate([{ $match: { active: true } }, { $out: \"copy\" }])", CAP).await);
        assert_eq!((n, notices), (0, vec!["The pipeline wrote its output to fx.copy.".to_string()]));
        assert_eq!(count_in(&url, "fx", "copy").await, 4);

        let merge = "db.users.aggregate([{ $merge: { into: { db: \"fx_other\", coll: \"m\" } } }])";
        let (n, notices) = affected(run(&*s, merge, CAP).await);
        assert_eq!((n, notices), (0, vec!["The pipeline wrote its output to fx_other.m.".to_string()]));
        assert_eq!(count_in(&url, "fx_other", "m").await, 6);

        let (_, notices) =
            affected(run(&*s, "db.users.aggregate([{ $out: { db: \"fx_other\", coll: \"o\" } }])", CAP).await);
        assert_eq!(notices, ["The pipeline wrote its output to fx_other.o."]);
        let (_, notices) = affected(run(&*s, "db.users.aggregate([{ $merge: \"copy\" }])", CAP).await);
        assert_eq!(notices, ["The pipeline wrote its output to fx.copy."]);
        let (_, notices) = affected(
            run(&*s, "db.users.aggregate([{ $merge: { into: \"copy\", whenMatched: \"replace\" } }])", CAP).await,
        );
        assert_eq!(notices, ["The pipeline wrote its output to fx.copy."]);
        done(d, s).await;
    })
    .await;
}

/// A read-only profile refuses every write before sending it.
#[tokio::test]
async fn read_only_refuses_writes_before_sending() {
    for_each_mongo(|mut p, pw, url| async move {
        p.read_only = true;
        let (d, s) = open(&p, pw).await;
        let refused = |verb: &str| {
            format!("This connection is read-only. Turn read-only off in the connection settings to run {verb}.")
        };
        for (stmt, verb) in [
            ("db.users.insertOne({ name: \"Zed\" })", "insertOne"),
            ("db.users.deleteMany({})", "deleteMany"),
            ("db.users.aggregate([{ $out: \"copy\" }])", "aggregate"),
        ] {
            let (message, code, notices) = failed(run(&*s, stmt, CAP).await);
            assert_eq!((message, code.as_deref()), (refused(verb), Some("MONGO_READ_ONLY")), "{stmt}");
            assert!(notices.is_empty());
        }
        assert_eq!(count_in(&url, "fx", "users").await, 6);
        assert_eq!(count_in(&url, "fx", "copy").await, 0);
        assert_eq!(rows(run(&*s, "db.users.find()", CAP).await).rows.len(), 6);
        // A parameter holding a $out pipeline is refused too: the parsed kind decides.
        let mut params = ParamValues::new();
        params.insert("p".into(), Some("[{ $out: \"copy\" }]".into()));
        let call = parse("db.users.aggregate(:p)", &params).unwrap();
        let (_, code, _) = failed(s.run_mongo(&call, CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_READ_ONLY"));
        assert_eq!(count_in(&url, "fx", "copy").await, 0);
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn unauthorized_message_is_short() {
    for_each_mongo(|mut p, _pw, url| async move {
        if !has_auth(&url) {
            return;
        }
        net(&mut p).user = "ro".into();
        let (d, s) = open(&p, Some("ropw".into())).await;
        let (message, code, _) = failed(run(&*s, "db.users.insertOne({ name: \"s3cret-value\" })", CAP).await);
        assert_eq!(
            (message.as_str(), code.as_deref()),
            ("Not authorized to run insertOne on fx.users.", Some("MONGO_13"))
        );
        let (message, _, _) = failed(run(&*s, "db.users.aggregate([{ $out: \"s3cret-copy\" }])", CAP).await);
        assert_eq!(message, "Not authorized to run aggregate on fx.users.");
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn whole_run_through_runner() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let req = run_req("db.users.insertOne({ _id: 500, name: \"Zed\" })\ndb.users.find({ _id: 500 })", None);
        let report = runner::run_in(&QueryLanguage::Mongo, &*s, &Environment::Local, &req, &|_| {}).await.unwrap();
        assert_eq!(report.statements[0].outcome, Outcome::Affected { rows: 1 });
        assert_eq!(report.statements[0].notices[0].message, "Inserted 1 document (_id: 500).");
        match &report.statements[1].outcome {
            Outcome::Rows { rows, .. } => assert_eq!(rows.len(), 1),
            o => panic!("{o:?}"),
        }
        done(d, s).await;
    })
    .await;
}

/// The grid's save preview lines, pasted into a tab, do what the save does.
#[tokio::test]
async fn grid_preview_lines_run_in_a_tab() {
    use porthole_lib::driver::mongodb::edit::{preview, MongoOp};
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let ann = Bson::ObjectId(mongodb::bson::oid::ObjectId::parse_str("650000000000000000000001").unwrap());
        let fay = Bson::ObjectId(mongodb::bson::oid::ObjectId::parse_str("650000000000000000000006").unwrap());
        let ops = [
            MongoOp::Update { id: ann, set: doc! { "name": "Ann B", "age": 31_i64 }, key_label: String::new() },
            MongoOp::Delete { id: fay, key_label: String::new() },
            MongoOp::Insert { doc: doc! { "_id": 700, "name": "Gus" } },
        ];
        let text = preview("users", &ops).join("\n");
        let report = runner::run_in(&QueryLanguage::Mongo, &*s, &Environment::Local, &run_req(&text, None), &|_| {})
            .await
            .unwrap();
        let outcomes: Vec<&Outcome> = report.statements.iter().map(|r| &r.outcome).collect();
        assert_eq!(outcomes, [&Outcome::Affected { rows: 1 }, &Outcome::Affected { rows: 1 }, &Outcome::Affected { rows: 1 }]);
        let r = rows(run(&*s, "db.users.find({ $or: [{ age: 31 }, { name: \"Fay\" }, { _id: 700 }] }).sort({ name: 1 })", CAP).await);
        assert_eq!(column(&r, "name"), texts(&["Ann B", "Gus"]));
        assert_eq!(column(&r, "age"), [Some("31".to_string()), None]);
        done(d, s).await;
    })
    .await;
}

/// The run's rows and its notices.
fn rows_and_notices(run: StatementRun) -> (Rows, Vec<String>) {
    let notices = run.notices.iter().map(|n| n.message.clone()).collect();
    (rows(run), notices)
}

/// findOneAnd*: the document (before or after) as a row, and what happened as a notice.
#[tokio::test]
async fn find_one_and_modify() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let update = "db.users.findOneAndUpdate({ name: \"Bob\" }, { $set: { age: 31 } }, { returnDocument: \"after\", projection: { name: 1, age: 1 } })";
        let (r, notices) = rows_and_notices(run(&*s, update, CAP).await);
        assert_eq!((column(&r, "name"), column(&r, "age")), (texts(&["Bob"]), texts(&["31"])));
        assert!(!r.columns.iter().any(|c| c.name == "email"), "{:?}", r.columns);
        assert_eq!(notices, ["Updated 1 document."]);
        // The document before the change, by default.
        let (r, notices) =
            rows_and_notices(run(&*s, "db.users.findOneAndUpdate({ name: \"Bob\" }, { $set: { age: 32 } })", CAP).await);
        assert_eq!((column(&r, "age"), notices), (texts(&["31"]), vec!["Updated 1 document.".to_string()]));

        let nobody = "db.users.findOneAndUpdate({ name: \"Nobody\" }, { $set: { age: 31 } }, { returnDocument: \"after\", projection: { name: 1, age: 1 } })";
        let (r, notices) = rows_and_notices(run(&*s, nobody, CAP).await);
        assert_eq!((r.rows.len(), notices), (0, vec!["No document matched.".to_string()]));

        let upsert = "db.users.findOneAndUpdate({ name: \"Zed\" }, { $set: { email: \"zed@x\" } }, { upsert: true })";
        let (r, notices) = rows_and_notices(run(&*s, upsert, CAP).await);
        assert_eq!(r.rows.len(), 0, "the document before the upsert: none");
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].starts_with("Upserted _id: ObjectId(\"") && notices[0].ends_with("\")."), "{notices:?}");
        assert_eq!(count(&*s, "users").await, "7");

        let replace = "db.users.findOneAndReplace({ name: \"Cid\" }, { name: \"Cid\", email: \"cid@x\", replaced: true }, { returnNewDocument: true })";
        let (r, notices) = rows_and_notices(run(&*s, replace, CAP).await);
        assert_eq!((column(&r, "replaced"), notices), (texts(&["true"]), vec!["Replaced 1 document.".to_string()]));

        let (r, notices) = rows_and_notices(run(&*s, "db.users.findOneAndDelete({ name: \"Fay\" })", CAP).await);
        assert_eq!((column(&r, "name"), notices), (texts(&["Fay"]), vec!["Deleted 1 document.".to_string()]));
        assert_eq!(count(&*s, "users").await, "6");
        let r = rows(run(&*s, "db.users.countDocuments({ name: \"Fay\" })", CAP).await);
        assert_eq!(r.rows, [texts(&["0"])]);
        let (r, notices) = rows_and_notices(run(&*s, "db.users.findOneAndDelete({ name: \"Fay\" })", CAP).await);
        assert_eq!((r.rows.len(), notices), (0, vec!["No document matched.".to_string()]));

        // A server error keeps its code.
        let (_, code, _) =
            failed(run(&*s, "db.users.findOneAndUpdate({ name: \"Ann\" }, { $set: { email: \"bob@example.com\" } })", CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_11000"));
        done(d, s).await;
    })
    .await;
}

/// The names of `coll`'s indexes, seen by a client of its own.
async fn index_names(url: &str, coll: &str) -> Vec<String> {
    let client = mongo_client(url).await;
    let names = client.database("fx").collection::<Document>(coll).list_index_names().await;
    names.unwrap_or_default()
}

async fn collection_names(url: &str) -> Vec<String> {
    mongo_client(url).await.database("fx").list_collection_names().await.unwrap()
}

/// createCollection, createIndex, getIndexes, dropIndex, renameCollection and drop: Done with a
/// notice, and what differs between servers looks the same.
#[tokio::test]
async fn collection_helpers() {
    for_each_mongo(|p, pw, url| async move {
        let (d, s) = open(&p, pw).await;
        assert_eq!(
            affected(run(&*s, "db.createCollection(\"p9_logs\")", CAP).await),
            (0, vec!["Created collection fx.p9_logs.".to_string()])
        );
        assert!(collection_names(&url).await.contains(&"p9_logs".to_string()));
        assert_eq!(
            affected(run(&*s, "db.p9_logs.createIndex({ at: -1 })", CAP).await),
            (0, vec!["Created index at_-1 on fx.p9_logs.".to_string()])
        );
        assert_eq!(
            affected(run(&*s, "db.p9_logs.createIndex({ at: -1 })", CAP).await),
            (0, vec!["Index at_-1 already exists.".to_string()])
        );
        assert_eq!(
            affected(run(&*s, "db.p9_new.createIndex({ a: 1 })", CAP).await),
            (0, vec!["Created index a_1 on fx.p9_new.".to_string(), "Created collection fx.p9_new for it.".to_string()])
        );
        // The name option, and options passed through as written.
        assert_eq!(
            affected(run(&*s, "db.p9_new.createIndex({ b: 1, c: -1 }, { name: \"bc\", unique: true })", CAP).await),
            (0, vec!["Created index bc on fx.p9_new.".to_string()])
        );
        let r = rows(run(&*s, "db.p9_new.getIndexes()", CAP).await);
        assert_eq!(column(&r, "name"), texts(&["_id_", "a_1", "bc"]));
        assert_eq!(column(&r, "unique"), [None, None, Some("true".to_string())]);

        let r = rows(run(&*s, "db.p9_logs.getIndexes()", CAP).await);
        assert_eq!((r.rows.len(), column(&r, "name")), (2, texts(&["_id_", "at_-1"])));
        assert_eq!(
            affected(run(&*s, "db.p9_logs.dropIndex(\"at_-1\")", CAP).await),
            (0, vec!["Dropped index at_-1.".to_string()])
        );
        let (_, code, _) = failed(run(&*s, "db.p9_logs.dropIndex(\"at_-1\")", CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_27"));
        assert_eq!(index_names(&url, "p9_logs").await, ["_id_"]);
        // By its keys.
        assert_eq!(
            affected(run(&*s, "db.p9_new.dropIndex({ a: 1 })", CAP).await),
            (0, vec!["Dropped index { a: 1 }.".to_string()])
        );
        assert_eq!(index_names(&url, "p9_new").await, ["_id_", "bc"]);

        assert_eq!(
            affected(run(&*s, "db.p9_logs.renameCollection(\"p9_logs2\")", CAP).await),
            (0, vec!["Renamed fx.p9_logs to fx.p9_logs2.".to_string()])
        );
        let names = collection_names(&url).await;
        assert!(names.contains(&"p9_logs2".to_string()) && !names.contains(&"p9_logs".to_string()), "{names:?}");
        // An existing target needs dropTarget (code 48 otherwise).
        let (_, code, _) = failed(run(&*s, "db.p9_new.renameCollection(\"p9_logs2\")", CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_48"));
        assert_eq!(
            affected(run(&*s, "db.p9_new.renameCollection(\"p9_logs2\", true)", CAP).await),
            (0, vec!["Renamed fx.p9_new to fx.p9_logs2.".to_string()])
        );
        assert_eq!(index_names(&url, "p9_logs2").await, ["_id_", "bc"]);

        assert_eq!(
            affected(run(&*s, "db.p9_logs2.drop()", CAP).await),
            (0, vec!["Dropped collection fx.p9_logs2.".to_string()])
        );
        assert_eq!(
            affected(run(&*s, "db.p9_logs2.drop()", CAP).await),
            (0, vec!["There was no collection fx.p9_logs2 to drop.".to_string()])
        );
        // A view's drop replies without nIndexesWas: it is dropped all the same.
        assert_eq!(
            affected(run(&*s, "db.createCollection(\"p9_view\", { viewOn: \"users\", pipeline: [] })", CAP).await),
            (0, vec!["Created collection fx.p9_view.".to_string()])
        );
        assert!(collection_names(&url).await.contains(&"p9_view".to_string()));
        assert_eq!(
            affected(run(&*s, "db.p9_view.drop()", CAP).await),
            (0, vec!["Dropped collection fx.p9_view.".to_string()])
        );
        assert!(!collection_names(&url).await.iter().any(|n| n.starts_with("p9_")));

        let r = rows(run(&*s, "db.p9_missing.getIndexes()", CAP).await);
        assert_eq!((r.rows.len(), r.truncated), (0, false));
        assert!(!collection_names(&url).await.contains(&"p9_missing".to_string()));

        // Through the runner the helpers are Done, getIndexes rows.
        let text = "db.createCollection(\"p9_ran\")\ndb.p9_ran.getIndexes()\ndb.p9_ran.drop()";
        let report = runner::run_in(&QueryLanguage::Mongo, &*s, &Environment::Local, &run_req(text, None), &|_| {})
            .await
            .unwrap();
        let outcomes: Vec<&Outcome> = report.statements.iter().map(|r| &r.outcome).collect();
        assert!(matches!(outcomes[..], [Outcome::Done, Outcome::Rows { .. }, Outcome::Done]), "{outcomes:?}");
        done(d, s).await;
    })
    .await;
}

/// A read-only profile refuses the helpers and findOneAnd* before sending
/// them; getIndexes runs.
#[tokio::test]
async fn read_only_refuses_the_helpers() {
    for_each_mongo(|mut p, pw, url| async move {
        p.read_only = true;
        let (d, s) = open(&p, pw).await;
        for (stmt, verb) in [
            ("db.users.drop()", "drop"),
            ("db.users.createIndex({ name: 1 })", "createIndex"),
            ("db.users.findOneAndDelete({ name: \"Ann\" })", "findOneAndDelete"),
            ("db.createCollection(\"p9_ro\")", "createCollection"),
        ] {
            let (message, code, notices) = failed(run(&*s, stmt, CAP).await);
            let refused =
                format!("This connection is read-only. Turn read-only off in the connection settings to run {verb}.");
            assert_eq!((message, code.as_deref()), (refused, Some("MONGO_READ_ONLY")), "{stmt}");
            assert!(notices.is_empty(), "{stmt}: {notices:?}");
        }
        assert_eq!(count_in(&url, "fx", "users").await, 6);
        assert_eq!(index_names(&url, "users").await, ["_id_", "email_1"]);
        assert!(!collection_names(&url).await.contains(&"p9_ro".to_string()));
        let r = rows(run(&*s, "db.users.getIndexes()", CAP).await);
        assert_eq!(column(&r, "name"), texts(&["_id_", "email_1"]));
        done(d, s).await;
    })
    .await;
}

/// The notice of an explain with the default verbosity.
const PLANNED_ONLY: &str = "Planned only. explain(\"executionStats\") also runs it and shows what it read.";

/// An explain's rows and notices.
fn explained(run: StatementRun) -> (Rows, Vec<String>) {
    let notices = run.notices.iter().map(|n| n.message.clone()).collect();
    (rows(run), notices)
}

/// explain of a find and an aggregate: one row per plan stage in the columns stage, index and
/// details; the stats with executionStats, "Planned only" without.
#[tokio::test]
async fn explain_shows_the_winning_plan() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let (r, notices) = explained(run(&*s, "db.users.find({ email: \"ann@example.com\" }).explain()", CAP).await);
        let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["stage", "index", "details"]);
        assert!(column(&r, "index").contains(&Some("email_1".to_string())), "{:?}", r.rows);
        assert_eq!(notices, [PLANNED_ONLY]);

        let stmt = "db.big.find({ n: { $gt: 5 } }).sort({ n: -1 }).explain(\"executionStats\")";
        let (r, notices) = explained(run(&*s, stmt, CAP).await);
        let stages = column(&r, "stage");
        for stage in ["COLLSCAN", "SORT"] {
            assert!(stages.contains(&Some(stage.to_string())), "{stage}: {stages:?}");
        }
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].starts_with("Returned "), "{notices:?}");

        let stmt = "db.users.aggregate([{ $match: { active: true } }, { $group: { _id: \"$active\", n: { $sum: 1 } } }]).explain()";
        let (r, notices) = explained(run(&*s, stmt, CAP).await);
        assert!(column(&r, "stage").contains(&Some("COLLSCAN".to_string())), "{:?}", r.rows);
        assert_eq!(notices, [PLANNED_ONLY]);
        done(d, s).await;
    })
    .await;
}

/// A statement that fails while explain runs it is its error, as when it is run: the server
/// replies ok, with `executionSuccess: false`.
#[tokio::test]
async fn explain_of_a_failing_statement_is_its_error() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let stmt = "db.users.find({ $where: \"throw new Error('boom')\" }).explain(\"executionStats\")";
        let (message, code, notices) = failed(run(&*s, stmt, CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_139"));
        assert!(message.starts_with("Error: boom"), "{message}");
        assert!(notices.is_empty(), "{notices:?}");
        done(d, s).await;
    })
    .await;
}

/// explain is a read: a read-only profile runs it.
#[tokio::test]
async fn read_only_runs_explain() {
    for_each_mongo(|mut p, pw, _url| async move {
        p.read_only = true;
        let (d, s) = open(&p, pw).await;
        let (r, notices) = explained(run(&*s, "db.users.find({ email: \"ann@example.com\" }).explain()", CAP).await);
        assert!(column(&r, "index").contains(&Some("email_1".to_string())), "{:?}", r.rows);
        assert_eq!(notices, [PLANNED_ONLY]);
        let (r, _) = explained(run(&*s, "db.users.aggregate([{ $match: { active: true } }]).explain(\"executionStats\")", CAP).await);
        assert!(column(&r, "stage").contains(&Some("COLLSCAN".to_string())), "{:?}", r.rows);
        done(d, s).await;
    })
    .await;
}

/// `coll`'s documents in `_id` order, seen by a client of its own.
async fn documents(url: &str, coll: &str) -> Vec<Document> {
    let client = mongo_client(url).await;
    let found = client.database("fx").collection::<Document>(coll).find(doc! {}).sort(doc! { "_id": 1 }).await.unwrap();
    found.try_collect().await.unwrap()
}

/// The `_id`s of `coll`'s documents, in order.
async fn ids(url: &str, coll: &str) -> Vec<Bson> {
    documents(url, coll).await.into_iter().filter_map(|d| d.get("_id").cloned()).collect()
}

fn ints(v: &[i32]) -> Vec<Bson> {
    v.iter().map(|n| Bson::Int32(*n)).collect()
}

/// Every kind of operation, in order: Affected, with the five counts.
#[tokio::test]
async fn bulk_write_runs_in_order() {
    for_each_mongo(|p, pw, url| async move {
        let (d, s) = open(&p, pw).await;
        let text = "db.p9_bulk.bulkWrite([
  { insertOne: { document: { _id: 1, n: 1 } } },
  { updateOne: { filter: { _id: 1 }, update: { $set: { n: 2 } } } },
  { updateMany: { filter: { _id: 2 }, update: { $set: { n: 5 } }, upsert: true } },
  { replaceOne: { filter: { _id: 2 }, replacement: { n: 6 } } },
  { deleteOne: { filter: { _id: 1 } } }
])";
        let report = runner::run_in(&QueryLanguage::Mongo, &*s, &Environment::Local, &run_req(text, None), &|_| {})
            .await
            .unwrap();
        assert_eq!(report.statements.len(), 1);
        assert_eq!(report.statements[0].outcome, Outcome::Affected { rows: 5 });
        let notices: Vec<&str> = report.statements[0].notices.iter().map(|n| n.message.as_str()).collect();
        assert_eq!(notices, ["Inserted 1, matched 2, modified 2, upserted 1, deleted 1."]);
        assert_eq!(documents(&url, "p9_bulk").await, [doc! { "_id": 2, "n": 6 }]);

        // A document without an _id gets one, as insertOne's.
        let (n, notices) = affected(run(&*s, "db.p9_bulk.bulkWrite([{ insertOne: { document: { n: 7 } } }])", CAP).await);
        assert_eq!((n, notices), (1, vec!["Inserted 1, matched 0, modified 0, upserted 0, deleted 0.".to_string()]));
        let docs = documents(&url, "p9_bulk").await;
        assert_eq!(docs.len(), 2);
        assert!(docs.iter().any(|d| matches!(d.get("_id"), Some(Bson::ObjectId(_)))), "{docs:?}");
        done(d, s).await;
    })
    .await;
}

/// An ordered bulk applies nothing after its first error, and says what ran.
#[tokio::test]
async fn ordered_bulk_stops_at_the_error() {
    for_each_mongo(|p, pw, url| async move {
        let (d, s) = open(&p, pw).await;
        let stmt = "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 2 } } }])";
        let (message, code, notices) = failed(run(&*s, stmt, CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_11000"));
        assert!(message.starts_with("E11000 duplicate key error"), "{message}");
        assert_eq!(
            notices,
            ["Inserted 1, matched 0, modified 0, upserted 0, deleted 0.", "Ran 1 of 3 operations before the error."]
        );
        assert_eq!(ids(&url, "p9_bulk").await, ints(&[1]));

        // The groups after the failed one aren't sent.
        let stmt = "db.p9_bulk2.bulkWrite([{ insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 1 } } }, { deleteMany: { filter: {} } }])";
        let (_, code, notices) = failed(run(&*s, stmt, CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_11000"));
        assert_eq!(
            notices,
            ["Inserted 1, matched 0, modified 0, upserted 0, deleted 0.", "Ran 1 of 3 operations before the error."]
        );
        assert_eq!(ids(&url, "p9_bulk2").await, ints(&[1]));

        // An error in a later group: the groups before it are counted.
        let stmt = "db.p9_bulk2.bulkWrite([{ updateOne: { filter: { _id: 1 }, update: { $set: { a: 1 } } } }, { deleteOne: { filter: { _id: 9 } } }, { insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 3 } } }])";
        let (_, code, notices) = failed(run(&*s, stmt, CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_11000"));
        assert_eq!(
            notices,
            ["Inserted 0, matched 1, modified 1, upserted 0, deleted 0.", "Ran 2 of 4 operations before the error."]
        );
        assert_eq!(documents(&url, "p9_bulk2").await, [doc! { "_id": 1, "a": 1 }]);
        done(d, s).await;
    })
    .await;
}

/// An unordered bulk runs every operation it can, and says how many failed.
#[tokio::test]
async fn unordered_bulk_runs_the_rest() {
    for_each_mongo(|p, pw, url| async move {
        let (d, s) = open(&p, pw).await;
        let stmt = "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 2 } } }], { ordered: false })";
        let (message, code, notices) = failed(run(&*s, stmt, CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_11000"));
        assert!(message.starts_with("E11000 duplicate key error"), "{message}");
        assert_eq!(
            notices,
            ["Inserted 2, matched 0, modified 0, upserted 0, deleted 0.", "1 of 3 operations failed; the others ran."]
        );
        assert_eq!(ids(&url, "p9_bulk").await, ints(&[1, 2]));

        // Across groups: the later groups run, every failure is counted, the first is the error.
        let stmt = "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 3 } } }, { insertOne: { document: { _id: 1 } } }, { updateOne: { filter: { _id: 2 }, update: { $set: { a: 1 } } } }, { updateOne: { filter: { _id: 3 }, update: { $inc: { _id: 1 } } } }, { insertOne: { document: { _id: 2 } } }], { ordered: false })";
        let (message, code, notices) = failed(run(&*s, stmt, CAP).await);
        assert_eq!(code.as_deref(), Some("MONGO_11000"));
        assert!(message.contains("{ _id: 1 }"), "the first error by operation order: {message}");
        assert_eq!(
            notices,
            ["Inserted 1, matched 1, modified 1, upserted 0, deleted 0.", "3 of 5 operations failed; the others ran."]
        );
        assert_eq!(documents(&url, "p9_bulk").await, [doc! { "_id": 1 }, doc! { "_id": 2, "a": 1 }, doc! { "_id": 3 }]);
        done(d, s).await;
    })
    .await;
}

/// Production holds a bulkWrite for confirmation; read-only refuses it before
/// sending it.
#[tokio::test]
async fn bulk_write_production_and_read_only() {
    for_each_mongo(|mut p, pw, url| async move {
        let text = "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 1 } } }, { deleteMany: { filter: {} } }])";
        let (d, s) = open(&p, pw.clone()).await;
        let report = runner::run_in(&QueryLanguage::Mongo, &*s, &Environment::Production, &run_req(text, None), &|_| {})
            .await
            .unwrap();
        assert_eq!(
            report.outcome,
            RunOutcome::NeedsConfirmation {
                statements: vec![PendingWrite { verb: "bulkWrite".into(), preview: text.into() }]
            }
        );
        assert!(report.statements.is_empty());
        assert!(!collection_names(&url).await.contains(&"p9_bulk".to_string()));
        let mut confirmed = run_req(text, None);
        confirmed.confirmed = true;
        let report = runner::run_in(&QueryLanguage::Mongo, &*s, &Environment::Production, &confirmed, &|_| {})
            .await
            .unwrap();
        assert_eq!(report.statements[0].outcome, Outcome::Affected { rows: 2 });
        done(d, s).await;

        p.read_only = true;
        let (d, s) = open(&p, pw).await;
        let (message, code, notices) =
            failed(run(&*s, "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 2 } } }])", CAP).await);
        let refused = "This connection is read-only. Turn read-only off in the connection settings to run bulkWrite.";
        assert_eq!((message.as_str(), code.as_deref()), (refused, Some("MONGO_READ_ONLY")));
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(count_in(&url, "fx", "p9_bulk").await, 0);
        assert_eq!(count_in(&url, "fx", "users").await, 6);
        done(d, s).await;
    })
    .await;
}

// Stop. Each run has its own `ClientSession` and the comment `ts-run-<uuid>`; Stop drops the
// operation and kills the session on the server (killSessions).

/// A filter that takes 100 ms a document: a find or an update on `fx.big` runs for minutes.
const SLOW: &str = "{ $where: \"sleep(100) || true\" }";
const STOPPED_WRITE: &str = "Stopped. Some documents may already have been changed.";

/// The app's session type itself, for its test hooks, on a client built as the driver builds
/// it; and that client.
async fn stoppable(p: &ConnectionProfile, pw: Option<String>) -> (Arc<MongoQuerySession>, mongodb::Client) {
    let client = mongodb::Client::with_options(MongoConfig::from_profile(p, pw).unwrap()).unwrap();
    // As the driver's: at least 1 s.
    let timeout = Duration::from_secs(u64::from(p.connect_timeout_secs).max(1));
    (Arc::new(MongoQuerySession::new(client.clone(), "fx".into(), timeout, false, None)), client)
}

/// Runs `stmt` on `s` in a task of its own.
fn spawn_run(s: &Arc<MongoQuerySession>, stmt: &str) -> tokio::task::JoinHandle<StatementRun> {
    let (s, stmt) = (s.clone(), stmt.to_string());
    tokio::spawn(async move { run(&*s, &stmt, CAP).await })
}

/// The comment of the run in progress on `s`, once it has registered, found without sleeping.
/// The run registers before it awaits anything; on a client that hasn't connected yet nothing
/// has been sent by then either (the test runtime has one thread), but on a client with an open
/// connection the run's first poll may already have sent its operation.
async fn registered(s: &MongoQuerySession) -> String {
    for _ in 0..10_000 {
        if let Some(comment) = s.current_run_comment() {
            return comment;
        }
        tokio::task::yield_now().await;
    }
    panic!("the run never registered");
}

/// The server's operations (and idle cursors) with `comment`, seen by `observer`.
async fn ops_with(observer: &mongodb::Client, comment: &str) -> usize {
    let pipeline = [
        doc! { "$currentOp": { "allUsers": true, "idleCursors": true } },
        doc! {
            "$match": { "$or": [{ "command.comment": comment }, { "cursor.originatingCommand.comment": comment }] }
        },
    ];
    let cursor = observer.database("admin").aggregate(pipeline).await.unwrap();
    cursor.try_collect::<Vec<Document>>().await.unwrap().len()
}

/// Waits (up to 5 s) until the server runs the operation with `comment`.
async fn running(observer: &mongodb::Client, comment: &str) {
    let started = Instant::now();
    while ops_with(observer, comment).await == 0 {
        assert!(started.elapsed() < Duration::from_secs(5), "{comment} never reached the server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits (up to 2 s) until the server no longer has an operation with `comment`: a kill takes a
/// moment to land.
async fn gone(observer: &mongodb::Client, comment: &str) {
    let started = Instant::now();
    while ops_with(observer, comment).await > 0 {
        assert!(started.elapsed() < Duration::from_secs(2), "{comment} still runs");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The run's result, which must come within `limit`.
async fn within(limit: Duration, task: tokio::task::JoinHandle<StatementRun>) -> StatementRun {
    tokio::time::timeout(limit, task).await.unwrap_or_else(|_| panic!("no result within {limit:?}")).unwrap()
}

fn assert_canceled(run: &StatementRun, notices: &[&str]) {
    let canceled = RawOutcome::Error { message: "Canceled".into(), code: Some(CANCELED_CODE.into()), position: None };
    assert_eq!(run.outcome, canceled);
    assert_eq!(run.notices.iter().map(|n| n.message.as_str()).collect::<Vec<_>>(), notices);
}

async fn updated(observer: &mongodb::Client) -> u64 {
    observer.database("fx").collection::<Document>("big").count_documents(doc! { "s": 1 }).await.unwrap()
}

/// Kills the server's operations with `comment` when the test panics before they end, so a
/// failing Stop test can't leave its `$where` sleep running into the next test (holding
/// `fx.big`, and changing it). `Drop` can't await on the test's runtime: the kill runs on a
/// thread of its own, with a runtime and a client of its own.
struct KillOnPanic {
    url: String,
    comment: String,
}

fn kill_on_panic(url: &str, comment: &str) -> KillOnPanic {
    KillOnPanic { url: url.into(), comment: comment.into() }
}

impl Drop for KillOnPanic {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let (url, comment) = (self.url.clone(), self.comment.clone());
        let killed = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let client = mongo_client(&url).await;
                kill_by_comment(&client, &comment, Duration::from_secs(5)).await;
                client.shutdown().immediate(true).await;
            });
        })
        .join();
        if killed.is_err() {
            eprintln!("couldn't kill {} after the test failed", self.comment);
        }
    }
}

#[tokio::test]
async fn stop_cancels_a_slow_find() {
    for_each_mongo(|p, pw, url| async move {
        let (s, _client) = stoppable(&p, pw).await;
        let observer = mongo_client(&url).await;
        let started = Instant::now();
        let task = spawn_run(&s, &format!("db.big.find({SLOW})"));
        let comment = registered(&s).await;
        let _kill = kill_on_panic(&url, &comment);
        assert!(comment.starts_with("ts-run-"), "{comment}");
        running(&observer, &comment).await;
        tokio::time::sleep(Duration::from_millis(300).saturating_sub(started.elapsed())).await;
        assert_eq!(s.cancel().await, Ok(()));
        assert_canceled(&within(Duration::from_secs(2), task).await, &[]);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(ops_with(&observer, &comment).await, 0);
        assert_eq!(s.current_run_comment(), None);
        // The session runs the next statement as usual.
        assert_eq!(rows(run(&*s, "db.users.find()", CAP).await).rows.len(), 6);
        s.close().await;
    })
    .await;
}

/// Dropping a write's future doesn't stop it on the server; Stop must.
#[tokio::test]
async fn stop_halts_a_running_update_many_on_the_server() {
    for_each_mongo(|p, pw, url| async move {
        let (s, _client) = stoppable(&p, pw).await;
        let observer = mongo_client(&url).await;
        let started = Instant::now();
        let task = spawn_run(&s, &format!("db.big.updateMany({SLOW}, {{ $set: {{ s: 1 }} }})"));
        let comment = registered(&s).await;
        let _kill = kill_on_panic(&url, &comment);
        running(&observer, &comment).await;
        tokio::time::sleep(Duration::from_secs(1).saturating_sub(started.elapsed())).await;
        assert_eq!(s.cancel().await, Ok(()));
        let stopped = Instant::now();
        assert_canceled(&within(Duration::from_secs(2), task).await, &[STOPPED_WRITE]);
        tokio::time::sleep(Duration::from_millis(500).saturating_sub(stopped.elapsed())).await;
        let at_half = updated(&observer).await;
        tokio::time::sleep(Duration::from_millis(1500).saturating_sub(stopped.elapsed())).await;
        let later = updated(&observer).await;
        assert!(at_half > 0, "the update changed nothing before Stop");
        assert_eq!(at_half, later, "the update went on after Stop");
        assert!(later < BIG_ROWS as u64, "{later}");
        assert_eq!(ops_with(&observer, &comment).await, 0);
        s.close().await;
    })
    .await;
}

/// Stop of a findOneAndUpdate: `findAndModify` runs in the run's session with its comment, so
/// Stop kills it. The sort on an unindexed field makes the server run the slow filter on every
/// document before it picks one (without it the first document matches after 100 ms).
#[tokio::test]
async fn stop_cancels_a_find_one_and_update() {
    for_each_mongo(|p, pw, url| async move {
        let (s, _client) = stoppable(&p, pw).await;
        let observer = mongo_client(&url).await;
        let started = Instant::now();
        let task = spawn_run(&s, &format!("db.big.findOneAndUpdate({SLOW}, {{ $set: {{ s: 1 }} }}, {{ sort: {{ n: 1 }} }})"));
        let comment = registered(&s).await;
        let _kill = kill_on_panic(&url, &comment);
        running(&observer, &comment).await;
        tokio::time::sleep(Duration::from_millis(500).saturating_sub(started.elapsed())).await;
        assert_eq!(s.cancel().await, Ok(()));
        assert_canceled(&within(Duration::from_secs(2), task).await, &[STOPPED_WRITE]);
        gone(&observer, &comment).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(ops_with(&observer, &comment).await, 0);
        assert_eq!(updated(&observer).await, 0);
        assert_eq!(s.current_run_comment(), None);
        s.close().await;
    })
    .await;
}

/// Stop through a bulkWrite, the one write that sends several commands: the group in flight is
/// killed, and the groups after it are never sent.
#[tokio::test]
async fn stop_through_a_bulk_write_sends_no_later_group() {
    for_each_mongo(|p, pw, url| async move {
        let (s, _client) = stoppable(&p, pw).await;
        let observer = mongo_client(&url).await;
        let started = Instant::now();
        let stmt = format!(
            "db.big.bulkWrite([{{ updateMany: {{ filter: {SLOW}, update: {{ $set: {{ s: 1 }} }} }} }}, {{ insertOne: {{ document: {{ _id: \"p9_stop\" }} }} }}])"
        );
        let task = spawn_run(&s, &stmt);
        let comment = registered(&s).await;
        let _kill = kill_on_panic(&url, &comment);
        running(&observer, &comment).await;
        tokio::time::sleep(Duration::from_millis(500).saturating_sub(started.elapsed())).await;
        assert_eq!(s.cancel().await, Ok(()));
        assert_canceled(&within(Duration::from_secs(2), task).await, &[STOPPED_WRITE]);
        gone(&observer, &comment).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(ops_with(&observer, &comment).await, 0);
        let big = observer.database("fx").collection::<Document>("big");
        assert_eq!(big.count_documents(doc! { "_id": "p9_stop" }).await.unwrap(), 0, "the later group was sent");
        assert!(updated(&observer).await < BIG_ROWS as u64);
        assert_eq!(s.current_run_comment(), None);
        s.close().await;
    })
    .await;
}

/// A Stop that lands before the operation is sent: killSessions would kill nothing then (fact
/// 66), so the operation must never be sent. Each run is the first on a new client, which
/// connects before it sends anything, so the Stop lands in that window. (On a client with an
/// open connection the run's first poll may already send the operation; killSessions stops it
/// then, as in the other tests.)
#[tokio::test]
async fn stop_before_send_sends_nothing() {
    for_each_mongo(|p, pw, url| async move {
        let observer = mongo_client(&url).await;
        let (s, _client) = stoppable(&p, pw.clone()).await;
        let task = spawn_run(&s, &format!("db.big.find({SLOW})"));
        let comment = registered(&s).await;
        let _kill = kill_on_panic(&url, &comment);
        assert_eq!(s.cancel().await, Ok(()));
        assert_canceled(&within(Duration::from_secs(2), task).await, &[]);
        for _ in 0..5 {
            assert_eq!(ops_with(&observer, &comment).await, 0, "{comment} reached the server");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        s.close().await;
        // A fast write stopped before it is sent changes nothing.
        let (s, _client) = stoppable(&p, pw).await;
        let task = spawn_run(&s, "db.big.updateMany({}, { $set: { s: 1 } })");
        let _kill_update = kill_on_panic(&url, &registered(&s).await);
        assert_eq!(s.cancel().await, Ok(()));
        assert_canceled(&within(Duration::from_secs(2), task).await, &[STOPPED_WRITE]);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(updated(&observer).await, 0);
        s.close().await;
    })
    .await;
}

/// The fallback for a server without killSessions: currentOp by comment, then killOp. The
/// server's Interrupted (11601) is a Stop too.
#[tokio::test]
async fn fallback_kill_by_comment() {
    for_each_mongo(|p, pw, url| async move {
        let (s, client) = stoppable(&p, pw).await;
        let observer = mongo_client(&url).await;
        let task = spawn_run(&s, &format!("db.big.find({SLOW})"));
        let comment = registered(&s).await;
        let _kill = kill_on_panic(&url, &comment);
        running(&observer, &comment).await;
        kill_by_comment(&client, &comment, Duration::from_secs(5)).await;
        assert_canceled(&within(Duration::from_secs(2), task).await, &[]);
        assert_eq!(ops_with(&observer, &comment).await, 0);
        s.close().await;
    })
    .await;
}

#[tokio::test]
async fn stop_with_nothing_running_is_ok() {
    for_each_mongo(|p, pw, _url| async move {
        let (s, _client) = stoppable(&p, pw).await;
        assert_eq!(s.cancel().await, Ok(()));
        assert_eq!(s.current_run_comment(), None);
        // No Stop is kept for the next run.
        assert_eq!(rows(run(&*s, "db.users.find()", CAP).await).rows.len(), 6);
        assert_eq!(s.cancel().await, Ok(()));
        assert_eq!(count(&*s, "users").await, "6");
        s.close().await;
        s.close().await;
    })
    .await;
}

#[tokio::test]
async fn close_stops_a_run() {
    for_each_mongo(|p, pw, url| async move {
        let (s, _client) = stoppable(&p, pw).await;
        let observer = mongo_client(&url).await;
        let task = spawn_run(&s, &format!("db.big.updateMany({SLOW}, {{ $set: {{ s: 1 }} }})"));
        let comment = registered(&s).await;
        let _kill = kill_on_panic(&url, &comment);
        running(&observer, &comment).await;
        let closing = Instant::now();
        s.close().await;
        assert!(closing.elapsed() < Duration::from_secs(2), "{:?}", closing.elapsed());
        // `close` returns once the run is over and the server has been told to stop it.
        assert_eq!(s.current_run_comment(), None);
        gone(&observer, &comment).await;
        assert_canceled(&within(Duration::from_secs(2), task).await, &[STOPPED_WRITE]);
        let after = updated(&observer).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(updated(&observer).await, after);
    })
    .await;
}

/// Whether the server runs an update of `fx.big` with `comment`.
async fn updating_big(observer: &mongodb::Client, comment: &str) -> bool {
    let pipeline = [
        doc! { "$currentOp": { "allUsers": true } },
        doc! { "$match": { "command.comment": comment, "op": "update", "ns": "fx.big" } },
    ];
    let cursor = observer.database("admin").aggregate(pipeline).await.unwrap();
    !cursor.try_collect::<Vec<Document>>().await.unwrap().is_empty()
}

/// Stop through the runner, on a real session: the write is canceled with its warning in the
/// report, the statement before it keeps its result, and the rest is skipped.
#[tokio::test]
async fn stop_through_the_runner() {
    for_each_mongo(|p, pw, url| async move {
        let (s, _client) = stoppable(&p, pw).await;
        let observer = mongo_client(&url).await;
        let text = format!(
            "db.users.insertOne({{ _id: 800, name: \"Hal\" }})\ndb.big.updateMany({SLOW}, {{ $set: {{ s: 1 }} }})\ndb.users.find()"
        );
        let task = {
            let s = s.clone();
            tokio::spawn(async move {
                runner::run_in(&QueryLanguage::Mongo, &*s, &Environment::Local, &run_req(&text, None), &|_| {}).await
            })
        };
        // The insert's run comes and goes; Stop once the update runs on the server.
        let started = Instant::now();
        let comment = loop {
            if let Some(comment) = s.current_run_comment()
                && updating_big(&observer, &comment).await
            {
                break comment;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "the update never reached the server");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let _kill = kill_on_panic(&url, &comment);
        assert_eq!(s.cancel().await, Ok(()));
        let report = tokio::time::timeout(Duration::from_secs(5), task).await.expect("the run ends").unwrap().unwrap();
        let outcomes: Vec<&Outcome> = report.statements.iter().map(|r| &r.outcome).collect();
        assert_eq!(outcomes, [&Outcome::Affected { rows: 1 }, &Outcome::Canceled, &Outcome::Skipped]);
        let notices: Vec<(&str, &str)> =
            report.statements[1].notices.iter().map(|n| (n.severity.as_str(), n.message.as_str())).collect();
        assert_eq!(notices, [("warning", STOPPED_WRITE)]);
        assert!(matches!(report.outcome, porthole_lib::query::RunOutcome::Completed { statement_count: 3, .. }));
        gone(&observer, &comment).await;
        // The insert before it stays.
        assert_eq!(rows(run(&*s, "db.users.find({ _id: 800 })", CAP).await).rows.len(), 1);
        s.close().await;
    })
    .await;
}

// The statement time limit (`statementTimeoutSecs`): a statement still running after it is
// stopped as by Stop, and is an error, not Canceled.

const LIMIT: Duration = Duration::from_secs(1);

/// As `stoppable`, with a statement time limit.
async fn limited(p: &ConnectionProfile, pw: Option<String>, limit: Option<Duration>) -> Arc<MongoQuerySession> {
    let client = mongodb::Client::with_options(MongoConfig::from_profile(p, pw).unwrap()).unwrap();
    let timeout = Duration::from_secs(u64::from(p.connect_timeout_secs).max(1));
    Arc::new(MongoQuerySession::new(client, "fx".into(), timeout, false, limit))
}

/// How long a limited run may take: the limit, the kill's bounded wait (the profile's timeout)
/// and a second of slack.
fn limit_bound(p: &ConnectionProfile) -> Duration {
    LIMIT + Duration::from_secs(u64::from(p.connect_timeout_secs).max(1)) + Duration::from_secs(1)
}

fn timed_out() -> RawOutcome {
    RawOutcome::Error {
        message: "Stopped after 1 s, the connection's statement time limit.".into(),
        code: Some(TIMEOUT_CODE.into()),
        position: None,
    }
}

#[tokio::test]
async fn statement_limit_stops_a_read() {
    for_each_mongo(|p, pw, url| async move {
        let s = limited(&p, pw, Some(LIMIT)).await;
        let observer = mongo_client(&url).await;
        let started = Instant::now();
        let task = spawn_run(&s, &format!("db.big.find({SLOW})"));
        let comment = registered(&s).await;
        let _kill = kill_on_panic(&url, &comment);
        running(&observer, &comment).await;
        let run = within(limit_bound(&p).saturating_sub(started.elapsed()), task).await;
        assert!(started.elapsed() >= LIMIT, "stopped after {:?}", started.elapsed());
        assert_eq!(run.outcome, timed_out());
        assert!(run.notices.is_empty(), "{:?}", run.notices);
        gone(&observer, &comment).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(ops_with(&observer, &comment).await, 0);
        assert_eq!(s.current_run_comment(), None);
        s.close().await;
    })
    .await;
}

/// A write stopped by the limit warns, and nothing keeps running on the server.
#[tokio::test]
async fn statement_limit_stops_a_write() {
    for_each_mongo(|p, pw, url| async move {
        let s = limited(&p, pw, Some(LIMIT)).await;
        let observer = mongo_client(&url).await;
        let started = Instant::now();
        let task = spawn_run(&s, &format!("db.big.updateMany({SLOW}, {{ $set: {{ s: 1 }} }})"));
        let comment = registered(&s).await;
        let _kill = kill_on_panic(&url, &comment);
        running(&observer, &comment).await;
        let run = within(limit_bound(&p).saturating_sub(started.elapsed()), task).await;
        let stopped = Instant::now();
        assert!(started.elapsed() >= LIMIT, "stopped after {:?}", started.elapsed());
        assert_eq!(run.outcome, timed_out());
        assert_eq!(run.notices.iter().map(|n| (n.severity.as_str(), n.message.as_str())).collect::<Vec<_>>(), [("warning", STOPPED_WRITE)]);
        gone(&observer, &comment).await;
        tokio::time::sleep(Duration::from_millis(500).saturating_sub(stopped.elapsed())).await;
        let at_half = updated(&observer).await;
        tokio::time::sleep(Duration::from_millis(1500).saturating_sub(stopped.elapsed())).await;
        let later = updated(&observer).await;
        assert_eq!(at_half, later, "the update went on after the limit");
        assert!(later < BIG_ROWS as u64, "{later}");
        assert_eq!(ops_with(&observer, &comment).await, 0);
        s.close().await;
    })
    .await;
}

#[tokio::test]
async fn under_the_limit_runs_normally() {
    for_each_mongo(|p, pw, _url| async move {
        let s = limited(&p, pw, Some(LIMIT)).await;
        let run = run(&*s, "db.users.find()", CAP).await;
        assert!(run.notices.is_empty(), "{:?}", run.notices);
        assert_eq!(rows(run).rows.len(), 6);
        s.close().await;
    })
    .await;
}

/// Without a limit a statement runs to its end, however long it takes.
#[tokio::test]
async fn no_limit_is_todays_behaviour() {
    for_each_mongo(|p, pw, _url| async move {
        let s = limited(&p, pw, None).await;
        let started = Instant::now();
        let run = run(&*s, "db.big.find({ $where: \"sleep(2000) || true\" }).limit(1)", CAP).await;
        assert!(started.elapsed() >= Duration::from_secs(2), "{:?}", started.elapsed());
        assert_eq!(rows(run).rows.len(), 1);
        s.close().await;
    })
    .await;
}

/// The profile's limit reaches the driver's tabs, and the runner skips the rest after it, as
/// after any error. The slow statement ends by itself after 1.5 s (`limit(1)`).
#[tokio::test]
async fn the_profiles_limit_reaches_its_tabs() {
    for_each_mongo(|p, pw, _url| async move {
        let p = ConnectionProfile { statement_timeout_secs: Some(1), ..p };
        let (d, s) = open(&p, pw).await;
        let text = "db.big.find({ $where: \"sleep(1500) || true\" }).limit(1)\ndb.users.find()";
        let report = runner::run_in(&QueryLanguage::Mongo, &*s, &Environment::Local, &run_req(text, None), &|_| {}).await.unwrap();
        let outcomes: Vec<&Outcome> = report.statements.iter().map(|r| &r.outcome).collect();
        let RawOutcome::Error { message, code, position } = timed_out() else { unreachable!() };
        assert_eq!(outcomes, [&Outcome::Error { message, code, position }, &Outcome::Skipped]);
        done(d, s).await;
    })
    .await;
}

// Export of a query result: `find` and read `aggregate` stream every document (no cap) through a
// plain cursor into the file.

async fn export(
    s: &dyn QuerySession,
    stmt: &str,
    format: ExportFormat,
    target: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> porthole_lib::error::AppResult<ExportOutcome> {
    let call = parse(stmt, &ParamValues::new()).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    run_mongo_export(s, &call, format, target, cancel, &|_| {}).await
}

fn no_part_files(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".part"))
}

#[tokio::test]
async fn export_big_find_json_has_every_document() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("big.json");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let out = export(&*s, "db.big.find().sort({ _id: 1 })", ExportFormat::Json, &target, &cancel).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows, .. } if rows == BIG_ROWS as u64), "{out:?}");
        let text = std::fs::read_to_string(&target).unwrap();
        let Some(Bson::Array(items)) = parse_json(&text) else { panic!("not an array") };
        assert_eq!(items.len(), BIG_ROWS as usize);
        let Bson::Document(last) = &items[BIG_ROWS as usize - 1] else { panic!() };
        // Key order is kept.
        assert_eq!(last.keys().collect::<Vec<_>>(), ["_id", "n", "tag"]);
        assert_eq!(last.get("_id"), Some(&Bson::Int32(BIG_ROWS - 1)));
        // A read aggregate and a limit export too.
        let out = export(&*s, "db.big.aggregate([{ $match: { n: 3 } }])", ExportFormat::Json, &target, &cancel)
            .await
            .unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows, .. } if rows == BIG_ROWS as u64 / 100), "{out:?}");
        let out = export(&*s, "db.big.find().limit(7)", ExportFormat::Json, &target, &cancel).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 7, .. }), "{out:?}");
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn export_csv_columns_from_first_thousand() {
    for_each_mongo(|p, pw, url| async move {
        let (d, s) = open(&p, pw).await;
        let coll = mongo_client(&url).await.database("fx").collection::<Document>("export_sample");
        coll.drop().await.unwrap();
        let docs: Vec<Document> =
            (0..1_500).map(|i| if i < 1_000 { doc! { "_id": i, "a": i } } else { doc! { "_id": i, "a": i, "late": 1 } }).collect();
        coll.insert_many(docs).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sample.csv");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let out = export(&*s, "db.export_sample.find().sort({ _id: 1 })", ExportFormat::Csv, &target, &cancel).await;
        coll.drop().await.unwrap();
        assert!(matches!(out, Ok(ExportOutcome::Done { rows: 1_500, .. })), "{out:?}");
        let text = std::fs::read_to_string(&target).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // `late` first appears in document 1000, past the sample: left out.
        assert_eq!(lines[0], "_id,a");
        assert_eq!(lines.len(), 1_501);
        assert_eq!(lines[1_500], "1499,1499");
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn export_refuses_writes_and_out_stage() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.json");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        for stmt in [
            "db.users.insertOne({ _id: 900 })",
            "db.users.deleteMany({})",
            "db.users.aggregate([{ $match: {} }, { $out: \"copy\" }])",
            "db.users.aggregate([{ $merge: { into: \"copy\" } }])",
            "db.users.countDocuments({})",
            "db.users.estimatedDocumentCount()",
        ] {
            let err = export(&*s, stmt, ExportFormat::Json, &target, &cancel).await.unwrap_err();
            assert_eq!(
                err,
                porthole_lib::error::AppError::Query("Only a statement that reads rows can be run again for an export.".into()),
                "{stmt}"
            );
        }
        assert!(!target.exists());
        assert!(no_part_files(dir.path()));
        // Nothing ran.
        assert_eq!(count(&*s, "users").await, "6");
        done(d, s).await;
    })
    .await;
}

#[tokio::test]
async fn export_cancel_stops() {
    for_each_mongo(|p, pw, url| async move {
        let (d, s) = open(&p, pw).await;
        let observer = mongo_client(&url).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("slow.json");
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = {
            let (s, target, cancel) = (s.clone(), target.clone(), cancel.clone());
            let stmt = format!("db.big.find({SLOW})");
            tokio::spawn(async move { export(&*s, &stmt, ExportFormat::Json, &target, &cancel).await })
        };
        let _kill = kill_on_panic(&url, EXPORT_COMMENT);
        running(&observer, EXPORT_COMMENT).await;
        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        let out = tokio::time::timeout(Duration::from_secs(5), task).await.expect("cancel").unwrap().unwrap();
        assert_eq!(out, ExportOutcome::Canceled);
        assert!(!target.exists());
        assert!(no_part_files(dir.path()));
        // Dropping the future ended the server's find.
        gone(&observer, EXPORT_COMMENT).await;
        done(d, s).await;
    })
    .await;
}

async fn export_with(
    s: &dyn QuerySession,
    stmt: &str,
    format: ExportFormat,
    target: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &(dyn Fn(u64) + Sync),
) -> porthole_lib::error::AppResult<ExportOutcome> {
    let call = parse(stmt, &ParamValues::new()).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    run_mongo_export(s, &call, format, target, cancel, progress).await
}

/// A `distinct` exports one row per value (a truncated one's "export again"): one column named
/// after the field, or one document `{ <field>: value }` per value.
#[tokio::test]
async fn export_distinct_has_every_value() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        // More values than the cap: the run is truncated, and offers the export.
        assert!(rows(run(&*s, "db.big.distinct(\"_id\")", CAP).await).truncated);
        let dir = tempfile::tempdir().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let json = dir.path().join("ids.json");
        let out = export(&*s, "db.big.distinct(\"_id\")", ExportFormat::Json, &json, &cancel).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows, .. } if rows == BIG_ROWS as u64), "{out:?}");
        let Some(Bson::Array(items)) = parse_json(&std::fs::read_to_string(&json).unwrap()) else { panic!("not an array") };
        assert_eq!(items.len(), BIG_ROWS as usize);
        assert_eq!(items[7], Bson::Document(doc! { "_id": 7 }));
        let csv = dir.path().join("n.csv");
        let out = export(&*s, "db.big.distinct(\"n\", { n: { $lt: 3 } })", ExportFormat::Csv, &csv, &cancel).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 3, .. }), "{out:?}");
        assert_eq!(std::fs::read_to_string(&csv).unwrap(), "n\r\n0\r\n1\r\n2\r\n");
        // No value: the field's column alone.
        let out = export(&*s, "db.users.distinct(\"nope\")", ExportFormat::Csv, &csv, &cancel).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 0, .. }), "{out:?}");
        assert_eq!(std::fs::read_to_string(&csv).unwrap(), "nope\r\n");
        done(d, s).await;
    })
    .await;
}

/// An empty result is an empty file: `[]` in JSON, and in CSV only the (empty) header, since
/// no document names a column. A limit of `i64::MIN` exports with no limit's sign trouble.
#[tokio::test]
async fn export_edge_cases() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let dir = tempfile::tempdir().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let json = dir.path().join("empty.json");
        let out = export(&*s, "db.users.find({ _id: -1 })", ExportFormat::Json, &json, &cancel).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 0, .. }), "{out:?}");
        assert_eq!(std::fs::read_to_string(&json).unwrap(), "[]\n");
        let csv = dir.path().join("empty.csv");
        let out = export(&*s, "db.users.find({ _id: -1 })", ExportFormat::Csv, &csv, &cancel).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 0, .. }), "{out:?}");
        assert_eq!(std::fs::read_to_string(&csv).unwrap(), "\r\n");

        let stmt = "db.big.find().limit(NumberLong(\"-9223372036854775808\"))";
        let out = export(&*s, stmt, ExportFormat::Json, &json, &cancel).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows, .. } if rows == BIG_ROWS as u64), "{out:?}");

        // The export runs in the tab's database.
        s.set_search_path(Some("fx_other")).await.unwrap();
        let out = export(&*s, "db.orders.find()", ExportFormat::Json, &json, &cancel).await.unwrap();
        assert!(matches!(out, ExportOutcome::Done { rows: 3, .. }), "{out:?}");

        // Canceled before it starts: nothing is written.
        let canceled = std::sync::atomic::AtomicBool::new(true);
        let untouched = dir.path().join("untouched.json");
        assert_eq!(export(&*s, "db.orders.find()", ExportFormat::Json, &untouched, &canceled).await, Ok(ExportOutcome::Canceled));
        assert!(!untouched.exists());
        assert!(no_part_files(dir.path()));
        done(d, s).await;
    })
    .await;
}

/// A cancel after rows were written: `Canceled`, no `.part` file left, and a file already at
/// the target is left as it was.
#[tokio::test]
async fn export_cancel_after_rows_keeps_the_old_file() {
    for_each_mongo(|p, pw, _url| async move {
        let (d, s) = open(&p, pw).await;
        let dir = tempfile::tempdir().unwrap();
        for format in [ExportFormat::Json, ExportFormat::Csv] {
            let target = dir.path().join("big.out");
            std::fs::write(&target, "old contents").unwrap();
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let written = std::sync::atomic::AtomicU64::new(0);
            let progress = |rows: u64| {
                written.store(rows, std::sync::atomic::Ordering::SeqCst);
                if rows >= 2_000 {
                    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            };
            let out = export_with(&*s, "db.big.find().sort({ _id: 1 })", format, &target, &cancel, &progress).await;
            assert_eq!(out, Ok(ExportOutcome::Canceled), "{format:?}");
            assert!(written.load(std::sync::atomic::Ordering::SeqCst) >= 2_000, "{format:?}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "old contents", "{format:?}");
            assert!(no_part_files(dir.path()), "{format:?}");
        }
        done(d, s).await;
    })
    .await;
}
