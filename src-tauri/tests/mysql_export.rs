//! MySQL / MariaDB table export: the grid's rows, streamed on a connection of the export's own,
//! on every server in `TEST_MYSQL_URLS`.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use porthole_lib::driver::Driver;
use porthole_lib::export::run::{run_table_export, ExportOutcome};
use porthole_lib::export::ExportFormat;
use porthole_lib::model::*;
use common::mysql::{for_each_mysql, mysql_conn, mysql_driver, mysql_profile_url};
use mysql_async::prelude::Queryable;

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

#[tokio::test]
async fn mysql_table_export_matches_the_page() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
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
    })
    .await;
}

#[tokio::test]
async fn mysql_json_export_types_follow_cells() {
    for_each_mysql(|p, pw| async move {
        let d = mysql_driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("typed.json");
        let cancel = AtomicBool::new(false);
        run_table_export(&d, &tq("typed", None, None), ExportFormat::Json, &target, &cancel, &|_| {}).await.unwrap();
        let got = std::fs::read_to_string(&target).unwrap();
        let row = |id: &str| {
            got.lines().find(|l| l.starts_with(&format!("{{\"id\":{id},"))).unwrap_or_else(|| panic!("row {id} in {got}"))
        };
        let first = row("1");
        for fragment in [
            r#""x":1e300"#,
            r#""f":1.23457"#,
            r#""dc":12.30"#,
            r#""u":18446744073709551615"#,
            r#""vb":"\\xdead""#,
            r#""bt":"00101""#,
            r#""j":"{\"a\": [1, 2]}""#,
        ] {
            assert!(first.contains(fragment), "{fragment} in {first}");
        }
        let nulls = r#"{"id":3,"x":null,"f":null,"dt":null,"tm":null,"dc":null,"vb":null,"bt":null,"j":null,"y":null,"e":null,"st":null,"u":null,"ti":null,"d":null}"#;
        assert_eq!(row("3").trim_end_matches(','), nulls);
        d.close().await;
    })
    .await;
}

/// The connection id of the statement reading `big_pairs`, other than the asking connection's
/// own; 0 when there is none.
const EXPORT_QUERY_SQL: &str = "SELECT ID FROM information_schema.PROCESSLIST \
     WHERE COMMAND = 'Query' AND ID <> CONNECTION_ID() AND INFO LIKE '%big_pairs%'";

/// Canceling a table export stops its statement on the server, and the rest of
/// the result isn't read in the background.
#[tokio::test]
async fn mysql_table_export_cancel_stops_the_server_query() {
    for_each_mysql(|p, pw| async move {
        let url = mysql_profile_url(&p, &pw);
        let mut watch = mysql_conn(&url).await;
        // 3.6 billion rows, streamed from the first: a view has no key, so no ORDER BY.
        watch
            .query_drop("CREATE VIEW fx.big_pairs AS SELECT a.id AS a_id, b.id AS b_id FROM fx.big a, fx.big b")
            .await
            .unwrap();
        let d = mysql_driver(&p, &pw).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pairs.csv");
        std::fs::write(&target, "old").unwrap();
        let cancel = AtomicBool::new(false);
        let export_id = AtomicU64::new(0);
        let q = tq("big_pairs", None, None);

        let progress = |n: u64| {
            if n >= 1000 && export_id.load(Ordering::SeqCst) != 0 {
                cancel.store(true, Ordering::SeqCst);
            }
        };
        let find_export = async {
            let started = Instant::now();
            loop {
                let id: Option<u64> = watch.query_first(EXPORT_QUERY_SQL).await.unwrap();
                if let Some(id) = id {
                    export_id.store(id, Ordering::SeqCst);
                    return;
                }
                assert!(started.elapsed() < Duration::from_secs(10), "the export's statement never showed");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let (out, ()) = tokio::join!(run_table_export(&d, &q, ExportFormat::Csv, &target, &cancel, &progress), find_export);
        let canceled_at = Instant::now();
        assert_eq!(out.unwrap(), ExportOutcome::Canceled);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(no_part_files(dir.path()));

        let id = export_id.load(Ordering::SeqCst);
        loop {
            let running: Option<u64> = watch
                .query_first(format!("SELECT count(*) FROM information_schema.PROCESSLIST WHERE ID = {id} AND COMMAND = 'Query'"))
                .await
                .unwrap();
            if running == Some(0) {
                break;
            }
            assert!(canceled_at.elapsed() < Duration::from_secs(1), "connection {id} still runs its query");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(canceled_at.elapsed() < Duration::from_secs(1), "{:?}", canceled_at.elapsed());

        let page = d.fetch_page(&PageRequest { table: fx("big"), filter: None, sort: None, page: 0, page_size: 10 }).await.unwrap();
        assert_eq!(page.rows.len(), 10);
        watch.disconnect().await.ok();
        d.close().await;
    })
    .await;
}
