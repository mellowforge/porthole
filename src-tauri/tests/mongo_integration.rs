mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use porthole_lib::commands::describe;
use porthole_lib::driver::Driver;
use porthole_lib::driver::mongodb::connect_mongodb;
use porthole_lib::error::AppError;
use porthole_lib::model::*;
use porthole_lib::query::QueryLanguage;
use common::mongo::{
    for_each_mongo, has_auth, mongo_client, mongo_driver, mongo_profile, mongo_server_facts, mongo_urls, BIG_ROWS,
};
use mongodb::bson::{doc, Document};
use common::net;

fn table(name: &str) -> TableRef {
    TableRef { schema: "fx".into(), name: name.into() }
}

fn page_req(name: &str, filter: Option<&str>, sort: Option<(&str, bool)>, page: u32, page_size: u32) -> PageRequest {
    PageRequest {
        table: table(name),
        filter: filter.map(String::from),
        sort: sort.map(|(c, d)| SortSpec { column: c.into(), descending: d }),
        page,
        page_size,
    }
}

async fn page(d: &dyn Driver, name: &str, filter: Option<&str>) -> TablePage {
    d.fetch_page(&page_req(name, filter, None, 0, 100)).await.unwrap()
}

fn column<'a>(p: &'a TablePage, name: &str) -> &'a ColumnInfo {
    p.columns.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"))
}

/// Each row's cell in column `name`.
fn values(p: &TablePage, name: &str) -> Vec<Option<String>> {
    let at = p.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    p.rows.iter().map(|r| r[at].clone()).collect()
}


#[tokio::test]
async fn mongo_connects_and_reports_version() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        assert_eq!(d.kind(), DbKind::Mongodb);
        assert_eq!(d.query_language(), QueryLanguage::Mongo);
        let (hello, version) = mongo_server_facts(&url).await;
        let v = d.server_version().await.unwrap();
        assert_eq!(v, format!("MongoDB {version}"));
        // The default servers: 6.0 on 27060, 8.0 on 27080 and the replica set on 27082.
        for (port, want) in [(":27060", "MongoDB 6.0."), (":27080", "MongoDB 8.0."), (":27082", "MongoDB 8.0.")] {
            if url.contains(port) {
                assert!(v.starts_with(want), "{url}: {v}");
            }
        }
        // One by one exactly where the server has no transactions: on 27060 and 27080, not on
        // the replica set.
        let standalone = !hello.contains_key("setName") && hello.get_str("msg").ok() != Some("isdbgrid");
        if [":27060", ":27080", ":27082"].iter().any(|port| url.contains(port)) {
            assert_eq!(standalone, !url.contains(":27082"), "{url}");
        }
        assert_eq!(d.one_by_one_saves(), standalone, "{url}");
        let info = describe(&p, d.as_ref()).await.unwrap();
        assert_eq!(info.server_version, v);
        assert_eq!(info.schema, "fx");
        assert_eq!(info.one_by_one_saves, standalone, "{url}");
        assert!(d.list_databases().await.unwrap().is_empty());
        let s = d.open_query_session(None).await.expect("a MongoDB query session opens");
        s.close().await;
        assert!(d.schema_columns(None, "fx").await.unwrap().is_empty());
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mongo_field_paths_come_from_the_sample() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let paths = d.field_paths("fx", "users").await.unwrap();
        for want in ["_id", "name", "address", "address.city"] {
            assert!(paths.iter().any(|f| f == want), "{url}: {want} in {paths:?}");
        }
        let at = |name: &str| paths.iter().position(|f| f == name).unwrap();
        assert!(at("address") < at("address.city"), "{url}: {paths:?}");
        assert!(d.field_paths("fx", "p9_missing").await.unwrap().is_empty(), "{url}");
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mongo_bad_password_fails_at_connect() {
    for_each_mongo(|p, _pw, url| async move {
        if !has_auth(&url) {
            return;
        }
        let started = Instant::now();
        match connect_mongodb(&p, Some("wrong".into())).await {
            Err(AppError::Connection(m)) => assert!(m.contains("Authentication failed"), "{m}"),
            Err(e) => panic!("expected a connection error, got {e:?}"),
            Ok(_) => panic!("connected with a wrong password"),
        }
        assert!(started.elapsed() < Duration::from_secs(p.connect_timeout_secs.into()), "{:?}", started.elapsed());
    })
    .await;
}

