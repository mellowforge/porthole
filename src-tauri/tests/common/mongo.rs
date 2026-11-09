//! Helpers for the MongoDB integration test binaries.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use porthole_lib::driver::mongodb::connect_mongodb;
use porthole_lib::driver::Driver;
use porthole_lib::model::*;
use mongodb::bson::{doc, Bson, Document};
use mongodb::options::{ClientOptions, IndexOptions, TimeseriesOptions};
use mongodb::{Client, IndexModel};

const DEFAULT_MONGO_URLS: &str = "mongodb://root:test@localhost:27060/?authSource=admin,\
mongodb://root:test@localhost:27080/?authSource=admin,mongodb://localhost:27082/";
/// How long the first connection to a server is retried: the containers take a while to start.
const STARTUP_WAIT: Duration = Duration::from_secs(60);
/// Documents in `fx.big`.
pub const BIG_ROWS: i32 = 20_000;

pub fn mongo_urls() -> Vec<String> {
    let urls = std::env::var("TEST_MONGO_URLS").unwrap_or_else(|_| DEFAULT_MONGO_URLS.to_string());
    urls.split(',').map(str::to_string).collect()
}

/// A MongoDB profile for the server of `url` (its user and `authSource`), on `database`,
/// without TLS.
pub fn mongo_profile(url: &str, database: &str) -> ConnectionProfile {
    let u = url::Url::parse(url).unwrap();
    let auth_source = u.query_pairs().find(|(k, _)| k == "authSource").map(|(_, v)| v.into_owned());
    ConnectionProfile {
        id: "g".into(),
        name: "g".into(),
        kind: DbKind::Mongodb,
        default_schema: None,
        environment: Environment::Local,
        connect_timeout_secs: 5,
        read_only: false,
        startup_sql: None,
        statement_timeout_secs: None,
        transport: Transport::Network(NetworkTarget {
            host: u.host_str().unwrap().into(),
            port: u.port().unwrap_or(27017),
            user: u.username().into(),
            database: database.into(),
            ssl_mode: SslMode::Disable,
            ssl_root_cert: None,
            auth_source,
        }),
    }
}

/// The URL's password; `None` for a server without auth (the replica set).
pub fn mongo_password(url: &str) -> Option<String> {
    url::Url::parse(url).unwrap().password().map(str::to_string)
}

/// Whether the server of `url` has auth (and so the read-only user `ro`): the replica set has
/// neither.
pub fn has_auth(url: &str) -> bool {
    !url::Url::parse(url).unwrap().username().is_empty()
}

/// A direct client for `url`, after the server answers a ping; retried for up to `STARTUP_WAIT`
/// while the server starts. Fixture setup and read-back only.
pub async fn mongo_client(url: &str) -> Client {
    let mut opts = ClientOptions::parse(url).await.expect("a MongoDB URL");
    opts.direct_connection = Some(true);
    opts.server_selection_timeout = Some(Duration::from_secs(2));
    let client = Client::with_options(opts).expect("client options");
    let started = Instant::now();
    loop {
        match client.database("admin").run_command(doc! { "ping": 1 }).await {
            Ok(_) => return client,
            Err(e) if started.elapsed() < STARTUP_WAIT => {
                eprintln!("waiting for {url}: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("connect for fixture to {url}: {e}"),
        }
    }
}

/// The server's `hello` and `buildInfo.version`, read on a fresh client: what the driver under
/// test should report.
pub async fn mongo_server_facts(url: &str) -> (Document, String) {
    let client = mongo_client(url).await;
    let admin = client.database("admin");
    let hello = admin.run_command(doc! { "hello": 1 }).await.expect("hello");
    let build = admin.run_command(doc! { "buildInfo": 1 }).await.expect("buildInfo");
    client.shutdown().immediate(true).await;
    (hello, build.get_str("version").expect("a version").to_string())
}

/// The fixture: `fx` and `fx_other` from `mongo_fixture.json` (canonical extended JSON, read
/// with `serde_json::from_str::<Bson>` so key order and types are kept), the unique index on
/// `users.email`, the view `active_users`, the time-series collection `metrics`, `big`, and on
/// a server with auth the read-only user `ro` / `ropw`.
pub async fn load_mongo_fixture(url: &str) {
    let client = mongo_client(url).await;
    let fixture: Bson = serde_json::from_str(include_str!("../fixtures/mongo_fixture.json")).expect("fixture JSON");
    let Bson::Document(fixture) = fixture else { panic!("the fixture is not a document") };
    for db_name in ["fx", "fx_other"] {
        client.database(db_name).drop().await.expect("drop the fixture database");
    }
    let fx = client.database("fx");
    // Created before the inserts, so `metrics`' documents go into a time-series collection.
    let ts = TimeseriesOptions::builder().time_field("t".to_string()).build();
    fx.create_collection("metrics").timeseries(ts).await.expect("create fx.metrics");

    for (db_name, collections) in &fixture {
        let Bson::Document(collections) = collections else { panic!("{db_name} is not a document") };
        let db = client.database(db_name);
        for (name, docs) in collections {
            let Bson::Array(docs) = docs else { panic!("{db_name}.{name} is not an array") };
            let docs: Vec<Document> = docs
                .iter()
                .map(|d| match d {
                    Bson::Document(d) => d.clone(),
                    other => panic!("{db_name}.{name}: not a document: {other:?}"),
                })
                .collect();
            db.collection::<Document>(name).insert_many(docs).await.unwrap_or_else(|e| panic!("insert {db_name}.{name}: {e}"));
        }
    }

    let unique = IndexOptions::builder().unique(true).build();
    let email = IndexModel::builder().keys(doc! { "email": 1 }).options(unique).build();
    fx.collection::<Document>("users").create_index(email).await.expect("index users.email");
    fx.create_collection("active_users")
        .view_on("users".to_string())
        .pipeline(vec![doc! { "$match": { "active": true } }])
        .await
        .expect("create fx.active_users");
    let big: Vec<Document> =
        (0..BIG_ROWS).map(|i| doc! { "_id": i, "n": i % 100, "tag": format!("t{}", i % 7) }).collect();
    fx.collection::<Document>("big").insert_many(big).await.expect("fill fx.big");

    if has_auth(url) {
        let admin = client.database("admin");
        let info = admin.run_command(doc! { "usersInfo": "ro" }).await.expect("usersInfo");
        if info.get_array("users").map(|u| u.is_empty()).unwrap_or(true) {
            admin
                .run_command(doc! { "createUser": "ro", "pwd": "ropw", "roles": [{ "role": "read", "db": "fx" }] })
                .await
                .expect("create user ro");
        }
    }
    client.shutdown().immediate(true).await;
}

/// Loads the fixture on every server in `TEST_MONGO_URLS`, then runs `f` with a profile on `fx`,
/// the password (none on the replica set) and the URL, one server after the other, holding the
/// shared test lock.
pub async fn for_each_mongo<F, Fut>(f: F)
where
    F: Fn(ConnectionProfile, Option<String>, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let _guard = super::LOCK.lock().await;
    for url in mongo_urls() {
        load_mongo_fixture(&url).await;
        f(mongo_profile(&url, "fx"), mongo_password(&url), url.clone()).await;
    }
}

pub async fn mongo_driver(p: &ConnectionProfile, pw: Option<String>) -> Arc<dyn Driver> {
    connect_mongodb(p, pw).await.unwrap()
}
