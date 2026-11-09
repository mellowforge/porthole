//! Helpers shared by the integration test binaries.
#![allow(dead_code)]

pub mod mongo;
pub mod mysql;
pub mod sqlite;

use std::future::Future;

use porthole_lib::driver::postgres::{connect_postgres, PostgresDriver};
use porthole_lib::model::*;
use sqlx::{AssertSqlSafe, Connection, PgConnection};

const DEFAULT_URLS: &str = "postgres://postgres:test@localhost:54311/app_test,postgres://postgres:test@localhost:54317/app_test";

pub fn profile_from_url(url: &str) -> ConnectionProfile {
    let u = url::Url::parse(url).unwrap();
    ConnectionProfile {
        id: "t".into(),
        name: "t".into(),
        kind: DbKind::Postgres,
        default_schema: None,
        environment: Environment::Local,
        connect_timeout_secs: 5,
        read_only: false,
        startup_sql: None,
        statement_timeout_secs: None,
        transport: Transport::Network(NetworkTarget {
            host: u.host_str().unwrap().into(),
            port: u.port().unwrap(),
            user: u.username().into(),
            database: u.path().trim_start_matches('/').into(),
            ssl_mode: SslMode::Disable,
            ssl_root_cert: None,
            auth_source: None,
        }),
    }
}

/// The network target of a profile built by `profile_from_url`, for tests that tweak it.
pub fn net(p: &mut ConnectionProfile) -> &mut NetworkTarget {
    match &mut p.transport {
        Transport::Network(n) => n,
        _ => panic!("not a network profile"),
    }
}

pub fn password_from_url(url: &str) -> String {
    url::Url::parse(url).unwrap().password().unwrap().to_string()
}

/// The URL for a profile built by `profile_from_url`.
pub fn url(p: &ConnectionProfile, pw: &str) -> String {
    let n = p.network().unwrap();
    format!("postgres://{}:{pw}@{}:{}/{}", n.user, n.host, n.port, n.database)
}

/// Serializes tests: they all reload the same fixture into shared databases.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn for_each_pg<F, Fut>(f: F)
where
    F: Fn(ConnectionProfile, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let _guard = LOCK.lock().await;
    let urls = std::env::var("TEST_PG_URLS").unwrap_or_else(|_| DEFAULT_URLS.to_string());
    for url in urls.split(',') {
        exec(url, include_str!("../fixtures/pg_fixture.sql")).await;
        f(profile_from_url(url), password_from_url(url)).await;
    }
}

pub async fn driver(p: &ConnectionProfile, pw: &str) -> PostgresDriver {
    connect_postgres(p, Some(pw.to_string())).await.unwrap()
}

/// Runs SQL (one or more statements) on a fresh, plain connection. Fixture setup only.
pub async fn exec(url: &str, sql: &str) {
    let mut conn = PgConnection::connect(url).await.expect("connect for fixture");
    sqlx::raw_sql(AssertSqlSafe(sql.to_string())).execute(&mut conn).await.expect("fixture sql");
    conn.close().await.ok();
}

/// `url` with its database replaced by `db`.
pub fn url_for_database(url: &str, db: &str) -> String {
    let mut u = url::Url::parse(url).unwrap();
    u.set_path(&format!("/{db}"));
    u.to_string()
}

/// Creates database `db` on the server of `url` unless it exists (CREATE DATABASE must be
/// its own statement), then makes sure it has `other_t` with one row.
pub async fn ensure_database(url: &str, db: &str) {
    let mut conn = PgConnection::connect(url).await.expect("connect for fixture");
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(db)
        .fetch_optional(&mut conn)
        .await
        .expect("look up the database");
    if exists.is_none() {
        let quoted = format!("\"{}\"", db.replace('"', "\"\""));
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {quoted}"))).execute(&mut conn).await.expect("create database");
    }
    conn.close().await.ok();
    exec(
        &url_for_database(url, db),
        "CREATE TABLE IF NOT EXISTS other_t (id int PRIMARY KEY, label text);
         INSERT INTO other_t VALUES (1, 'one') ON CONFLICT (id) DO NOTHING;",
    )
    .await;
}
