mod common;

use std::time::{Duration, Instant};

use porthole_lib::driver::postgres::connect_postgres;
use porthole_lib::driver::{test_connection, Driver};
use porthole_lib::error::AppError;
use porthole_lib::model::*;
use porthole_lib::query::RawOutcome;
use common::{driver, for_each_pg};

/// Runs `sql` in a query session opened for the profile and returns its error message.
async fn session_write_error(p: &ConnectionProfile, pw: &str, sql: &str) -> String {
    let d = driver(p, pw).await;
    let session = d.open_query_session(None).await.unwrap();
    let run = session.run(sql, &[], 10).await;
    session.close().await;
    d.close().await;
    match run.outcome {
        RawOutcome::Error { message, .. } => message,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn lists_schemas_including_mixed_case() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = d.list_schemas(None).await.unwrap();
        assert!(s.contains(&"fx".to_string()));
        assert!(s.contains(&"Mixed Case".to_string()));
        assert!(!s.contains(&"pg_catalog".to_string()));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn schema_tree_kinds() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let t = d.schema_tree("fx").await.unwrap();
        for n in ["users", "orders", "no_pk", "big", "events"] {
            assert!(t.tables.contains(&n.to_string()), "missing {n}: {:?}", t.tables);
        }
        assert!(!t.tables.contains(&"events_2026".to_string()));
        assert_eq!(t.views, vec!["active_users", "mv_totals"]);
        assert!(t
            .functions
            .iter()
            .any(|f| f.name == "calc_tax" && f.signature == "numeric"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn structure_users() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = d
            .table_structure(&TableRef { schema: "fx".into(), name: "users".into() })
            .await
            .unwrap();
        assert_eq!(s.columns[0].name, "id");
        assert!(s.columns[0].is_primary_key);
        let email = s.columns.iter().find(|c| c.name == "email").unwrap();
        assert_eq!(email.data_type, "varchar");
        assert_eq!(email.full_type, "character varying(255)");
        let name = s.columns.iter().find(|c| c.name == "name").unwrap();
        assert!(!name.nullable);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn structure_orders_fk_and_index() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = d
            .table_structure(&TableRef { schema: "fx".into(), name: "orders".into() })
            .await
            .unwrap();
        assert_eq!(s.foreign_keys.len(), 1);
        let fk = &s.foreign_keys[0];
        assert_eq!(fk.columns, vec!["user_id"]);
        assert_eq!(fk.ref_table, TableRef { schema: "fx".into(), name: "users".into() });
        assert_eq!(fk.ref_columns, vec!["id"]);
        assert!(s.indexes.iter().any(|i| i.primary && i.unique));
        assert!(s.indexes.iter().any(|i| !i.primary && i.columns == vec!["user_id"]));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn read_only_blocks_writes() {
    for_each_pg(|mut p, pw| async move {
        p.read_only = true;
        let err = session_write_error(&p, &pw, "INSERT INTO fx.no_pk VALUES (9, 'x')").await;
        assert!(err.contains("read-only"), "{err}");
    })
    .await;
}

#[tokio::test]
async fn wrong_password_is_connection_error() {
    for_each_pg(|p, _pw| async move {
        let err = test_connection(&p, Some("s3cr3t-pw-xyz".into())).await.unwrap_err();
        match err {
            AppError::Connection(m) => {
                assert!(m.contains("password authentication failed"), "{m}");
                assert!(!m.contains("s3cr3t-pw-xyz"), "{m}");
                assert!(!m.starts_with("connection error"), "double prefix: {m}");
            }
            other => panic!("expected Connection, got {other:?}"),
        }
    })
    .await;
}

#[tokio::test]
async fn unreachable_port_times_out_quickly() {
    for_each_pg(|mut p, pw| async move {
        common::net(&mut p).port = 1;
        p.connect_timeout_secs = 2;
        let start = Instant::now();
        let err = match connect_postgres(&p, Some(pw)).await {
            Ok(_) => panic!("connect to port 1 succeeded"),
            Err(e) => e,
        };
        assert!(start.elapsed() < Duration::from_secs(5));
        match err {
            AppError::Connection(m) => assert!(!m.contains("pool timed out"), "{m}"),
            other => panic!("expected Connection, got {other:?}"),
        }
    })
    .await;
}

#[tokio::test]
async fn startup_sql_error_is_reported() {
    for_each_pg(|mut p, pw| async move {
        p.startup_sql = Some("SELEC 1".into());
        let err = match connect_postgres(&p, Some(pw)).await {
            Ok(_) => panic!("bad startup SQL connected"),
            Err(e) => e,
        };
        match err {
            AppError::Connection(m) => assert!(m.contains("syntax error"), "{m}"),
            other => panic!("expected Connection, got {other:?}"),
        }
    })
    .await;
}

#[tokio::test]
async fn read_only_survives_startup_sql() {
    for_each_pg(|mut p, pw| async move {
        p.read_only = true;
        p.startup_sql = Some("SET default_transaction_read_only = off".into());
        let err = session_write_error(&p, &pw, "INSERT INTO fx.no_pk VALUES (9, 'x')").await;
        assert!(err.contains("read-only"), "{err}");
    })
    .await;
}

#[tokio::test]
async fn test_connection_succeeds() {
    for_each_pg(|p, pw| async move {
        let r = test_connection(&p, Some(pw)).await.unwrap();
        assert!(r.server_version.contains("PostgreSQL"), "{}", r.server_version);
        assert!(r.latency_ms < 5_000, "{}", r.latency_ms);
    })
    .await;
}

fn page_req(schema: &str, name: &str, filter: Option<&str>, sort: Option<(&str, bool)>, page: u32, size: u32) -> PageRequest {
    PageRequest {
        table: TableRef { schema: schema.into(), name: name.into() },
        filter: filter.map(String::from),
        sort: sort.map(|(c, d)| SortSpec { column: c.into(), descending: d }),
        page,
        page_size: size,
    }
}

fn col_index(p: &TablePage, name: &str) -> usize {
    p.columns.iter().position(|c| c.name == name).unwrap()
}

#[tokio::test]
async fn fetch_page_of_users_as_text() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("fx", "users", None, None, 0, 100)).await.unwrap();
        assert_eq!(page.rows.len(), 17);
        assert_eq!(page.total, RowCount::Exact(17));
        assert_eq!(page.matches, None);
        let (plan, bio, avatar, data, tags) = (
            col_index(&page, "plan"),
            col_index(&page, "bio"),
            col_index(&page, "avatar"),
            col_index(&page, "data"),
            col_index(&page, "tags"),
        );
        assert_eq!(page.rows[0][plan], None);
        assert_eq!(page.rows[1][plan], Some(String::new()));
        assert_eq!(page.rows[2][bio].as_ref().unwrap().len(), 10_000);
        assert_eq!(page.rows[3][avatar].as_deref(), Some("\\xdeadbeef"));
        assert_eq!(page.rows[3][data].as_deref(), Some("{\"a\": 1}"));
        assert_eq!(page.rows[3][tags].as_deref(), Some("{x,y}"));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_filter_spec_example_matches_15() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let f = r#"status = "active" and name like "%ann%""#;
        let page = d.fetch_page(&page_req("fx", "users", Some(f), None, 0, 100)).await.unwrap();
        assert_eq!(page.matches, Some(RowCount::Exact(15)));
        assert_eq!(page.rows.len(), 15);
        let blank = d.fetch_page(&page_req("fx", "users", Some("  "), None, 0, 100)).await.unwrap();
        assert_eq!(blank.matches, None);
        assert_eq!(blank.rows.len(), 17);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_filter_injection_is_harmless() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let f = r#"name = "x'; drop table fx.users; --""#;
        let page = d.fetch_page(&page_req("fx", "users", Some(f), None, 0, 100)).await.unwrap();
        assert_eq!(page.rows.len(), 0);
        let again = d.fetch_page(&page_req("fx", "users", None, None, 0, 100)).await.unwrap();
        assert_eq!(again.rows.len(), 17);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_filter_unknown_column_is_filter_error() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let err = d.fetch_page(&page_req("fx", "users", Some("nope = 1"), None, 0, 100)).await.unwrap_err();
        assert!(matches!(err, AppError::Filter { position: 0, .. }), "{err:?}");
        let err = d
            .fetch_page(&page_req("fx", "users", None, Some(("nope", false)), 0, 100))
            .await
            .unwrap_err();
        assert!(matches!(&err, AppError::Query(m) if m.contains("Unknown column")), "{err:?}");
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_big_table_uses_estimate_and_pages() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("fx", "big", None, None, 2, 100)).await.unwrap();
        assert_eq!(page.rows.len(), 100);
        assert_eq!(page.rows[0][col_index(&page, "id")].as_deref(), Some("201"));
        match page.total {
            RowCount::Estimated(n) => assert!((54_000..=66_000).contains(&n), "{n}"),
            other => panic!("expected estimate, got {other:?}"),
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_sort_desc_nulls_last() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let page = d
            .fetch_page(&page_req("fx", "users", None, Some(("plan", true)), 0, 100))
            .await
            .unwrap();
        let (plan, id) = (col_index(&page, "plan"), col_index(&page, "id"));
        assert_eq!(page.rows.len(), 17);
        assert_eq!(page.rows.last().unwrap()[plan], None);
        let values: Vec<&str> = page.rows[..16].iter().map(|r| r[plan].as_deref().unwrap()).collect();
        let mut sorted = values.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(values, sorted);
        assert_eq!(values[0], "pro");
        assert_eq!(values[15], "");
        // Ties are broken by the primary key, ascending.
        let pro_ids: Vec<i64> = page.rows[..7].iter().map(|r| r[id].as_deref().unwrap().parse().unwrap()).collect();
        assert_eq!(pro_ids, vec![3, 4, 8, 10, 12, 14, 17]);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_sort_int_column_is_numeric() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("fx", "big", None, Some(("id", true)), 0, 100)).await.unwrap();
        let id = col_index(&page, "id");
        assert_eq!(page.rows[0][id].as_deref(), Some("60000"));
        assert_eq!(page.rows[99][id].as_deref(), Some("59901"));
        d.close().await;
    })
    .await;
}

