//! The schema browser's catalog reads, run on a worker's connection.
//!
//! Table and schema names are bound arguments of the table-valued pragma functions
//! (`pragma_table_xinfo(?1, ?2)`, …), never SQL text. Only a schema's own `sqlite_master` needs its
//! name in the text, as a quoted identifier.

use std::collections::BTreeMap;

use rusqlite::{params, Connection};

use super::dialect::{affinity, SqliteDialect};
use super::query_error;
use crate::driver::Dialect;
use crate::error::{AppError, AppResult};
use crate::model::*;

/// `{schema}.sqlite_master`, the schema quoted.
fn master(schema: &str) -> String {
    format!("{}.sqlite_master", SqliteDialect.quote_ident(schema))
}

/// A failed read of a table's structure. SQLite's own failures keep their error (and code), so a
/// caller can map them its own way: a grid save reports a lock with its locked copy.
#[derive(Debug)]
pub(super) enum ReadError {
    Sqlite(rusqlite::Error),
    Other(AppError),
}

impl From<rusqlite::Error> for ReadError {
    fn from(e: rusqlite::Error) -> Self {
        ReadError::Sqlite(e)
    }
}

impl ReadError {
    /// As an `AppError`, SQLite's failures through `map`.
    pub(super) fn into_app(self, map: impl FnOnce(rusqlite::Error) -> AppError) -> AppError {
        match self {
            ReadError::Sqlite(e) => map(e),
            ReadError::Other(e) => e,
        }
    }
}

type ReadResult<T> = Result<T, ReadError>;

/// Runs `sql` with `params` and collects every row through `f`.
fn rows<T, P: rusqlite::Params>(
    c: &Connection,
    sql: &str,
    params: P,
    f: impl FnMut(&rusqlite::Row) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    let mut stmt = c.prepare(sql)?;
    stmt.query_map(params, f)?.collect()
}

/// The attached databases, `main` first, without `temp`.
pub(super) fn list_schemas(c: &Connection) -> AppResult<Vec<String>> {
    rows(c, "SELECT name FROM pragma_database_list WHERE name <> 'temp' ORDER BY seq", [], |r| r.get(0)).map_err(query_error)
}

/// Tables and views by name; SQLite's own `sqlite_%` tables are left out. No functions.
pub(super) fn schema_tree(c: &Connection, schema: &str) -> AppResult<SchemaTree> {
    let sql = format!(
        "SELECT name, type FROM {} WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
        master(schema)
    );
    let (mut tables, mut views) = (Vec::new(), Vec::new());
    for (name, kind) in rows(c, &sql, [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(query_error)? {
        if kind == "view" { views.push(name) } else { tables.push(name) }
    }
    Ok(SchemaTree { tables, views, functions: Vec::new() })
}

/// One `pragma_table_xinfo` row of a column the grid shows.
struct XColumn {
    info: ColumnInfo,
    /// Position in the primary key, from 1; 0 when not in it.
    pk: i64,
}

/// The table's columns in table order; virtual-table internals (`hidden == 1`) are left out.
fn table_columns(c: &Connection, table: &TableRef) -> ReadResult<Vec<XColumn>> {
    let all = rows(
        c,
        "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1, ?2) ORDER BY cid",
        params![table.name, table.schema],
        |r| {
            let declared: Option<String> = r.get(1)?;
            let declared = declared.unwrap_or_default();
            let pk: i64 = r.get(4)?;
            let hidden: i64 = r.get(5)?;
            Ok((
                hidden,
                XColumn {
                    info: ColumnInfo {
                        name: r.get(0)?,
                        data_type: declared.to_lowercase(),
                        cast_type: affinity(&declared).to_string(),
                        full_type: declared,
                        nullable: r.get::<_, i64>(2)? == 0,
                        default: r.get(3)?,
                        is_primary_key: pk > 0,
                        // 2 and 3: generated (virtual or stored); the database fills them in.
                        read_only: matches!(hidden, 2 | 3),
                    },
                    pk,
                },
            ))
        },
    )?;
    Ok(all.into_iter().filter(|(hidden, _)| *hidden != 1).map(|(_, col)| col).collect())
}

/// Primary-key column names in key order.
fn primary_key(cols: &[XColumn]) -> Vec<String> {
    let mut keyed: Vec<&XColumn> = cols.iter().filter(|c| c.pk > 0).collect();
    keyed.sort_by_key(|c| c.pk);
    keyed.into_iter().map(|c| c.info.name.clone()).collect()
}

fn indexes(c: &Connection, table: &TableRef) -> ReadResult<Vec<IndexInfo>> {
    let list = rows(
        c,
        "SELECT name, \"unique\", origin FROM pragma_index_list(?1, ?2) ORDER BY origin = 'pk' DESC, name",
        params![table.name, table.schema],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
    )?;
    list.into_iter()
        .map(|(name, unique, origin)| {
            // cid -2 is an expression, -1 the rowid: no column to show, as on Postgres.
            let columns = rows(
                c,
                "SELECT name FROM pragma_index_info(?1, ?2) WHERE cid >= 0 ORDER BY seqno",
                params![name, table.schema],
                |r| r.get(0),
            )?;
            Ok(IndexInfo { name, columns, included: Vec::new(), unique: unique != 0, primary: origin == "pk" })
        })
        .collect()
}

/// One foreign key's `pragma_foreign_key_list` rows, in `seq` order.
struct ForeignKeyRows {
    parent: String,
    from: Vec<String>,
    /// `None` when the key names no parent columns.
    to: Vec<Option<String>>,
}

fn foreign_keys(c: &Connection, table: &TableRef) -> ReadResult<Vec<ForeignKeyInfo>> {
    let list = rows(
        c,
        "SELECT id, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
        params![table.name, table.schema],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)),
    )?;
    let mut grouped: BTreeMap<i64, ForeignKeyRows> = BTreeMap::new();
    for (id, parent, from, to) in list {
        let fk = grouped.entry(id).or_insert_with(|| ForeignKeyRows { parent, from: Vec::new(), to: Vec::new() });
        fk.from.push(from);
        fk.to.push(to);
    }
    grouped
        .into_iter()
        .map(|(id, ForeignKeyRows { parent, from: columns, to })| {
            let ref_table = TableRef { schema: table.schema.clone(), name: parent };
            // `REFERENCES parent` without columns means the parent's primary key (none when the
            // parent doesn't exist).
            let ref_columns = if to.iter().all(Option::is_some) {
                to.into_iter().flatten().collect()
            } else {
                primary_key(&table_columns(c, &ref_table)?)
            };
            Ok(ForeignKeyInfo { name: format!("fk{id}"), columns, ref_table, ref_columns })
        })
        .collect()
}

