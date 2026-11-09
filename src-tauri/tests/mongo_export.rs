//! MongoDB table export (CSV columns, JSON whole documents) on every server in `TEST_MONGO_URLS`.

mod common;

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use porthole_lib::driver::mongodb::values::parse_json;
use porthole_lib::error::{AppError, AppResult};
use porthole_lib::export::run::{run_rows_export, run_table_export, ExportOutcome};
use porthole_lib::export::{ExportColumn, ExportFormat, RowSink};
use porthole_lib::model::*;
use porthole_lib::query::ResultColumn;
use common::mongo::{for_each_mongo, mongo_client, mongo_driver};
use futures_util::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};

fn fx(name: &str) -> TableRef {
    TableRef { schema: "fx".into(), name: name.into() }
}

fn tq(name: &str, filter: Option<&str>, sort: Option<(&str, bool)>) -> TableQuery {
    TableQuery {
        table: fx(name),
        filter: filter.map(str::to_string),
        sort: sort.map(|(column, descending)| SortSpec { column: column.into(), descending }),
    }
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

fn no_part_files(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".part"))
}

#[tokio::test]
async fn mongo_csv_export_matches_the_grid() {
    for_each_mongo(|p, pw, _url| async move {
        let d = mongo_driver(&p, pw).await;
        let dir = tempfile::tempdir().unwrap();
        let q = tq("typed", None, Some(("_id", false)));
        let target = dir.path().join("typed.csv");
        let cancel = AtomicBool::new(false);
        let out = run_table_export(&*d, &q, ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();

        let page = d
            .fetch_page(&PageRequest { table: q.table.clone(), filter: None, sort: q.sort.clone(), page: 0, page_size: 1000 })
            .await
            .unwrap();
        assert!(!page.rows.is_empty());
        let mut expected = page.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(",");
        expected.push_str("\r\n");
        for row in &page.rows {
            expected.push_str(&row.iter().map(csv_field).collect::<Vec<_>>().join(","));
            expected.push_str("\r\n");
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
        match out {
            ExportOutcome::Done { rows, .. } => assert_eq!(rows as usize, page.rows.len()),
            other => panic!("expected done, got {other:?}"),
        }

        // A filter the compiler refuses fails the export and leaves no file.
        let bad = dir.path().join("bad.csv");
        let err = run_table_export(&*d, &tq("typed", Some("nope nope"), None), ExportFormat::Csv, &bad, &cancel, &|_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Filter { .. }), "{err:?}");
        assert!(!bad.exists());
        assert!(no_part_files(dir.path()));
        d.close().await;
    })
    .await;
}

/// A read-only profile refuses saves, but exports still run, in both formats.
#[tokio::test]
async fn mongo_read_only_profile_exports() {
    for_each_mongo(|mut p, pw, url| async move {
        p.read_only = true;
        let d = mongo_driver(&p, pw).await;
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        for (format, name) in [(ExportFormat::Csv, "users.csv"), (ExportFormat::Json, "users.json")] {
            let target = dir.path().join(name);
            match run_table_export(&*d, &tq("users", None, None), format, &target, &cancel, &|_| {}).await {
                Ok(ExportOutcome::Done { rows, .. }) => assert_eq!(rows, 6, "{url}: {name}"),
                other => panic!("{url}: {name}: expected done, got {other:?}"),
            }
            assert!(target.exists(), "{url}: {name}");
        }
        d.close().await;
    })
    .await;
}

fn bytes(d: &Document) -> Vec<u8> {
    d.to_vec().unwrap()
}

#[tokio::test]
async fn mongo_json_export_writes_whole_documents() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let client = mongo_client(&url).await;
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);

        let exported = |text: String| -> Vec<Document> {
            // Read as the app reads a JSON cell: `serde_json::from_str::<Bson>` would make every
            // negative int32 an int64.
            let Some(Bson::Array(items)) = parse_json(&text) else { panic!("not an array: {text}") };
            items
                .into_iter()
                .map(|b| match b {
                    Bson::Document(d) => d,
                    other => panic!("not a document: {other:?}"),
                })
                .collect()
        };

        // Every type, field and key order of `typed` survives.
        let target = dir.path().join("typed.json");
        run_table_export(&*d, &tq("typed", None, Some(("_id", false))), ExportFormat::Json, &target, &cancel, &|_| {})
            .await
            .unwrap();
        let got = exported(std::fs::read_to_string(&target).unwrap());
        let stored: Vec<Document> = client
            .database("fx")
            .collection::<Document>("typed")
            .find(doc! {})
            .sort(doc! { "_id": 1 })
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert!(!stored.is_empty());
        assert_eq!(got.len(), stored.len());
        for (g, s) in got.iter().zip(&stored) {
            assert_eq!(bytes(g), bytes(s), "{g:?} vs {s:?}");
        }

        // The filter and sort are respected.
        let filter = r#"name like "%n%""#;
        let target = dir.path().join("users.json");
        run_table_export(&*d, &tq("users", Some(filter), Some(("name", true))), ExportFormat::Json, &target, &cancel, &|_| {})
            .await
            .unwrap();
        let got = exported(std::fs::read_to_string(&target).unwrap());
        let page = d
            .fetch_page(&PageRequest {
                table: fx("users"),
                filter: Some(filter.into()),
                sort: Some(SortSpec { column: "name".into(), descending: true }),
                page: 0,
                page_size: 1000,
            })
            .await
            .unwrap();
        let name_col = page.columns.iter().position(|c| c.name == "name").unwrap();
        let names: Vec<_> = got.iter().map(|g| g.get_str("name").unwrap().to_string()).collect();
        let expected: Vec<_> = page.rows.iter().map(|r| r[name_col].clone().unwrap()).collect();
        assert!(!names.is_empty() && names.len() < 6, "{names:?}");
        assert_eq!(names, expected);
        client.shutdown().immediate(true).await;
        d.close().await;
    })
    .await;
}

