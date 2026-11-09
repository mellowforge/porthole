//! Table export: the grid's rows (its filter and sort, without paging), streamed on a connection
//! of the export's own, opened for it and closed after it, so the export never holds a pooled
//! connection and stopping it can't touch browsing.

use mysql_async::{Conn, Value};

use super::session::row_cells;
use super::{build_select, discard, filter_query_error, kill, query_error, text, MysqlConfig, MysqlDriver};
use crate::driver::Driver;
use crate::error::{AppError, AppResult};
use crate::export::{ExportColumn, RowSink};
use crate::model::TableQuery;

/// The export's connection. If it is dropped while its statement still runs (the export was
/// canceled, or the sink failed), a spawned task stops the statement with `KILL QUERY` and only
/// then closes the connection: closing it first would read the rest of the result (the client
/// reads a pending result before it quits), and the server would go on sending it.
struct ExportConn {
    /// `None` once closed.
    conn: Option<Conn>,
    config: MysqlConfig,
}

impl ExportConn {
    fn conn(&mut self) -> &mut Conn {
        self.conn.as_mut().expect("the export's connection is open until closed")
    }

    /// The statement has ended (its last row, or its error): the connection quits as usual.
    async fn close(mut self) {
        if let Some(conn) = self.conn.take() {
            let id = conn.id();
            if let Err(e) = conn.disconnect().await {
                eprintln!("closing the export's MySQL connection {id} failed: {}", super::error_message(&e));
            }
        }
    }
}

impl Drop for ExportConn {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else { return };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // No runtime to stop the statement from: dropping the connection closes its socket.
            return;
        };
        let config = self.config.clone();
        runtime.spawn(async move {
            let id = conn.id();
            if let Err(e) = kill::kill_query(&config, id).await {
                eprintln!("couldn't stop the canceled export's statement on MySQL connection {id}: {}", e.message());
            }
            // After the KILL only what the server had already sent is left to read.
            discard(conn);
        });
    }
}

/// Why the stream stopped early.
enum Stopped {
    /// The statement failed; it has ended.
    Db(mysql_async::Error),
    /// The sink refused a row (a cancel, or the file); the statement still runs.
    Sink(AppError),
}

/// Every row of `sql` to `sink`, with its cell types, through the text protocol.
async fn stream(conn: &mut Conn, sql: &str, values: Vec<Value>, sink: &mut dyn RowSink) -> Result<(), Stopped> {
    let mut result = text::query_values(conn, sql, values).await.map_err(Stopped::Db)?;
    while let Some(row) = result.next().await.map_err(Stopped::Db)? {
        let (texts, types) = row_cells(&row);
        sink.typed_row(texts, Some(&types)).map_err(Stopped::Sink)?;
    }
    result.drop_result().await.map_err(Stopped::Db)
}

/// Streams the table to `sink`: its columns, then each row of the grid's SELECT (`build_select`,
/// as `fetch_page` runs it, without LIMIT and OFFSET) with its cell types.
///
/// When the export future is dropped mid-stream (a cancel in `run_table_export`), or the sink
/// fails, `ExportConn`'s `Drop` stops the statement and closes the connection.
pub(super) async fn export_table(driver: &MysqlDriver, q: &TableQuery, sink: &mut dyn RowSink) -> AppResult<()> {
    let structure = driver.table_structure(&q.table).await?;
    let built = build_select(&driver.dialect, &structure.columns, q)?;
    let columns: Vec<ExportColumn> = structure
        .columns
        .iter()
        .map(|c| ExportColumn { name: c.name.clone(), data_type: c.data_type.clone() })
        .collect();
    sink.begin(&columns)?;

    let values = built.values();
    let mut export = ExportConn { conn: Some(driver.config.open().await?), config: driver.config.clone() };
    match stream(export.conn(), &built.sql, values, sink).await {
        Ok(()) => {
            export.close().await;
            Ok(())
        }
        Err(Stopped::Db(e)) => {
            export.close().await;
            Err(if built.where_sql.is_some() { filter_query_error(e) } else { query_error(e) })
        }
        // `export` is dropped here, with its statement still running.
        Err(Stopped::Sink(e)) => Err(e),
    }
}
