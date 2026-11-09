//! SQL text for MySQL and MariaDB: backtick quoting and bare `?` parameters.
//!
//! A bare `?` is numbered by its place in the text. The edit builder and the filter compiler add
//! their parameters in text order and use each once, so the values bind in the right order.
//!
//! Values bind by their column (`text::to_column_value`): canonical integers as
//! integers in integer, `year` and `decimal` columns, everything else as text, which the server
//! converts for the column it is compared with or stored in. So no casts.

use crate::driver::Dialect;
use crate::edit::{is_number, render_literal};
use crate::model::{CellType, ColumnInfo};

pub struct MysqlDialect;

/// Column types whose values the SQL preview shows as bare numbers.
fn is_numeric_type(data_type: &str) -> bool {
    matches!(
        data_type,
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal" | "numeric" | "float" | "double"
            | "real"
    )
}

/// The hex digits of a binary value as the grid shows it (`\x0001` → `0001`); `None` for text that
/// isn't `\x` and hex digits.
fn hex_digits(v: &str) -> Option<&str> {
    v.strip_prefix("\\x").filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
}

impl Dialect for MysqlDialect {
    fn quote_ident(&self, s: &str) -> String {
        format!("`{}`", s.replace('`', "``"))
    }
    fn placeholder(&self, _n: usize) -> String {
        "?".into()
    }
    fn cast_param(&self, placeholder: &str, _cast_type: &str) -> String {
        placeholder.into()
    }
    /// The column itself: `LIKE` converts a number or a date to text by itself, and a text column
    /// keeps its own collation (so `like` is case-insensitive where the column is).
    fn text_expr(&self, quoted_col: &str) -> String {
        quoted_col.into()
    }
    /// A binary cell (or key) comes back as the grid shows it, `\x` and hex: the bytes are
    /// `UNHEX` of the text after `\x`. Other `b` cells (a BIT key's `00101`) bind as they are, as
    /// `preview_param` shows them.
    fn typed_param(&self, ph: &str, _col: &ColumnInfo, value: Option<&str>, original: Option<CellType>) -> String {
        if original == Some(CellType::Blob) && value.and_then(hex_digits).is_some() {
            format!("UNHEX(SUBSTRING({ph}, 3))")
        } else {
            ph.into()
        }
    }
    /// A binary value bound as `UNHEX(SUBSTRING(?, 3))` (`typed_param`) as the hex literal of the
    /// same bytes (`X'0001'`); a number bare in a numeric column; anything else as
    /// `render_literal` shows it.
    fn preview_param(&self, col: &ColumnInfo, value: Option<&str>, original: Option<CellType>) -> String {
        if let Some(hex) = value.and_then(hex_digits).filter(|_| original == Some(CellType::Blob)) {
            return format!("X'{hex}'");
        }
        match value {
            Some(v) if is_numeric_type(&col.data_type) && is_number(v) => v.to_string(),
            _ => render_literal(value, &col.data_type),
        }
    }
    /// MySQL has no `DEFAULT VALUES`.
    fn insert_defaults(&self, table: &str) -> String {
        format!("INSERT INTO {table} () VALUES ()")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::edit::plan_changes;
    use crate::model::*;

    fn col(name: &str, data_type: &str, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            full_type: data_type.into(),
            cast_type: data_type.into(),
            nullable: true,
            default: None,
            is_primary_key: pk,
            read_only: false,
        }
    }

    fn structure() -> TableStructure {
        TableStructure {
            columns: vec![col("k1", "int", true), col("k2", "varchar", true), col("a", "int", false), col("b", "text", false)],
            primary_key: vec!["k1".into(), "k2".into()],
            relation: RelationKind::Table,
            editing: Editing::Allowed,
            indexes: vec![],
            foreign_keys: vec![],
        }
    }

    fn table() -> TableRef {
        TableRef { schema: "fx".into(), name: "t".into() }
    }

