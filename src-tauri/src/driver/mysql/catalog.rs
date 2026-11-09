//! The schema browser's catalog reads, from `information_schema`. A MySQL database is the app's
//! schema. Every name is a bound parameter.
//!
//! Names are sorted here, by their bytes, rather than by the server, whose case-insensitive
//! collation would put `Order Items` among the `o`s: so the tree reads as on SQLite.

use std::collections::BTreeMap;

use mysql_async::prelude::{FromValue, Queryable};
use mysql_async::{Conn, Params, Row, Value};

use super::query_error;
use crate::error::{AppError, AppResult};
use crate::model::*;

/// The server's own databases, left out of the schema list.
const SYSTEM_DATABASES_SQL: &str = "('information_schema', 'performance_schema', 'mysql', 'sys')";

/// The rows of `sql` with `params` bound in order.
async fn rows(conn: &mut Conn, sql: &str, params: &[&str]) -> AppResult<Vec<Row>> {
    let params = if params.is_empty() {
        Params::Empty
    } else {
        Params::Positional(params.iter().map(|p| Value::from(*p)).collect())
    };
    conn.exec(sql, params).await.map_err(query_error)
}

/// Column `i` of a catalog row.
fn get<T: FromValue>(row: &Row, i: usize) -> AppResult<T> {
    match row.get_opt::<T, usize>(i) {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(AppError::Query(format!("Unexpected catalog value: {e}"))),
        None => Err(AppError::Query(format!("The catalog read has no column {i}"))),
    }
}

/// The databases the user sees, without the server's own, by name.
pub(super) async fn list_schemas(conn: &mut Conn) -> AppResult<Vec<String>> {
    let sql = format!("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME NOT IN {SYSTEM_DATABASES_SQL}");
    let mut names = rows(conn, &sql, &[]).await?.iter().map(|r| get(r, 0)).collect::<AppResult<Vec<String>>>()?;
    names.sort();
    Ok(names)
}

const TABLES_SQL: &str = "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?";

const ROUTINES_SQL: &str = "
SELECT ROUTINE_NAME, ROUTINE_TYPE, SPECIFIC_NAME FROM information_schema.ROUTINES
WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE IN ('FUNCTION', 'PROCEDURE')";

/// Parameters in order; position 0 is a function's return type.
const PARAMETERS_SQL: &str = "
SELECT SPECIFIC_NAME, ROUTINE_TYPE, PARAMETER_MODE, PARAMETER_NAME, DTD_IDENTIFIER, DATA_TYPE
FROM information_schema.PARAMETERS
WHERE SPECIFIC_SCHEMA = ? AND ORDINAL_POSITION > 0
ORDER BY ORDINAL_POSITION";

pub(super) const INTEGER_TYPES: [&str; 5] = ["tinyint", "smallint", "mediumint", "int", "bigint"];

/// A parameter's type as MySQL 8 writes it: MariaDB adds a display width to integer types
/// (`int(11)`), which MySQL dropped in 8.0.19 (except `tinyint(1)`).
fn parameter_type(dtd: &str, data_type: &str) -> String {
    if !INTEGER_TYPES.contains(&data_type) {
        return dtd.to_string();
    }
    let Some(rest) = dtd.strip_prefix(data_type).and_then(|r| r.strip_prefix('(')) else {
        return dtd.to_string();
    };
    match rest.split_once(')') {
        Some((width, after))
            if !width.is_empty() && width.bytes().all(|b| b.is_ascii_digit()) && !(data_type == "tinyint" && width == "1") =>
        {
            format!("{data_type}{after}")
        }
        _ => dtd.to_string(),
    }
}

