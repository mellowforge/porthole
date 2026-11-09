//! Helpers for the SQLite integration test binaries.

use std::path::{Path, PathBuf};

use porthole_lib::model::*;

/// Builds `fx.sqlite` from `sqlite_fixture.sql` in a fresh temporary directory. The directory (and
/// the file) go away when the `TempDir` is dropped.
pub fn sqlite_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("fx.sqlite");
    let conn = rusqlite::Connection::open(&path).expect("create the fixture file");
    conn.execute_batch(include_str!("../fixtures/sqlite_fixture.sql")).expect("fixture sql");
    conn.close().map_err(|(_, e)| e).expect("close the fixture file");
    (dir, path)
}

pub fn sqlite_profile(path: &Path, read_only: bool) -> ConnectionProfile {
    ConnectionProfile {
        id: "s".into(),
        name: "s".into(),
        kind: DbKind::Sqlite,
        environment: Environment::Local,
        default_schema: None,
        connect_timeout_secs: 5,
        read_only,
        startup_sql: None,
        statement_timeout_secs: None,
        transport: Transport::File { file_path: path.to_string_lossy().into_owned() },
    }
}
