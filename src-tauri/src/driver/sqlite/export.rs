//! Table export: the grid's rows (its filter and sort, without paging), streamed on a connection
//! of the export's own, so stopping the export can't interrupt browsing.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use tokio::sync::mpsc;

use super::session::{relay, STREAM_BUFFER};
use super::{catalog, filtered_count_error, query_error, read_row, table_select, SqliteConfig, SqliteWorker, CLOSE_WAIT};
use crate::error::AppResult;
use crate::export::{ExportColumn, RowSink};
use crate::model::{TableQuery, TableStructure};

/// A row's text values and its storage types (`row_types`).
type TypedRow = (Vec<Option<String>>, String);

/// Sends every row of `sql` to `tx`. Stops quietly when the receiver is gone.
fn stream_rows(c: &Connection, sql: &str, bind: Vec<Value>, n_columns: usize, tx: &mpsc::Sender<TypedRow>) -> rusqlite::Result<()> {
    let mut stmt = c.prepare(sql)?;
    let mut rows = stmt.query(params_from_iter(bind))?;
    while let Some(r) = rows.next()? {
        if tx.blocking_send(read_row(r, n_columns)?).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Streams the table to `sink`: its columns, then each row with its cell types.
///
/// The connection is closed however the export ends. When the export future is dropped (a
/// cancel in `run_table_export`), dropping `worker` interrupts the statement and stops the
/// connection (`SqliteWorker`'s `Drop`), and the dropped call and receiver stop the job.
pub(super) async fn export_table(config: &SqliteConfig, q: &TableQuery, sink: &mut dyn RowSink) -> AppResult<()> {
    let worker = SqliteWorker::open_async(config).await?;
    let result = stream(&worker, q, sink).await;
    // The job has ended: the connection closes at once.
    if !worker.shutdown(CLOSE_WAIT).await {
        eprintln!("an export's SQLite connection is still busy after the export; it closes when its work ends");
    }
    result
}

async fn stream(worker: &SqliteWorker, q: &TableQuery, sink: &mut dyn RowSink) -> AppResult<()> {
    let tq = q.clone();
    let (structure, sql, params) = worker
        .call(move |c| -> AppResult<(TableStructure, String, Vec<String>)> {
            let structure = catalog::table_structure(c, &tq.table)?;
            let (sql, params) = table_select(&structure, &tq)?;
            Ok((structure, sql, params))
        })
        .await??;
    let columns: Vec<ExportColumn> = structure
        .columns
        .iter()
        .map(|c| ExportColumn { name: c.name.clone(), data_type: c.data_type.clone() })
        .collect();
    sink.begin(&columns)?;

    let filtered = q.filter.as_deref().is_some_and(|f| !f.trim().is_empty());
    let (tx, rx) = mpsc::channel::<TypedRow>(STREAM_BUFFER);
    let bind: Vec<Value> = params.into_iter().map(Value::Text).collect();
    let n_columns = columns.len();
    // `tx` lives in the job, so the channel closes when the job ends (or never runs).
    let job = worker.call(move |c| stream_rows(c, &sql, bind, n_columns, &tx));
    let result = relay(job, rx, |(row, types)| sink.typed_row(row, Some(&types))).await?;
    // The page with this filter has loaded, so a generic failure is the rows', not the filter's
    // (as for the filtered count).
    result.map_err(if filtered { filtered_count_error } else { query_error })
}