#[test]
fn mongo_rows_export_embeds_nested_values() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("r.json");
    let cancel = AtomicBool::new(false);
    let cols = vec![
        ResultColumn { name: "v".into(), data_type: "object".into() },
        ResultColumn { name: "s".into(), data_type: "string".into() },
    ];
    let rows = vec![vec![Some(r#"{"a": [1, {"b": 2}]}"#.to_string()), Some("{\"a\":1}".to_string())]];
    let types = vec!["jt".to_string()];
    run_rows_export(&cols, &rows, Some(&types), ExportFormat::Json, &target, &cancel, &|_| {}).unwrap();
    let got: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(got[0]["v"]["a"][1]["b"], 2, "an object, not a string: {got}");
    assert_eq!(got[0]["s"], "{\"a\":1}", "text stays text");
}

/// Ends the stream at its tenth document, the way a canceled export drops it: the first batch
/// (101 documents) is in, and the server cursor has the rest.
struct StopAtTenth(u64);

impl RowSink for StopAtTenth {
    fn begin(&mut self, _columns: &[ExportColumn]) -> AppResult<()> {
        Ok(())
    }
    fn row(&mut self, _row: Vec<Option<String>>) -> AppResult<()> {
        self.0 += 1;
        if self.0 == 10 {
            return Err(AppError::Query("stop".into()));
        }
        Ok(())
    }
}

/// A slow sink: each document takes 5 ms, so the first batch (101 documents) outlasts a 200 ms
/// timeout, which then fires at the export's next await (the `getMore`).
struct SlowSink(u64);

impl RowSink for SlowSink {
    fn begin(&mut self, _columns: &[ExportColumn]) -> AppResult<()> {
        Ok(())
    }
    fn row(&mut self, _row: Vec<Option<String>>) -> AppResult<()> {
        self.0 += 1;
        std::thread::sleep(Duration::from_millis(5));
        Ok(())
    }
}

async fn export_ops(admin: &mongodb::Database) -> Vec<Document> {
    admin
        .aggregate(vec![
            doc! { "$currentOp": { "allUsers": true, "idleCursors": true } },
            doc! { "$match": { "$or": [
                { "command.comment": "ts-export" },
                { "cursor.originatingCommand.comment": "ts-export" },
            ] } },
        ])
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap()
}

/// A canceled export leaves nothing running on the server.
#[tokio::test]
async fn mongo_table_export_cancel_stops_the_server_query() {
    for_each_mongo(|p, pw, url| async move {
        let d = mongo_driver(&p, pw).await;
        let client = mongo_client(&url).await;
        let admin = client.database("admin");

        // The detector sees an open export cursor: one held by the test shows as idle.
        let held = client
            .database("fx")
            .collection::<Document>("big")
            .find(doc! {})
            .batch_size(10)
            .comment(Bson::String("ts-export".into()))
            .await
            .unwrap();
        assert_eq!(export_ops(&admin).await.len(), 1, "the control cursor shows in $currentOp");
        drop(held);
        let started = Instant::now();
        while !export_ops(&admin).await.is_empty() {
            assert!(started.elapsed() < Duration::from_secs(2), "the control cursor stays");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The export stops with most of `big` unread; its cursor doesn't linger.
        let mut sink = StopAtTenth(0);
        let err = d.export_table(&tq("big", None, Some(("_id", false))), &mut sink).await.unwrap_err();
        assert_eq!(err.message(), "stop");
        let stopped = Instant::now();
        loop {
            let ops = export_ops(&admin).await;
            if ops.is_empty() {
                break;
            }
            assert!(stopped.elapsed() < Duration::from_secs(1), "still on the server: {ops:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The export's future is dropped while it awaits the next batch (what `run_table_export`'s
        // `select!` does on a cancel); the server cursor must go too.
        let mut slow = SlowSink(0);
        let dropped = tokio::time::timeout(
            Duration::from_millis(200),
            d.export_table(&tq("big", None, Some(("_id", false))), &mut slow),
        )
        .await;
        assert!(dropped.is_err(), "the export should still be streaming: {dropped:?}");
        assert!((100..=101).contains(&slow.0), "the first batch was read, not the rest: {}", slow.0);
        let dropped_at = Instant::now();
        loop {
            let ops = export_ops(&admin).await;
            if ops.is_empty() {
                break;
            }
            assert!(dropped_at.elapsed() < Duration::from_secs(1), "still on the server after the drop: {ops:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // And through the whole flow: a canceled file export removes its part file.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("big.csv");
        let cancel = AtomicBool::new(true);
        let out = run_table_export(&*d, &tq("big", None, None), ExportFormat::Csv, &target, &cancel, &|_| {}).await.unwrap();
        assert_eq!(out, ExportOutcome::Canceled);
        assert!(!target.exists() && no_part_files(dir.path()));
        assert!(export_ops(&admin).await.is_empty());
        client.shutdown().immediate(true).await;
        d.close().await;
    })
    .await;
}