/// Tables (base and system-versioned) and views by name, and the functions and procedures with
/// their parameters: `{name} {type}` for a function, `{mode} {name} {type}` for a procedure.
pub(super) async fn schema_tree(conn: &mut Conn, db: &str) -> AppResult<SchemaTree> {
    let (mut tables, mut views) = (Vec::new(), Vec::new());
    for r in rows(conn, TABLES_SQL, &[db]).await? {
        let (name, kind): (String, String) = (get(&r, 0)?, get(&r, 1)?);
        match kind.as_str() {
            "BASE TABLE" | "SYSTEM VERSIONED" => tables.push(name),
            "VIEW" => views.push(name),
            _ => {}
        }
    }
    tables.sort();
    views.sort();

    // Parameters by routine: (specific name, routine type).
    let mut params: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for r in rows(conn, PARAMETERS_SQL, &[db]).await? {
        let routine: (String, String) = (get(&r, 0)?, get(&r, 1)?);
        let mode: Option<String> = get(&r, 2)?;
        let name: Option<String> = get(&r, 3)?;
        let dtd: Option<String> = get(&r, 4)?;
        let data_type: Option<String> = get(&r, 5)?;
        let ty = parameter_type(dtd.as_deref().unwrap_or(""), data_type.as_deref().unwrap_or(""));
        let name = name.unwrap_or_default();
        let text = match (routine.1.as_str(), mode) {
            ("PROCEDURE", Some(mode)) => format!("{mode} {name} {ty}"),
            _ => format!("{name} {ty}"),
        };
        params.entry(routine).or_default().push(text);
    }
    let mut functions = Vec::new();
    for r in rows(conn, ROUTINES_SQL, &[db]).await? {
        let (name, kind, specific): (String, String, String) = (get(&r, 0)?, get(&r, 1)?, get(&r, 2)?);
        let signature = params.get(&(specific, kind.clone())).map(|p| p.join(", ")).unwrap_or_default();
        let kind = if kind == "PROCEDURE" { RoutineKind::Procedure } else { RoutineKind::Function };
        functions.push(FunctionInfo { name, signature, kind: Some(kind) });
    }
    functions.sort_by(|a, b| {
        (&a.name, a.kind == Some(RoutineKind::Procedure)).cmp(&(&b.name, b.kind == Some(RoutineKind::Procedure)))
    });
    Ok(SchemaTree { tables, views, functions })
}

/// The table's columns in order, and whether the server is MariaDB (whose `COLUMN_DEFAULT` is
/// SQL text: `NULL` for no default, `'x'` for a string).
const COLUMNS_SQL: &str = "
SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, VERSION() LIKE '%MariaDB%'
FROM information_schema.COLUMNS
WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
ORDER BY ORDINAL_POSITION";

const TABLE_TYPE_SQL: &str = "SELECT TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";

/// Index parts in order. `COLUMN_NAME` is NULL for a MySQL functional key part.
const INDEXES_SQL: &str = "
SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS
WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
ORDER BY SEQ_IN_INDEX";

/// The key parts of the table's unique indexes other than the primary key, by index then part:
/// the column and its `IS_NULLABLE`, both NULL for a functional key part.
const UNIQUE_KEY_PARTS_SQL: &str = "
SELECT s.INDEX_NAME, s.COLUMN_NAME, c.IS_NULLABLE
FROM information_schema.STATISTICS s
LEFT JOIN information_schema.COLUMNS c
  ON c.TABLE_SCHEMA = s.TABLE_SCHEMA AND c.TABLE_NAME = s.TABLE_NAME AND c.COLUMN_NAME = s.COLUMN_NAME
WHERE s.TABLE_SCHEMA = ? AND s.TABLE_NAME = ? AND s.NON_UNIQUE = 0 AND s.INDEX_NAME <> 'PRIMARY'
ORDER BY s.INDEX_NAME, s.SEQ_IN_INDEX";

const FOREIGN_KEYS_SQL: &str = "
SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME
FROM information_schema.KEY_COLUMN_USAGE k
JOIN information_schema.REFERENTIAL_CONSTRAINTS r
  ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME
 AND r.TABLE_NAME = k.TABLE_NAME
WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ?
ORDER BY k.ORDINAL_POSITION";

/// A column's default as the grid shows it: none for MariaDB's `NULL` text, `auto_increment`
/// for an auto-increment column without one, otherwise as the server gives it.
fn column_default(default: Option<String>, extra: &str, mariadb: bool) -> Option<String> {
    let default = default.filter(|d| !(mariadb && d == "NULL"));
    if default.is_none() && extra.to_ascii_lowercase().contains("auto_increment") {
        return Some("auto_increment".into());
    }
    default
}

/// A generated column, which the server fills in. (`DEFAULT_GENERATED` is a column whose
/// default is an expression: it can be written.)
fn is_generated(extra: &str) -> bool {
    let extra = extra.to_ascii_uppercase();
    extra.contains("STORED GENERATED") || extra.contains("VIRTUAL GENERATED")
}

/// The table's (or view's) columns, keys, indexes and foreign keys.
pub(super) async fn table_structure(conn: &mut Conn, table: &TableRef) -> AppResult<TableStructure> {
    let names = [table.schema.as_str(), table.name.as_str()];
    let not_found = || AppError::NotFound(format!("{}.{}", table.schema, table.name));
    let table_type: String = match rows(conn, TABLE_TYPE_SQL, &names).await?.first() {
        Some(r) => get(r, 0)?,
        None => return Err(not_found()),
    };

    // Indexes in the order their parts came (SEQ_IN_INDEX), then the primary key first, by name.
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for r in rows(conn, INDEXES_SQL, &names).await? {
        let (name, non_unique, column): (String, i64, Option<String>) = (get(&r, 0)?, get(&r, 1)?, get(&r, 2)?);
        let at = match indexes.iter().position(|i| i.name == name) {
            Some(at) => at,
            None => {
                let primary = name == "PRIMARY";
                indexes.push(IndexInfo { name, columns: Vec::new(), included: Vec::new(), unique: non_unique == 0, primary });
                indexes.len() - 1
            }
        };
        indexes[at].columns.extend(column);
    }
    indexes.sort_by(|a, b| (!a.primary, &a.name).cmp(&(!b.primary, &b.name)));
    let primary_key: Vec<String> = indexes.iter().find(|i| i.primary).map(|i| i.columns.clone()).unwrap_or_default();

    let mut columns = Vec::new();
    for r in rows(conn, COLUMNS_SQL, &names).await? {
        let name: String = get(&r, 0)?;
        let data_type: String = get(&r, 1)?;
        let extra: String = get::<Option<String>>(&r, 5)?.unwrap_or_default();
        let mariadb: bool = get(&r, 6)?;
        columns.push(ColumnInfo {
            is_primary_key: primary_key.contains(&name),
            full_type: get(&r, 2)?,
            cast_type: data_type.clone(),
            nullable: get::<String>(&r, 3)? == "YES",
            default: column_default(get(&r, 4)?, &extra, mariadb),
            read_only: is_generated(&extra),
            name,
            data_type,
        });
    }
    if columns.is_empty() {
        return Err(not_found());
    }

    let mut grouped: BTreeMap<String, ForeignKeyInfo> = BTreeMap::new();
    for r in rows(conn, FOREIGN_KEYS_SQL, &names).await? {
        let name: String = get(&r, 0)?;
        let fk = match grouped.get_mut(&name) {
            Some(fk) => fk,
            None => {
                let ref_table = TableRef { schema: get(&r, 2)?, name: get(&r, 3)? };
                let fk = ForeignKeyInfo { name: name.clone(), columns: Vec::new(), ref_table, ref_columns: Vec::new() };
                grouped.entry(name).or_insert(fk)
            }
        };
        fk.columns.push(get(&r, 1)?);
        fk.ref_columns.push(get(&r, 4)?);
    }
    let foreign_keys = grouped.into_values().collect();

    let relation = if table_type == "VIEW" { RelationKind::View } else { RelationKind::Table };
    let float_key = columns.iter().any(|c| c.is_primary_key && matches!(c.data_type.as_str(), "float" | "double"));
    let editing = if relation == RelationKind::View {
        Editing::View
    } else if primary_key.is_empty() {
        Editing::NoPrimaryKey
    } else if float_key {
        Editing::FloatKey
    } else {
        Editing::Allowed
    };
    Ok(TableStructure { columns, primary_key, relation, editing, indexes, foreign_keys })
}

