//! SQLite table export: the grid's rows, streamed on a connection of the export's own.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use porthole_lib::driver::sqlite::connect_sqlite;
use porthole_lib::driver::Driver;
use porthole_lib::export::run::{run_table_export, ExportOutcome};
use porthole_lib::export::ExportFormat;
use porthole_lib::model::*;
use common::sqlite::{sqlite_fixture, sqlite_profile};

fn main_table(name: &str) -> TableRef {
    TableRef { schema: "main".into(), name: name.into() }
}

fn tq(name: &str, filter: Option<&str>, sort: Option<(&str, bool)>) -> TableQuery {
    TableQuery {
        table: main_table(name),
        filter: filter.map(str::to_string),
        sort: sort.map(|(column, descending)| SortSpec { column: column.into(), descending }),
    }
}

fn no_part_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".part"))
}

fn csv_field(v: &Option<String>) -> String {
    match v {
        None => String::new(),
        Some(s) if s.is_empty() => "\"\"".into(),
        Some(s) if s.contains([',', '"', '\r', '\n']) || s.starts_with(' ') || s.ends_with(' ') => {
            format!("\"{}\"", s.replace('"', "\"\""))
        }
        Some(s) => s.clone(),
    }
}

/// A save of one name, which needs the file to itself at COMMIT: it fails as locked (after the
/// 5 s busy timeout) while any other connection still reads.
fn rename(id: &str, name: &str) -> ChangeSet {
    ChangeSet {
        table: main_table("users"),
        changes: vec![RowChange::Update {
            key: vec![id.into()],
            values: [("name".to_string(), Some(name.to_string()))].into(),
            original_types: [("id".to_string(), CellType::Integer), ("name".to_string(), CellType::Text)].into(),
        }],
    }
}

#[tokio::test]
async fn sqlite_table_export_matches_the_page() {
    let (dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let filter = r#"status = "active" and name like "%ann%""#;
    let q = tq("users", Some(filter), Some(("name", true)));
    let target = dir.path().join("users.csv");
    let cancel = AtomicBool::new(false);
    let out = run_table_export(&d, &q, ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();

    let page = d
        .fetch_page(&PageRequest { table: q.table.clone(), filter: q.filter.clone(), sort: q.sort.clone(), page: 0, page_size: 1000 })
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 15);
    let mut expected = page.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(",");
    expected.push_str("\r\n");
    for row in &page.rows {
        expected.push_str(&row.iter().map(csv_field).collect::<Vec<_>>().join(","));
        expected.push_str("\r\n");
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
    assert!(expected.contains("\\xdeadbeef"), "the blob reads as on the page");
    match out {
        ExportOutcome::Done { rows, file_name, .. } => assert_eq!((rows, file_name.as_str()), (15, "users.csv")),
        other => panic!("expected done, got {other:?}"),
    }
    assert!(no_part_files(dir.path()));

    // A filter the compiler refuses fails the export, and leaves no file.
    let bad = dir.path().join("bad.csv");
    let err = run_table_export(&d, &tq("users", Some("nope = 1"), None), ExportFormat::Csv, &bad, &cancel, &|_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, porthole_lib::error::AppError::Filter { .. }), "{err:?}");
    assert!(!bad.exists());
    assert!(no_part_files(dir.path()));
    d.close().await;
}

#[tokio::test]
async fn sqlite_json_export_types_follow_cells() {
    let (dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let target = dir.path().join("typed.json");
    let cancel = AtomicBool::new(false);
    run_table_export(&d, &tq("typed", None, None), ExportFormat::Json, &target, &cancel, &|_| {}).await.unwrap();
    let rows = [
        r#"{"id":1,"i":42,"r":1.0,"n":1.5,"t":"x","b":"\\xdead","anyv":5,"d":"2026-01-01"}"#,
        r#"{"id":2,"i":-7,"r":0.1,"n":10,"t":"","b":"\\x","anyv":"5","d":null}"#,
        r#"{"id":3,"i":null,"r":1e300,"n":null,"t":null,"b":null,"anyv":2.5,"d":null}"#,
        r#"{"id":4,"i":0,"r":"Inf","n":0,"t":"y","b":null,"anyv":null,"d":null}"#,
    ];
    assert_eq!(std::fs::read_to_string(&target).unwrap(), format!("[\n{}\n]\n", rows.join(",\n")));

    // `loose.k` holds 5 and '5': a number and a string.
    let loose = dir.path().join("loose.json");
    run_table_export(&d, &tq("loose", None, None), ExportFormat::Json, &loose, &cancel, &|_| {}).await.unwrap();
    let got = std::fs::read_to_string(&loose).unwrap();
    assert!(got.contains(r#"{"k":5,"v":"int five"}"#) && got.contains(r#"{"k":"5","v":"text five"}"#), "{got}");
    d.close().await;
}

#[tokio::test]
async fn sqlite_table_export_cancel_leaves_no_file() {
    let (dir, path) = sqlite_fixture();
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let target = dir.path().join("big.csv");
    std::fs::write(&target, "old").unwrap();
    let cancel = AtomicBool::new(false);
    let out = run_table_export(&d, &tq("big", None, None), ExportFormat::Csv, &target, &cancel, &|n| {
        if n >= 1000 {
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .await
    .unwrap();
    assert_eq!(out, ExportOutcome::Canceled);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    assert!(no_part_files(dir.path()));

    let page = d.fetch_page(&PageRequest { table: main_table("big"), filter: None, sort: None, page: 0, page_size: 10 }).await.unwrap();
    assert_eq!(page.rows.len(), 10);
    // Nothing reads the file any more: a save gets it to itself at once.
    let started = Instant::now();
    d.apply_changes(&rename("1", "After export")).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    d.close().await;
}

/// A cancel while the statement runs and no row has come yet: `run_table_export` drops the
/// export, which interrupts the statement and closes the export's connection.
#[tokio::test]
async fn sqlite_table_export_cancel_stops_a_running_statement() {
    let (dir, path) = sqlite_fixture();
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("create view slow as select count(*) as n from big a, big b, big c").unwrap();
    }
    let d = connect_sqlite(&sqlite_profile(&path, false)).await.unwrap();
    let target = dir.path().join("slow.csv");
    let cancel = AtomicBool::new(false);
    let q = tq("slow", None, None);
    let started = Instant::now();
    let (out, ()) = tokio::join!(
        run_table_export(&d, &q, ExportFormat::Csv, &target, &cancel, &|_| {}),
        async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.store(true, Ordering::SeqCst);
        }
    );
    assert_eq!(out.unwrap(), ExportOutcome::Canceled);
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert!(!target.exists());
    assert!(no_part_files(dir.path()));

    // The interrupted read has let go of the file: a save commits without waiting it out.
    let saved = Instant::now();
    d.apply_changes(&rename("1", "After cancel")).await.unwrap();
    assert!(saved.elapsed() < Duration::from_secs(2), "{:?}", saved.elapsed());
    d.close().await;
}