    #[test]
    fn mysql_placeholders_follow_text_order() {
        let d: &dyn Dialect = &MysqlDialect;
        let values: CellValues = [("a".to_string(), Some("1".to_string())), ("b".to_string(), Some("x".to_string()))].into();
        let update = RowChange::Update { key: vec!["7".into(), "k".into()], values, original_types: BTreeMap::new() };
        let set = ChangeSet { table: table(), changes: vec![update] };
        let planned = plan_changes(d, &structure(), &set).unwrap();
        assert_eq!(planned[0].sql, "UPDATE `fx`.`t` SET `a` = ?, `b` = ? WHERE `k1` = ? AND `k2` = ?");
        assert_eq!(planned[0].sql.matches('?').count(), 4);
        assert_eq!(planned[0].params, ["1", "x", "7", "k"].map(|s| Some(s.to_string())));
        assert_eq!(planned[0].preview, "UPDATE `fx`.`t` SET `a` = 1, `b` = 'x' WHERE `k1` = 7 AND `k2` = 'k';");

        let filter = crate::filter::parse(r#"a = 1 and b = "x""#).unwrap();
        let (sql, params) = crate::filter::compile(&filter, &structure().columns, d, 1).unwrap();
        assert_eq!(sql, "(`a` = ? AND `b` = ?)");
        assert_eq!(params, ["1", "x"]);

        let insert = ChangeSet { table: table(), changes: vec![RowChange::Insert { values: CellValues::new() }] };
        let planned = plan_changes(d, &structure(), &insert).unwrap();
        assert_eq!(planned[0].sql, "INSERT INTO `fx`.`t` () VALUES ()");
        assert!(planned[0].params.is_empty());
    }

    #[test]
    fn mysql_dialect_hooks() {
        let d: &dyn Dialect = &MysqlDialect;
        assert_eq!(d.quote_ident("Order `Items`"), "`Order ``Items```");
        assert_eq!(d.placeholder(3), "?");
        assert_eq!(d.cast_param("?", "int"), "?");
        assert_eq!(d.text_expr("`k`"), "`k`");
        assert_eq!(d.literal_param("?", &col("k", "int", false), true), "?");
        let bin = col("id", "binary", true);
        assert_eq!(d.typed_param("?", &bin, Some("\\x0001"), Some(CellType::Blob)), "UNHEX(SUBSTRING(?, 3))");
        for original in [None, Some(CellType::Text), Some(CellType::Integer), Some(CellType::Null)] {
            assert_eq!(d.typed_param("?", &bin, Some("\\x0001"), original), "?", "{original:?}");
        }
    }

    /// A BIT cell is `b` too, but its text is binary digits (`00101`), not `\x` and hex: it
    /// binds as it is, never as `UNHEX` of its text.
    #[test]
    fn mysql_only_hex_text_binds_as_bytes() {
        let d: &dyn Dialect = &MysqlDialect;
        let bit = col("bt", "bit", true);
        let blob = Some(CellType::Blob);
        assert_eq!(d.typed_param("?", &bit, Some("00101"), blob), "?");
        assert_eq!(d.preview_param(&bit, Some("00101"), blob), "'00101'");
        assert_eq!(d.typed_param("?", &bit, None, blob), "?");
        // Bound and shown alike: only `\x` and hex digits are bytes.
        assert_eq!(d.typed_param("?", &col("id", "binary", true), Some("\\xzz"), blob), "?");
        assert_eq!(d.typed_param("?", &col("id", "binary", true), Some("\\x00ff"), blob), "UNHEX(SUBSTRING(?, 3))");
    }

    #[test]
    fn mysql_preview_shows_binary_values_as_the_hex_literal_that_runs() {
        let d: &dyn Dialect = &MysqlDialect;
        let bin = col("id", "binary", true);
        let blob = Some(CellType::Blob);
        assert_eq!(d.preview_param(&bin, Some("\\x0001"), blob), "X'0001'");
        assert_eq!(d.preview_param(&bin, Some("\\xDEadbeef"), blob), "X'DEadbeef'");
        assert_eq!(d.preview_param(&bin, Some("\\x"), blob), "X''");
        assert_eq!(d.preview_param(&bin, None, blob), "NULL");
        // Only a binary cell's `\x` text binds as bytes; other text is shown as the text it is.
        assert_eq!(d.preview_param(&bin, Some("\\x0001"), Some(CellType::Text)), "'\\x0001'");
        assert_eq!(d.preview_param(&bin, Some("\\x0001"), None), "'\\x0001'");
        assert_eq!(d.preview_param(&bin, Some("\\xzz"), blob), "'\\xzz'");
        assert_eq!(d.preview_param(&col("n", "int", false), Some("5"), blob), "5");
    }

    #[test]
    fn mysql_preview_shows_numbers_bare_in_numeric_columns() {
        let d: &dyn Dialect = &MysqlDialect;
        for t in ["tinyint", "smallint", "mediumint", "int", "bigint", "decimal", "float", "double"] {
            assert_eq!(d.preview_param(&col("c", t, false), Some("-1.5e3"), None), "-1.5e3", "{t}");
            assert_eq!(d.preview_param(&col("c", t, false), Some("abc"), None), "'abc'", "{t}");
        }
        assert_eq!(d.preview_param(&col("c", "varchar", false), Some("5"), None), "'5'");
        assert_eq!(d.preview_param(&col("c", "text", false), Some("it's"), None), "'it''s'");
        assert_eq!(d.preview_param(&col("c", "int", false), None, None), "NULL");
        assert_eq!(d.preview_param(&col("c", "int", false), Some("NaN"), None), "'NaN'");
    }
}
