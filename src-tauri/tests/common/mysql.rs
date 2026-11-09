//! Helpers for the MySQL / MariaDB integration test binaries.

use std::future::Future;
use std::time::{Duration, Instant};

use porthole_lib::driver::mysql::{connect_mysql, MysqlDriver};
use porthole_lib::model::*;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder};

const DEFAULT_MYSQL_URLS: &str =
    "mysql://root:test@localhost:33084/app_test,mysql://root:test@localhost:33114/app_test";
/// How long the first connection to a server is retried: the containers take a while to start.
const STARTUP_WAIT: Duration = Duration::from_secs(60);

/// A MySQL profile for the server of `url`, on `database`, without TLS.
pub fn mysql_profile(url: &str, database: &str) -> ConnectionProfile {
    let u = url::Url::parse(url).unwrap();
    ConnectionProfile {
        id: "m".into(),
        name: "m".into(),
        kind: DbKind::Mysql,
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
            database: database.into(),
            ssl_mode: SslMode::Disable,
            ssl_root_cert: None,
            auth_source: None,
        }),
    }
}

pub fn mysql_urls() -> Vec<String> {
    let urls = std::env::var("TEST_MYSQL_URLS").unwrap_or_else(|_| DEFAULT_MYSQL_URLS.to_string());
    urls.split(',').map(str::to_string).collect()
}

/// Loads `mysql_fixture.sql` on every server in `TEST_MYSQL_URLS`, then runs `f` with a profile
/// on `fx` and the password, one server after the other, holding the shared test lock.
pub async fn for_each_mysql<F, Fut>(f: F)
where
    F: Fn(ConnectionProfile, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let _guard = super::LOCK.lock().await;
    for url in mysql_urls() {
        mysql_exec_script(&url, include_str!("../fixtures/mysql_fixture.sql")).await;
        f(mysql_profile(&url, "fx"), super::password_from_url(&url)).await;
    }
}

pub async fn mysql_driver(p: &ConnectionProfile, pw: &str) -> MysqlDriver {
    connect_mysql(p, Some(pw.to_string())).await.unwrap()
}

/// A plain connection to `url` as given (no socket, no TLS). The first attempt is retried for
/// up to `STARTUP_WAIT` while the server starts. Fixture setup only.
pub async fn mysql_conn(url: &str) -> Conn {
    let opts = OptsBuilder::from_opts(Opts::from_url(url).expect("a MySQL URL")).prefer_socket(false);
    let started = Instant::now();
    loop {
        match Conn::new(opts.clone()).await {
            Ok(c) => return c,
            Err(e) if started.elapsed() < STARTUP_WAIT => {
                eprintln!("waiting for {url}: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("connect for fixture to {url}: {e}"),
        }
    }
}

/// Runs SQL (one or more statements) as the URL's user on a fresh connection. Fixture setup only.
pub async fn mysql_exec(url: &str, sql: &str) {
    let mut conn = mysql_conn(url).await;
    conn.query_drop(sql).await.expect("fixture sql");
    conn.disconnect().await.ok();
}

/// Runs a script the way the `mysql` client does: split with the app's MySQL splitter, which
/// honours `DELIMITER` lines, one statement at a time. The fixture uses `DELIMITER` around its
/// procedure body so that `mysql < mysql_fixture.sql` loads it too (docs/manual-check.md).
pub async fn mysql_exec_script(url: &str, sql: &str) {
    let chars: Vec<char> = sql.chars().collect();
    let mut conn = mysql_conn(url).await;
    for st in porthole_lib::sqltext::split_statements_in(porthole_lib::sqltext::SqlDialect::Mysql, sql) {
        let text: String = chars[st.start..st.end].iter().collect();
        if let Err(e) = conn.query_drop(&text).await {
            panic!("fixture sql failed: {e}\n{text}");
        }
    }
    conn.disconnect().await.ok();
}

/// The URL of a profile built by `mysql_profile`, with its password: for test connections that
/// read back what the app did.
pub fn mysql_profile_url(p: &ConnectionProfile, pw: &str) -> String {
    let n = p.network().unwrap();
    format!("mysql://{}:{pw}@{}:{}/{}", n.user, n.host, n.port, n.database)
}

/// The first value of the first row of `sql` as text, read on a fresh connection; `None` when
/// there is no row or the value is NULL.
pub async fn mysql_one(p: &ConnectionProfile, pw: &str, sql: &str) -> Option<String> {
    let mut conn = mysql_conn(&mysql_profile_url(p, pw)).await;
    let value: Option<Option<String>> = conn.query_first(sql).await.expect("read-back sql");
    conn.disconnect().await.ok();
    value.flatten()
}

/// Creates `fx.skus`, a table with a text key whose values read as numbers (`5`, `05`): a
/// numeric literal compared with it must compare as text. Created by the tests that use it,
/// after the fixture's load, so the fixture's own table list is unchanged.
pub async fn mysql_create_skus(p: &ConnectionProfile, pw: &str) {
    mysql_exec(
        &mysql_profile_url(p, pw),
        "CREATE TABLE fx.skus (k VARCHAR(10) PRIMARY KEY, v TEXT);
         INSERT INTO fx.skus VALUES ('5', 'five'), ('05', 'oh-five'), ('x', 'ex');",
    )
    .await;
}