fn column(p: &TablePage, name: &str) -> Vec<Option<String>> {
    let i = col_index(p, name);
    p.rows.iter().map(|r| r[i].clone()).collect()
}

fn some(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|s| Some(s.to_string())).collect()
}

#[tokio::test]
async fn fetch_mixed_case_identifiers_work() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let all = d.fetch_page(&page_req("Mixed Case", "Order Items", None, None, 0, 100)).await.unwrap();
        assert_eq!(all.total, RowCount::Exact(2));
        assert_eq!(column(&all, "id"), some(&["1", "2"]));
        assert_eq!(column(&all, "select"), some(&["b", "a"]));
        assert_eq!(column(&all, "Weird \"Quote\""), vec![None, Some("7".to_string())]);

        let filtered = d
            .fetch_page(&page_req("Mixed Case", "Order Items", Some("`Weird \"Quote\"` is null"), None, 0, 100))
            .await
            .unwrap();
        assert_eq!(filtered.matches, Some(RowCount::Exact(1)));
        assert_eq!(column(&filtered, "id"), some(&["1"]));

        let sorted = d
            .fetch_page(&page_req("Mixed Case", "Order Items", None, Some(("select", false)), 0, 100))
            .await
            .unwrap();
        assert_eq!(column(&sorted, "select"), some(&["a", "b"]));

        let by_number = d
            .fetch_page(&page_req("Mixed Case", "Order Items", Some("`Weird \"Quote\"` = 7"), None, 0, 100))
            .await
            .unwrap();
        assert_eq!(column(&by_number, "select"), some(&["a"]));

        let tree = d.schema_tree("Mixed Case").await.unwrap();
        assert!(tree.tables.contains(&"Order Items".to_string()), "{:?}", tree.tables);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_keyword_named_table_and_column() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let all = d.fetch_page(&page_req("fx", "select", None, None, 0, 100)).await.unwrap();
        assert_eq!(all.total, RowCount::Exact(2));
        assert_eq!(column(&all, "from"), some(&["x", "y"]));

        let filtered = d.fetch_page(&page_req("fx", "select", Some("from = \"y\""), None, 0, 100)).await.unwrap();
        assert_eq!(filtered.matches, Some(RowCount::Exact(1)));
        assert_eq!(column(&filtered, "id"), some(&["2"]));
        let quoted = d.fetch_page(&page_req("fx", "select", Some("`from` like \"x%\""), None, 0, 100)).await.unwrap();
        assert_eq!(column(&quoted, "id"), some(&["1"]));

        let sorted = d.fetch_page(&page_req("fx", "select", None, Some(("from", true)), 0, 100)).await.unwrap();
        assert_eq!(column(&sorted, "from"), some(&["y", "x"]));

        let tree = d.schema_tree("fx").await.unwrap();
        assert!(tree.tables.contains(&"select".to_string()));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_no_pk_has_no_default_order_and_works() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("fx", "no_pk", None, None, 0, 100)).await.unwrap();
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.total, RowCount::Exact(2));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_counts_views_matviews_and_partitioned_exactly() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        for (name, n) in [("events", 0), ("active_users", 16), ("mv_totals", 2)] {
            let page = d.fetch_page(&page_req("fx", name, None, None, 0, 100)).await.unwrap();
            assert_eq!(page.total, RowCount::Exact(n), "{name}");
            assert_eq!(page.rows.len() as u64, n, "{name}");
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_unanalyzed_table_counts_exactly() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        common::exec(&common::url(&p, &pw), "CREATE TABLE fx.fresh (id int PRIMARY KEY)").await;
        common::exec(&common::url(&p, &pw), "INSERT INTO fx.fresh SELECT generate_series(1, 250)").await;
        let page = d.fetch_page(&page_req("fx", "fresh", None, None, 0, 100)).await.unwrap();
        assert_eq!(page.total, RowCount::Exact(250));
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn filtered_count_timeout_is_unknown() {
    for_each_pg(|p, pw| async move {
        let mut d = driver(&p, &pw).await;
        d.set_count_timeout_ms(1);
        let page = d
            .fetch_page(&page_req("fx", "big", Some(r#"v like "%x%""#), None, 0, 100))
            .await
            .unwrap();
        assert_eq!(page.matches, Some(RowCount::Unknown));
        assert!(page.rows.is_empty());
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_filter_bad_cast() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let err = d
            .fetch_page(&page_req("fx", "users", Some(r#"id = "abc""#), None, 0, 100))
            .await
            .unwrap_err();
        match err {
            AppError::Filter { message, position } => {
                assert!(message.contains("abc"), "{message}");
                assert_eq!(position, 0);
            }
            other => panic!("expected Filter, got {other:?}"),
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_filter_typed_columns() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let cases = [
            ("users", "id > 5", 12),
            ("users", "id in (1,2,3)", 3),
            ("users", r#"created_at > "2000-01-01""#, 17),
            ("users", r#"data = '{"a": 1}'"#, 1),
            ("users", r#"tags = "{x,y}""#, 1),
            ("orders", "total > 10", 2),
            ("orders", "total = 5.25", 1),
            ("orders", "total <= 10.5", 2),
        ];
        for (table, filter, n) in cases {
            let page = d.fetch_page(&page_req("fx", table, Some(filter), None, 0, 100)).await.unwrap();
            assert_eq!(page.matches, Some(RowCount::Exact(n)), "{filter}");
            assert_eq!(page.rows.len() as u64, n, "{filter}");
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn page_size_is_clamped() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let tiny = d.fetch_page(&page_req("fx", "big", None, None, 0, 0)).await.unwrap();
        assert_eq!(tiny.rows.len(), 1);
        let huge = d.fetch_page(&page_req("fx", "big", None, None, 0, 5000)).await.unwrap();
        assert!(huge.rows.len() <= 1000, "{}", huge.rows.len());
        assert_eq!(huge.rows.len(), 1000);
        d.close().await;
    })
    .await;
}

fn fx(name: &str) -> TableRef {
    TableRef { schema: "fx".into(), name: name.into() }
}

#[tokio::test]
async fn structure_primary_key_in_index_order() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let s = d.table_structure(&fx("pair")).await.unwrap();
        assert_eq!(s.primary_key, vec!["select", "Key A"]);
        assert_eq!(d.table_structure(&fx("no_pk")).await.unwrap().primary_key, Vec::<String>::new());
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn structure_editing_kinds() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        for (t, editing) in [
            ("users", Editing::Allowed),
            ("active_users", Editing::View),
            ("no_pk", Editing::NoPrimaryKey),
            ("floaty", Editing::FloatKey),
            ("mv_totals", Editing::View),
        ] {
            assert_eq!(d.table_structure(&fx(t)).await.unwrap().editing, editing, "{t}");
        }
        assert_eq!(d.table_structure(&fx("events")).await.unwrap().relation, RelationKind::Partitioned);
        assert_eq!(d.table_structure(&fx("users")).await.unwrap().relation, RelationKind::Table);
        assert_eq!(d.table_structure(&fx("active_users")).await.unwrap().relation, RelationKind::View);
        assert_eq!(d.table_structure(&fx("mv_totals")).await.unwrap().relation, RelationKind::Materialized);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn structure_cast_type_and_read_only() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let version = d.server_version().await.unwrap();
        let typed = d.table_structure(&fx("typed")).await.unwrap();
        let get = |s: &TableStructure, n: &str| s.columns.iter().find(|c| c.name == n).unwrap().clone();
        assert_eq!(get(&typed, "v").cast_type, "character varying");
        assert_eq!(get(&typed, "n").cast_type, "numeric");
        assert_eq!(get(&typed, "c").cast_type, "bpchar");
        assert_eq!(get(&typed, "bits").cast_type, "varbit");
        assert_eq!(get(&typed, "arr").cast_type, "integer[]");
        assert_eq!(get(&typed, "cc").cast_type, "bpchar[]");
        let genr = d.table_structure(&fx("gen")).await.unwrap();
        assert!(get(&genr, "id").read_only);
        assert!(!get(&genr, "name").read_only);
        if !version.starts_with("PostgreSQL 11") {
            assert!(get(&genr, "upper_name").read_only);
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn fetch_page_reports_key_and_editing() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let page = d.fetch_page(&page_req("fx", "users", None, None, 0, 100)).await.unwrap();
        assert_eq!(page.primary_key, vec!["id"]);
        assert_eq!(page.editing, Editing::Allowed);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn filter_cast_uses_cast_type() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        for (filter, n) in [("n = 1.499", 0), ("n = 1.5", 1), (r#"c = "abc""#, 1), (r#"v = "abc""#, 1)] {
            let page = d.fetch_page(&page_req("fx", "typed", Some(filter), None, 0, 100)).await.unwrap();
            assert_eq!(page.matches, Some(RowCount::Exact(n)), "{filter}");
        }
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn structure_shows_expression_and_included_index_columns() {
    for_each_pg(|p, pw| async move {
        let url = common::url(&p, &pw);
        common::exec(
            &url,
            "DROP SCHEMA IF EXISTS ts_scratch CASCADE;
             CREATE SCHEMA ts_scratch;
             CREATE TABLE ts_scratch.t (id int PRIMARY KEY, email text, total int, status text);
             CREATE INDEX t_lower_email ON ts_scratch.t (lower(email), id);
             CREATE INDEX t_id_incl ON ts_scratch.t (id) INCLUDE (total, status);",
        )
        .await;
        let d = driver(&p, &pw).await;
        let s = d.table_structure(&TableRef { schema: "ts_scratch".into(), name: "t".into() }).await;
        d.close().await;
        common::exec(&url, "DROP SCHEMA IF EXISTS ts_scratch CASCADE").await;
        let s = s.unwrap();
        let idx = |n: &str| s.indexes.iter().find(|i| i.name == n).unwrap().clone();
        let expr = idx("t_lower_email");
        assert_eq!(expr.columns, vec!["lower(email)", "id"]);
        assert!(expr.included.is_empty());
        let incl = idx("t_id_incl");
        assert_eq!(incl.columns, vec!["id"]);
        assert_eq!(incl.included, vec!["total", "status"]);
    })
    .await;
}

type Pages = (Vec<Vec<Option<String>>>, Vec<bool>);

/// Pages 0–2 (100 rows each) of `ts_scratch.{name}`: their rows in page order, and each page's
/// `unstable_order`.
async fn three_pages(d: &dyn Driver, name: &str, sort: Option<(&str, bool)>) -> Pages {
    let mut rows = Vec::new();
    let mut unstable = Vec::new();
    for page in 0..3 {
        let p = d.fetch_page(&page_req("ts_scratch", name, None, sort, page, 100)).await.unwrap();
        // Still read-only.
        assert!(matches!(p.editing, Editing::NoPrimaryKey | Editing::View), "{name}: {:?}", p.editing);
        rows.extend(p.rows);
        unstable.push(p.unstable_order);
    }
    (rows, unstable)
}

/// Column `i` of every row, as integers.
fn ints(rows: &[Vec<Option<String>>], i: usize) -> Vec<i32> {
    rows.iter().map(|r| r[i].as_deref().unwrap().parse().unwrap()).collect()
}

#[tokio::test]
async fn page_order_without_a_primary_key() {
    for_each_pg(|p, pw| async move {
        let url = common::url(&p, &pw);
        common::exec(
            &url,
            "DROP SCHEMA IF EXISTS ts_scratch CASCADE;
             CREATE SCHEMA ts_scratch;
             CREATE TABLE ts_scratch.nokey (a int, b text);
             INSERT INTO ts_scratch.nokey SELECT g, (g % 2)::text FROM generate_series(1, 250) g;
             CREATE TABLE ts_scratch.uniq (a int NOT NULL, b text, UNIQUE (a));
             INSERT INTO ts_scratch.uniq SELECT g, (g % 2)::text FROM generate_series(250, 1, -1) g;
             CREATE TABLE ts_scratch.uniq_null (a int UNIQUE);
             INSERT INTO ts_scratch.uniq_null SELECT g FROM generate_series(250, 1, -1) g;
             CREATE TABLE ts_scratch.skipped (a int NOT NULL);
             CREATE UNIQUE INDEX skipped_partial ON ts_scratch.skipped (a) WHERE a > 0;
             CREATE UNIQUE INDEX skipped_expr ON ts_scratch.skipped ((-a));
             INSERT INTO ts_scratch.skipped SELECT g FROM generate_series(250, 1, -1) g;
             CREATE TABLE ts_scratch.two (a int NOT NULL, b int NOT NULL, UNIQUE (a, b), UNIQUE (b));
             INSERT INTO ts_scratch.two SELECT g, 251 - g FROM generate_series(1, 250) g;
             CREATE VIEW ts_scratch.v AS SELECT * FROM ts_scratch.nokey;",
        )
        .await;
        let d = driver(&p, &pw).await;
        let nokey = three_pages(&d, "nokey", None).await;
        let nokey_by_b = three_pages(&d, "nokey", Some(("b", false))).await;
        let uniq = three_pages(&d, "uniq", None).await;
        let uniq_by_b = three_pages(&d, "uniq", Some(("b", true))).await;
        let uniq_null = three_pages(&d, "uniq_null", None).await;
        let skipped = three_pages(&d, "skipped", None).await;
        let two = three_pages(&d, "two", None).await;
        let view = three_pages(&d, "v", None).await;
        d.close().await;
        common::exec(&url, "DROP SCHEMA IF EXISTS ts_scratch CASCADE").await;

        let stable = vec![false; 3];
        let up: Vec<i32> = (1..=250).collect();
        let down: Vec<i32> = (1..=250).rev().collect();
        let evens_then_odds: Vec<i32> = (1..=250).filter(|a| a % 2 == 0).chain((1..=250).filter(|a| a % 2 == 1)).collect();
        let odds_then_evens: Vec<i32> = (1..=250).filter(|a| a % 2 == 1).chain((1..=250).filter(|a| a % 2 == 0)).collect();

        // No key and no unique index: by ctid, here the order the rows went in, each row once.
        assert_eq!(ints(&nokey.0, 0), up);
        assert_eq!(nokey.1, stable);
        // Rows equal on the sort keep that order, so no row repeats or goes missing across pages.
        assert_eq!(ints(&nokey_by_b.0, 0), evens_then_odds);
        assert_eq!(nokey_by_b.1, stable);
        // A unique NOT NULL index: by its column.
        assert_eq!(ints(&uniq.0, 0), up);
        assert_eq!(uniq.1, stable);
        assert_eq!(ints(&uniq_by_b.0, 0), odds_then_evens);
        assert_eq!(uniq_by_b.1, stable);
        // A nullable unique column, a partial and an expression index don't count: ctid.
        assert_eq!(ints(&uniq_null.0, 0), down);
        assert_eq!(uniq_null.1, stable);
        assert_eq!(ints(&skipped.0, 0), down);
        assert_eq!(skipped.1, stable);
        // Of two unique indexes, the one with fewer columns: (b), not (a, b).
        assert_eq!(ints(&two.0, 0), down);
        assert_eq!(two.1, stable);
        // A view has no ctid: no fixed order, and the page says so.
        assert_eq!(view.0.len(), 250);
        assert_eq!(view.1, vec![true; 3]);
    })
    .await;
}

#[tokio::test]
async fn page_order_is_not_fixed_on_a_large_keyless_table_or_an_inheritance_parent() {
    use sqlx::Connection;
    for_each_pg(|p, pw| async move {
        let url = common::url(&p, &pw);
        common::exec(
            &url,
            "DROP SCHEMA IF EXISTS ts_scratch CASCADE;
             CREATE SCHEMA ts_scratch;
             CREATE TABLE ts_scratch.big (a int, pad text);
             INSERT INTO ts_scratch.big SELECT g, repeat('x', 200) FROM generate_series(1, 120000) g;
             CREATE TABLE ts_scratch.parent (a int);
             CREATE TABLE ts_scratch.child () INHERITS (ts_scratch.parent);
             INSERT INTO ts_scratch.parent SELECT g FROM generate_series(1, 150) g;
             INSERT INTO ts_scratch.child SELECT g FROM generate_series(1, 150) g;
             CREATE TABLE ts_scratch.parent_u (a int NOT NULL UNIQUE);
             CREATE TABLE ts_scratch.child_u () INHERITS (ts_scratch.parent_u);
             INSERT INTO ts_scratch.parent_u SELECT g FROM generate_series(1, 150) g;
             INSERT INTO ts_scratch.child_u SELECT g FROM generate_series(1, 150) g;",
        )
        .await;
        let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
        let big_size: i64 = sqlx::query_scalar("SELECT pg_relation_size('ts_scratch.big')")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        conn.close().await.ok();
        let d = driver(&p, &pw).await;
        let page = |name: &'static str| page_req("ts_scratch", name, None, None, 0, 100);
        let big = d.fetch_page(&page("big")).await;
        let parent = d.fetch_page(&page("parent")).await;
        let parent_u = d.fetch_page(&page("parent_u")).await;
        d.close().await;
        common::exec(&url, "DROP SCHEMA IF EXISTS ts_scratch CASCADE").await;

        // Over 16 MiB: not sorted by ctid (the sort would read it all), and the page says so.
        assert!(big_size > 16 * 1024 * 1024, "ts_scratch.big is only {big_size} bytes");
        let big = big.unwrap();
        assert_eq!(big.rows.len(), 100);
        assert!(big.unstable_order);
        // The page reads the children too: neither ctid nor the parent's unique index orders them.
        for (name, p) in [("parent", parent), ("parent_u", parent_u)] {
            let p = p.unwrap();
            assert_eq!(p.rows.len(), 100, "{name}");
            assert_eq!(p.total, RowCount::Exact(300), "{name}");
            assert!(p.unstable_order, "{name}");
        }
    })
    .await;
}

#[tokio::test]
async fn stopping_a_page_cancels_its_query() {
    use sqlx::Connection;
    for_each_pg(|p, pw| async move {
        let url = common::url(&p, &pw);
        common::exec(
            &url,
            "DROP SCHEMA IF EXISTS ts_scratch CASCADE;
             CREATE SCHEMA ts_scratch;
             CREATE VIEW ts_scratch.slow AS SELECT pg_sleep(30)::text AS s;",
        )
        .await;
        let d = std::sync::Arc::new(driver(&p, &pw).await);
        let table = TableRef { schema: "ts_scratch".into(), name: "slow".into() };
        let fetch = tokio::spawn({
            let d = d.clone();
            async move { d.fetch_page(&page_req("ts_scratch", "slow", None, None, 0, 100)).await }
        });

        // Wait until the tagged page query is running on the server.
        let mut watch = sqlx::PgConnection::connect(&url).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let running: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE pid <> pg_backend_pid() AND state = 'active' AND left(query, 11) = '/* ts-page:'",
            )
            .fetch_one(&mut watch)
            .await
            .unwrap();
            if running > 0 {
                break;
            }
            assert!(Instant::now() < deadline, "the page query never showed in pg_stat_activity");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        watch.close().await.ok();

        let cancelled = d.cancel_pages(&table).await;
        let fetched = tokio::time::timeout(Duration::from_secs(5), fetch).await;
        d.close().await;
        common::exec(&url, "DROP SCHEMA IF EXISTS ts_scratch CASCADE").await;

        assert_eq!(cancelled.unwrap(), 1);
        let fetched = fetched.expect("the fetch didn't end within 5 s of Stop").unwrap();
        assert!(fetched.is_err(), "a cancelled page is an error: {fetched:?}");
    })
    .await;
}

#[tokio::test]
async fn cancel_pages_with_nothing_running_is_zero() {
    for_each_pg(|p, pw| async move {
        let d = driver(&p, &pw).await;
        let users = TableRef { schema: "fx".into(), name: "users".into() };
        assert_eq!(d.cancel_pages(&users).await.unwrap(), 0);
        // A finished page leaves nothing behind to cancel.
        d.fetch_page(&page_req("fx", "users", None, None, 0, 10)).await.unwrap();
        assert_eq!(d.cancel_pages(&users).await.unwrap(), 0);
        d.close().await;
    })
    .await;
}

#[tokio::test]
async fn stopping_a_page_waiting_on_a_lock_in_its_tiebreak_sends_no_row_query() {
    use sqlx::Connection;
    for_each_pg(|p, pw| async move {
        let url = common::url(&p, &pw);
        common::exec(
            &url,
            "DROP SCHEMA IF EXISTS ts_scratch CASCADE;
             CREATE SCHEMA ts_scratch;
             CREATE TABLE ts_scratch.locked (a int);",
        )
        .await;
        // Another session's open ALTER-like lock: the keyless table's tie-break lookup
        // (`pg_relation_size`) waits for it.
        let mut holder = sqlx::PgConnection::connect(&url).await.unwrap();
        sqlx::raw_sql("BEGIN; LOCK TABLE ts_scratch.locked IN ACCESS EXCLUSIVE MODE")
            .execute(&mut holder)
            .await
            .unwrap();
        let d = std::sync::Arc::new(driver(&p, &pw).await);
        let table = TableRef { schema: "ts_scratch".into(), name: "locked".into() };
        let fetch = tokio::spawn({
            let d = d.clone();
            async move { d.fetch_page(&page_req("ts_scratch", "locked", None, None, 0, 100)).await }
        });

        // Wait until the page's tagged tie-break lookup waits on the lock.
        let mut watch = sqlx::PgConnection::connect(&url).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE pid <> pg_backend_pid() AND state = 'active' AND wait_event_type = 'Lock'
                   AND left(query, 11) = '/* ts-page:'",
            )
            .fetch_one(&mut watch)
            .await
            .unwrap();
            if waiting > 0 {
                break;
            }
            assert!(Instant::now() < deadline, "the tie-break lookup never waited on the lock");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let cancelled = d.cancel_pages(&table).await;
        let fetched = tokio::time::timeout(Duration::from_secs(5), fetch).await;
        // Let the lock go: a row query sent after Stop would run now.
        sqlx::raw_sql("ROLLBACK").execute(&mut holder).await.unwrap();
        holder.close().await.ok();
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Any session's current or last query: the page's row query reads the table with LIMIT.
        let row_queries: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE pid <> pg_backend_pid() AND left(query, 11) = '/* ts-page:'
               AND position('\"ts_scratch\".\"locked\"' in query) > 0 AND position('LIMIT $' in query) > 0",
        )
        .fetch_one(&mut watch)
        .await
        .unwrap();
        watch.close().await.ok();
        d.close().await;
        common::exec(&url, "DROP SCHEMA IF EXISTS ts_scratch CASCADE").await;

        assert_eq!(cancelled.unwrap(), 1);
        let fetched = fetched.expect("the fetch didn't end within 5 s of Stop").unwrap();
        assert!(fetched.is_err(), "a stopped page is an error: {fetched:?}");
        assert_eq!(row_queries, 0, "the row query ran after Stop");
    })
    .await;
}
