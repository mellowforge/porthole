//! Table export: the grid's documents (its filter and sort, without paging) streamed through one
//! cursor. A JSON export writes each whole document; a CSV export writes the sampled columns.
//!
//! Canceling drops this future, which drops the cursor: the driver sends `killCursors`, and a
//! `find` still running is killed by the server when its connection goes. Nothing extra is needed.

use futures_util::TryStreamExt;
use mongodb::bson::Bson;

use super::{read_error, values, MongoDriver};
use crate::error::AppResult;
use crate::export::{ExportColumn, RowSink};
use crate::model::{row_types, CellType, TableQuery};

/// The `comment` of the export's `find`, which a test finds in `$currentOp`.
pub const EXPORT_COMMENT: &str = "ts-export";

pub(super) async fn export_table(driver: &MongoDriver, q: &TableQuery, sink: &mut dyn RowSink) -> AppResult<()> {
    let ns = format!("{}.{}", q.table.schema, q.table.name);
    let filter = super::parse_filter(q.filter.as_deref())?;
    let documents = sink.wants_documents();
    // CSV: the sampled columns, as the grid has them. JSON: whole documents, no columns.
    let columns = if documents {
        sink.begin(&[])?;
        Vec::new()
    } else {
        let columns = driver.sampled_structure(&q.table).await?.0.columns.clone();
        let export_columns: Vec<ExportColumn> =
            columns.iter().map(|c| ExportColumn { name: c.name.clone(), data_type: c.data_type.clone() }).collect();
        sink.begin(&export_columns)?;
        columns
    };

    let mut cursor = driver
        .collection(&q.table)
        .find(filter.unwrap_or_default())
        .sort(super::filter::sort_doc(q.sort.as_ref()))
        .comment(Bson::String(EXPORT_COMMENT.into()))
        .await
        .map_err(|e| read_error(e, &ns))?;
    while let Some(doc) = cursor.try_next().await.map_err(|e| read_error(e, &ns))? {
        if documents {
            sink.document(&values::ext_json(&Bson::Document(doc)))?;
        } else {
            let (texts, codes): (Vec<Option<String>>, Vec<CellType>) =
                columns.iter().map(|c| values::cell(doc.get(&c.name))).unzip();
            sink.typed_row(texts, Some(&row_types(&codes)))?;
        }
    }
    Ok(())
}