/// One key part of a unique index (`UNIQUE_KEY_PARTS_SQL`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct KeyPart {
    pub index: String,
    /// `None` for a functional key part.
    pub column: Option<String>,
    pub not_null: bool,
}

/// The key parts of the table's unique indexes other than the primary key, by index then part.
pub(super) async fn unique_key_parts(conn: &mut Conn, table: &TableRef) -> AppResult<Vec<KeyPart>> {
    let mut parts = Vec::new();
    for r in rows(conn, UNIQUE_KEY_PARTS_SQL, &[&table.schema, &table.name]).await? {
        let nullable: Option<String> = get(&r, 2)?;
        parts.push(KeyPart { index: get(&r, 0)?, column: get(&r, 1)?, not_null: nullable.as_deref() == Some("NO") });
    }
    Ok(parts)
}

/// The table's row count as the server estimates it (`TABLE_ROWS`); `None` for a view, or when
/// the server has none.
pub(super) async fn estimated_rows(conn: &mut Conn, table: &TableRef) -> AppResult<Option<u64>> {
    let sql = "SELECT TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";
    match rows(conn, sql, &[&table.schema, &table.name]).await?.first() {
        Some(r) => get(r, 0),
        None => Ok(None),
    }
}

/// Every table and view of `db` with its column names in order, for completion.
pub(super) async fn schema_columns(conn: &mut Conn, db: &str) -> AppResult<BTreeMap<String, Vec<String>>> {
    let sql = "SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? ORDER BY ORDINAL_POSITION";
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in rows(conn, sql, &[db]).await? {
        out.entry(get(&r, 0)?).or_default().push(get(&r, 1)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_parameter_types_drop_mariadb_display_widths() {
        assert_eq!(parameter_type("int(11)", "int"), "int");
        assert_eq!(parameter_type("int", "int"), "int");
        assert_eq!(parameter_type("bigint(20) unsigned", "bigint"), "bigint unsigned");
        assert_eq!(parameter_type("tinyint(4)", "tinyint"), "tinyint");
        assert_eq!(parameter_type("tinyint(1)", "tinyint"), "tinyint(1)");
        assert_eq!(parameter_type("decimal(10,2)", "decimal"), "decimal(10,2)");
        assert_eq!(parameter_type("varchar(20)", "varchar"), "varchar(20)");
    }

    #[test]
    fn mysql_column_defaults() {
        assert_eq!(column_default(Some("NULL".into()), "VIRTUAL GENERATED", true), None);
        // On MySQL, `NULL` text is a string default 'NULL'.
        assert_eq!(column_default(Some("NULL".into()), "", false).as_deref(), Some("NULL"));
        assert_eq!(column_default(None, "auto_increment", false).as_deref(), Some("auto_increment"));
        assert_eq!(column_default(Some("NULL".into()), "auto_increment", true).as_deref(), Some("auto_increment"));
        assert_eq!(column_default(Some("'x'".into()), "", true).as_deref(), Some("'x'"));
        assert_eq!(column_default(Some("CURRENT_TIMESTAMP".into()), "DEFAULT_GENERATED", false).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert!(is_generated("STORED GENERATED") && is_generated("VIRTUAL GENERATED"));
        assert!(!is_generated("DEFAULT_GENERATED") && !is_generated("auto_increment") && !is_generated(""));
    }
}