/// `table` or `view`, or `None` when the schema has no such relation.
fn relation_type(c: &Connection, table: &TableRef) -> ReadResult<Option<String>> {
    let sql = format!("SELECT type FROM {} WHERE name = ?1 AND type IN ('table', 'view')", master(&table.schema));
    Ok(rows(c, &sql, params![table.name], |r| r.get(0))?.into_iter().next())
}

pub(super) fn table_structure(c: &Connection, table: &TableRef) -> AppResult<TableStructure> {
    read_table_structure(c, table).map_err(|e| e.into_app(query_error))
}

/// `table_structure`, with SQLite's failures as they came.
pub(super) fn read_table_structure(c: &Connection, table: &TableRef) -> ReadResult<TableStructure> {
    let not_found = || ReadError::Other(AppError::NotFound(format!("{}.{}", table.schema, table.name)));
    let kind = relation_type(c, table)?.ok_or_else(not_found)?;
    let cols = table_columns(c, table)?;
    if cols.is_empty() {
        return Err(not_found());
    }
    let primary_key = primary_key(&cols);
    let relation = if kind == "view" { RelationKind::View } else { RelationKind::Table };
    let float_key = cols.iter().any(|c| c.pk > 0 && c.info.cast_type == "real");
    let editing = if relation == RelationKind::View {
        Editing::View
    } else if primary_key.is_empty() {
        Editing::NoPrimaryKey
    } else if float_key {
        Editing::FloatKey
    } else {
        Editing::Allowed
    };
    let indexes = indexes(c, table)?;
    let foreign_keys = foreign_keys(c, table)?;
    let columns = cols.into_iter().map(|c| c.info).collect();
    Ok(TableStructure { columns, primary_key, relation, editing, indexes, foreign_keys })
}

/// Every table and view of `schema` with its column names in table order, for completion.
pub(super) fn schema_columns(c: &Connection, schema: &str) -> AppResult<BTreeMap<String, Vec<String>>> {
    let sql = format!(
        "SELECT m.name, p.name FROM {} m JOIN pragma_table_info(m.name, ?1) p \
         WHERE m.type IN ('table','view') AND m.name NOT LIKE 'sqlite_%' ORDER BY m.name, p.cid",
        master(schema)
    );
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (table, column) in
        rows(c, &sql, params![schema], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(query_error)?
    {
        out.entry(table).or_default().push(column);
    }
    Ok(out)
}