#[tokio::test]
async fn mongo_closed_port_fails_within_the_timeout() {
    let mut p = mongo_profile(&mongo_urls()[0], "fx");
    net(&mut p).host = "127.0.0.1".into();
    net(&mut p).port = 27999;
    p.connect_timeout_secs = 2;
    let started = Instant::now();
    let r = connect_mongodb(&p, Some("test".into())).await;
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    match r {
        // The server and the first cause, not the driver's topology dump.
        Err(AppError::Connection(m)) => assert_eq!(m, "Couldn't reach the MongoDB server at 127.0.0.1:27999. Connection refused."),
        Err(e) => panic!("expected a connection error, got {e:?}"),
        Ok(_) => panic!("connected to a closed port"),
    }
}

#[tokio::test]
async fn mongo_lists_databases_and_collections() {
    for_each_mongo(|p, pw, _url| async move {
        let d = mongo_driver(&p, pw).await;
        let schemas = d.list_schemas(None).await.unwrap();
        assert!(schemas.contains(&"fx".to_string()) && schemas.contains(&"fx_other".to_string()), "{schemas:?}");
        for system in ["admin", "local", "config"] {
            assert!(!schemas.contains(&system.to_string()), "{schemas:?}");
        }
        let mut sorted = schemas.clone();
        sorted.sort();
        assert_eq!(schemas, sorted);
        let tree = d.schema_tree("fx").await.unwrap();
        assert_eq!(tree.tables, ["big", "keys", "typed", "users"]);
        assert_eq!(tree.views, ["active_users", "metrics"]);
        assert!(tree.functions.is_empty());
        assert_eq!(d.schema_tree("fx_other").await.unwrap().tables, ["orders"]);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mongo_read_user_sees_its_database() {
    for_each_mongo(|p, _pw, url| async move {
        if !has_auth(&url) {
            return;
        }
        let mut ro = p.clone();
        net(&mut ro).user = "ro".into();
        let d = mongo_driver(&ro, Some("ropw".into())).await;
        assert_eq!(d.list_schemas(None).await.unwrap(), ["fx"]);
        assert_eq!(d.schema_tree("fx").await.unwrap().tables, ["big", "keys", "typed", "users"]);
        assert_eq!(page(d.as_ref(), "users", None).await.rows.len(), 6);
        d.close().await;
    })
    .await;
}

/// A user who may read `fx.users` but not list the database's collections still sees it in the
/// sidebar and opens it (`authorizedCollections`).
#[tokio::test]
async fn mongo_find_only_user_sees_its_collection() {
    for_each_mongo(|p, _pw, url| async move {
        if !has_auth(&url) {
            return;
        }
        let client = mongo_client(&url).await;
        let admin = client.database("admin");
        let _ = admin.run_command(doc! { "dropUser": "fo" }).await;
        let _ = admin.run_command(doc! { "dropRole": "ts_find_users" }).await;
        let privileges = [doc! { "resource": { "db": "fx", "collection": "users" }, "actions": ["find", "listIndexes"] }];
        admin.run_command(doc! { "createRole": "ts_find_users", "privileges": privileges.to_vec(), "roles": [] }).await.unwrap();
        admin.run_command(doc! { "createUser": "fo", "pwd": "fopw", "roles": [{ "role": "ts_find_users", "db": "admin" }] }).await.unwrap();

        let mut fo = p.clone();
        net(&mut fo).user = "fo".into();
        let d = mongo_driver(&fo, Some("fopw".into())).await;
        assert_eq!(d.schema_tree("fx").await.unwrap().tables, ["users"], "{url}");
        assert_eq!(page(d.as_ref(), "users", None).await.rows.len(), 6, "{url}");
        assert_eq!(d.table_structure(&table("users")).await.unwrap().relation, RelationKind::Table, "{url}");
        d.close().await;

        admin.run_command(doc! { "dropUser": "fo" }).await.unwrap();
        admin.run_command(doc! { "dropRole": "ts_find_users" }).await.unwrap();
        client.shutdown().immediate(true).await;
    })
    .await;
}

#[tokio::test]
async fn mongo_page_columns_and_cells() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let users = page(d.as_ref(), "users", None).await;
        let names: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_id", "name", "email", "active", "created", "tags", "age", "score", "address"]);
        let age = column(&users, "age");
        assert_eq!(age.full_type, "int32, int64, double, string");
        assert_eq!(age.data_type, "int32");
        assert!(age.nullable);
        let id = column(&users, "_id");
        assert!(id.is_primary_key && id.read_only && !id.nullable);
        assert_eq!(id.data_type, "objectId");
        assert_eq!(column(&users, "score").full_type, "int32, double");
        assert!(column(&users, "address").nullable && !column(&users, "name").nullable);
        assert_eq!(users.primary_key, ["_id"]);
        assert_eq!(users.editing, Editing::Allowed);
        assert_eq!(
            values(&users, "age"),
            [Some("30"), Some("30"), Some("30.0"), Some("30"), Some("41"), None].map(|v| v.map(String::from))
        );
        let types = users.cell_types.as_ref().unwrap();
        // _id name email active created tags age score address
        assert_eq!(types[0], "ottydjiij", "{url}");
        assert_eq!(types[1], "ottydjlrj");
        assert_eq!(types[2], "ottydjrij");
        assert_eq!(types[3], "ottydjtrj");
        assert_eq!(types[5], "ottydjnrn");
        assert_eq!(values(&users, "created")[0].as_deref(), Some("2024-01-02T03:04:05.678Z"));
        assert_eq!(values(&users, "tags")[0].as_deref(), Some(r#"["admin","ops"]"#));
        assert_eq!(values(&users, "address")[0].as_deref(), Some(r#"{"city":"Oslo","zip":"0150"}"#));

        let typed = page(d.as_ref(), "typed", None).await;
        assert_eq!(typed.rows.len(), 1);
        let codes: Vec<char> = typed.cell_types.as_ref().unwrap()[0].chars().collect();
        let got: BTreeMap<&str, (Option<&str>, char, &str)> = typed
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.as_str(), (typed.rows[0][i].as_deref(), codes[i], c.data_type.as_str())))
            .collect();
        let want: [(&str, Option<&str>, char, &str); 34] = [
            ("_id", Some("1"), 'i', "int32"),
            ("int32", Some("42"), 'i', "int32"),
            ("int64", Some("42"), 'l', "int64"),
            ("int64_big", Some("9007199254740993"), 'l', "int64"),
            ("double_one", Some("1.0"), 'r', "double"),
            ("double_tenth", Some("0.1"), 'r', "double"),
            ("double_1e300", Some("1e+300"), 'r', "double"),
            ("double_1e21", Some("1e+21"), 'r', "double"),
            ("double_12345678", Some("12345678.0"), 'r', "double"),
            ("double_nan", Some("NaN"), 'r', "double"),
            ("double_neg_inf", Some("-Infinity"), 'r', "double"),
            ("double_neg_zero", Some("-0.0"), 'r', "double"),
            ("decimal", Some("1.10"), 'm', "decimal"),
            ("string", Some("héllo \"q\""), 't', "string"),
            ("bool", Some("true"), 'y', "bool"),
            ("null", None, 'n', "null"),
            ("oid", Some("65a1b2c3d4e5f60718293a4b"), 'o', "objectId"),
            ("date_ms", Some("2024-01-02T03:04:05.678Z"), 'd', "date"),
            ("date_pre_epoch", Some("1969-12-31T23:59:59Z"), 'd', "date"),
            ("date_year_10000", Some(r#"{"$date":{"$numberLong":"253402300800000"}}"#), 'd', "date"),
            ("binary", Some(r#"{"$binary":{"base64":"AQID/w==","subType":"00"}}"#), 'x', "binary"),
            ("uuid", Some(r#"{"$binary":{"base64":"ASNFZ4mrze8BI0VniavN7w==","subType":"04"}}"#), 'x', "uuid"),
            ("nested", Some(r#"{"a":1,"b":{"$numberLong":"2"},"c":{"d":[1.0,"x"]}}"#), 'j', "object"),
            ("nested_za", Some(r#"{"z":1,"a":{"$numberLong":"2"}}"#), 'j', "object"),
            ("nested_neg", Some(r#"{"a":-7,"b":[-1,2],"c":-2147483648,"d":{"$numberLong":"-5"}}"#), 'j', "object"),
            ("timestamp", Some(r#"{"$timestamp":{"i":7,"t":1700000000}}"#), 'x', "timestamp"),
            ("regex", Some(r#"{"$regularExpression":{"options":"i","pattern":"^ab+c"}}"#), 'x', "regex"),
            ("min_key", Some(r#"{"$minKey":1}"#), 'x', "other"),
            ("max_key", Some(r#"{"$maxKey":1}"#), 'x', "other"),
            ("undefined", Some(r#"{"$undefined":true}"#), 'x', "other"),
            ("code", Some(r#"{"$code":"return 1"}"#), 'x', "other"),
            ("symbol", Some(r#"{"$symbol":"sym"}"#), 'x', "other"),
            ("a.b", Some("5"), 'i', "int32"),
            ("$c", Some("7"), 'i', "int32"),
        ];
        assert_eq!(typed.columns.len(), want.len(), "{:?}", got.keys());
        // Columns in document order.
        assert_eq!(typed.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), want.map(|w| w.0));
        for (name, text, code, data_type) in want {
            assert_eq!(got[name], (text, code, data_type), "{url}: {name}");
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mongo_columns_are_stable_between_loads() {
    for_each_mongo(|p, pw, _url| async move {
        let d = mongo_driver(&p, pw).await;
        for name in ["users", "typed", "keys", "big", "active_users", "metrics"] {
            let first = page(d.as_ref(), name, None).await.columns;
            let second = page(d.as_ref(), name, None).await.columns;
            assert_eq!(first, second, "{name}");
            assert_eq!(d.table_structure(&table(name)).await.unwrap().columns, first, "{name}");
        }
        d.close().await;
    })
    .await;
}

/// Pages reuse the connection's sample: a field added after it shows on the page as a read-only
/// page field until the Structure view samples again; a new connection starts afresh.
#[tokio::test]
async fn mongo_pages_reuse_the_sample_until_the_structure_view() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw.clone()).await;
        assert!(page(d.as_ref(), "users", None).await.columns.iter().all(|c| c.name != "zz"), "{url}");
        let client = mongo_client(&url).await;
        let users = client.database("fx").collection::<Document>("users");
        users.insert_one(doc! { "name": "Zed", "email": "zed@example.com", "zz": 1 }).await.unwrap();

        let zz_read_only = |p: &TablePage| column(p, "zz").read_only;
        assert!(zz_read_only(&page(d.as_ref(), "users", None).await), "{url}: the page sampled again");
        let structure = d.table_structure(&table("users")).await.unwrap();
        let zz = structure.columns.iter().find(|c| c.name == "zz").unwrap_or_else(|| panic!("{url}: no zz"));
        assert!(!zz.read_only, "{url}");
        assert!(!zz_read_only(&page(d.as_ref(), "users", None).await), "{url}: the Structure view's sample is reused");
        d.close().await;

        let fresh = mongo_driver(&p, pw).await;
        assert!(!zz_read_only(&page(fresh.as_ref(), "users", None).await), "{url}");
        fresh.close().await;
        // Leaves the fixture as loaded (manual check step 90 loads it with this test binary).
        users.delete_one(doc! { "name": "Zed" }).await.unwrap();
        client.shutdown().immediate(true).await;
    })
    .await;
}

#[tokio::test]
async fn mongo_paging_and_sort() {
    for_each_mongo(|p, pw, _url| async move {
        let d = mongo_driver(&p, pw).await;
        let ids = |page: &TablePage| values(page, "_id").into_iter().map(|v| v.unwrap().parse::<i32>().unwrap()).collect::<Vec<_>>();
        // The default order is `_id` ascending.
        let first = d.fetch_page(&page_req("big", None, None, 0, 50)).await.unwrap();
        assert_eq!(ids(&first), (0..50).collect::<Vec<_>>());
        assert_eq!(first.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["_id", "n", "tag"]);
        // Page 3 of 50 by `n` descending: the 200 documents with n = 99 come first, by `_id`.
        let third = d.fetch_page(&page_req("big", None, Some(("n", true)), 2, 50)).await.unwrap();
        assert_eq!(ids(&third), (100..150).map(|k| 99 + 100 * k).collect::<Vec<_>>());
        assert!(values(&third, "n").iter().all(|n| n.as_deref() == Some("99")));
        // Page 5 crosses into n = 98.
        let fifth = d.fetch_page(&page_req("big", None, Some(("n", true)), 4, 50)).await.unwrap();
        assert_eq!(ids(&fifth), (0..50).map(|k| 98 + 100 * k).collect::<Vec<_>>());
        // Ascending, filtered.
        let asc = d.fetch_page(&page_req("big", Some("n = 7"), Some(("_id", true)), 1, 10)).await.unwrap();
        assert_eq!(ids(&asc), (180..190).rev().map(|k| 7 + 100 * k).collect::<Vec<_>>());
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mongo_counts() {
    for_each_mongo(|p, pw, _url| async move {
        let d = mongo_driver(&p, pw).await;
        let big = page(d.as_ref(), "big", None).await;
        // Below MySQL's exact-count limit (50,000), so counted exactly.
        assert_eq!(big.total, RowCount::Exact(BIG_ROWS as u64));
        assert_eq!(big.matches, None);
        let seven = page(d.as_ref(), "big", Some("n = 7")).await;
        assert_eq!(seven.total, RowCount::Exact(BIG_ROWS as u64));
        assert_eq!(seven.matches, Some(RowCount::Exact(200)));
        let view = page(d.as_ref(), "active_users", None).await;
        assert_eq!(view.total, RowCount::Exact(4));
        assert_eq!(view.rows.len(), 4);
        let ts = page(d.as_ref(), "metrics", None).await;
        assert_eq!(ts.total, RowCount::Exact(3));
        assert_eq!(page(d.as_ref(), "active_users", Some("name = 'Ann'")).await.matches, Some(RowCount::Exact(1)));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mongo_filters_respect_types() {
    for_each_mongo(|p, pw, _url| async move {
        let d = mongo_driver(&p, pw).await;
        let names = |page: TablePage| values(&page, "name").into_iter().map(Option::unwrap).collect::<Vec<_>>();
        assert_eq!(names(page(d.as_ref(), "users", Some("age = 30")).await), ["Ann", "Bob", "Cid"]);
        assert_eq!(names(page(d.as_ref(), "users", Some("age = '30'")).await), ["Dee"]);
        assert_eq!(names(page(d.as_ref(), "users", Some("`address.city` like '%osl%'")).await), ["Ann", "Eve"]);
        assert_eq!(names(page(d.as_ref(), "users", Some("age is null")).await), ["Fay"]);
        let unknown = page(d.as_ref(), "users", Some("nope = 1")).await;
        assert!(unknown.rows.is_empty());
        assert_eq!(unknown.matches, Some(RowCount::Exact(0)));

        let keys = page(d.as_ref(), "keys", Some("_id = '65a1b2c3d4e5f60718293a4b'")).await;
        let mut found: Vec<(String, String)> = keys
            .rows
            .iter()
            .zip(keys.cell_types.as_ref().unwrap())
            .map(|(r, t)| (r[0].clone().unwrap(), t[..1].to_string()))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [("65a1b2c3d4e5f60718293a4b".to_string(), "o".to_string()), ("65a1b2c3d4e5f60718293a4b".to_string(), "t".to_string())]
        );
        assert_eq!(page(d.as_ref(), "keys", Some("_id = 9007199254740993")).await.rows.len(), 1);

        match d.fetch_page(&page_req("users", Some("age = "), None, 0, 10)).await {
            Err(AppError::Filter { .. }) => {}
            other => panic!("expected a filter error, got {other:?}"),
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn mongo_structure() {
    for_each_mongo(|p, pw, _url| async move {
        let d = mongo_driver(&p, pw).await;
        let users = d.table_structure(&table("users")).await.unwrap();
        assert_eq!(users.relation, RelationKind::Table);
        assert_eq!(users.editing, Editing::Allowed);
        assert_eq!(users.primary_key, ["_id"]);
        assert_eq!(
            users.indexes,
            [
                IndexInfo { name: "_id_".into(), columns: vec!["_id".into()], included: Vec::new(), unique: true, primary: true },
                IndexInfo { name: "email_1".into(), columns: vec!["email".into()], included: Vec::new(), unique: true, primary: false },
            ]
        );
        assert!(users.foreign_keys.is_empty());
        for name in ["active_users", "metrics"] {
            let s = d.table_structure(&table(name)).await.unwrap();
            assert_eq!(s.editing, Editing::View, "{name}");
            assert_eq!(s.relation, RelationKind::View, "{name}");
        }
        assert!(d.table_structure(&table("active_users")).await.unwrap().indexes.is_empty());

        let typed = d.table_structure(&table("typed")).await.unwrap();
        let read_only: Vec<&str> = typed.columns.iter().filter(|c| c.read_only).map(|c| c.name.as_str()).collect();
        assert_eq!(read_only, ["_id", "a.b", "$c"]);

        match d.table_structure(&table("nope")).await {
            Err(AppError::NotFound(m)) => assert_eq!(m, "fx.nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
        d.close().await;
    })
    .await;
}
