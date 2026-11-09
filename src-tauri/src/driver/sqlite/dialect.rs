//! SQL text for SQLite: quoting, parameters, and how bound values get their storage type.
//!
//! Every parameter is bound as TEXT. A column with INTEGER, REAL, NUMERIC or TEXT affinity
//! converts it on comparison and on storage; a column without affinity (`cast_type == "blob"`)
//! keeps it as text. There, `({ph} + 0)` makes a number literal (or an edited number) numeric.
//!
//! An edited value becomes a number only when the cell held a number before. So a number typed
//! into a column with no declared type is stored as text both in an insert and in an edit of a
//! NULL cell: there is no earlier number to go by, and this keeps it possible to enter the text
//! `'5'`. (A query tab's `insert` stores a number as a number.)

use crate::driver::Dialect;
use crate::model::{CellType, ColumnInfo};

pub struct SqliteDialect;

/// The column affinity of a declared type, by SQLite's rules, in their order: `INT` → integer;
/// `CHAR`, `CLOB` or `TEXT` → text; `BLOB` or no type → blob; `REAL`, `FLOA` or `DOUB` → real;
/// anything else → numeric.
pub(super) fn affinity(declared: &str) -> &'static str {
    let t = declared.to_ascii_uppercase();
    let has = |s: &str| t.contains(s);
    if has("INT") {
        "integer"
    } else if has("CHAR") || has("CLOB") || has("TEXT") {
        "text"
    } else if has("BLOB") || t.trim().is_empty() {
        "blob"
    } else if has("REAL") || has("FLOA") || has("DOUB") {
        "real"
    } else {
        "numeric"
    }
}

/// Whether the text is a finite number. Unlike the shared binding rule (`query::bind`, canonical
/// integers only), it also accepts reals and non-canonical forms: a no-affinity column keeps
/// whatever number the user typed.
fn numeric_text(value: &str) -> bool {
    value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok_and(f64::is_finite)
}

/// Whether `typed_param` binds the value as a number: a number, edited into (or naming a row by)
/// a no-affinity column whose cell held a number.
fn binds_as_number(col: &ColumnInfo, value: Option<&str>, original: Option<CellType>) -> bool {
    let was_number = matches!(original, Some(CellType::Integer | CellType::Real));
    col.cast_type == "blob" && was_number && value.is_some_and(numeric_text)
}

fn as_number(ph: &str) -> String {
    format!("({ph} + 0)")
}

impl Dialect for SqliteDialect {
    fn quote_ident(&self, s: &str) -> String {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
    fn placeholder(&self, n: usize) -> String {
        format!("?{n}")
    }
    /// SQLite has no parameter casts.
    fn cast_param(&self, placeholder: &str, _cast_type: &str) -> String {
        placeholder.to_string()
    }
    /// The cell's text as the grid shows it: a blob as `\x` plus lowercase hex, anything else
    /// cast to TEXT. So `like` matches what the user sees.
    fn text_expr(&self, quoted_col: &str) -> String {
        format!("CASE WHEN typeof({quoted_col}) = 'blob' THEN '\\x' || lower(hex({quoted_col})) ELSE CAST({quoted_col} AS TEXT) END")
    }
    /// A number edited into a no-affinity column stays a number when the cell held one.
    fn typed_param(&self, ph: &str, col: &ColumnInfo, value: Option<&str>, original: Option<CellType>) -> String {
        if binds_as_number(col, value, original) {
            as_number(ph)
        } else {
            ph.to_string()
        }
    }
    /// What runs: a number where `typed_param` makes one, otherwise the text that is bound.
    fn preview_param(&self, col: &ColumnInfo, value: Option<&str>, original: Option<CellType>) -> String {
        match value {
            None => "NULL".into(),
            Some(v) if binds_as_number(col, value, original) => v.to_string(),
            Some(v) => format!("'{}'", v.replace('\'', "''")),
        }
    }
    /// `k = 5` finds the number 5 in a no-affinity column, `k = "5"` the text.
    fn literal_param(&self, ph: &str, col: &ColumnInfo, numeric: bool) -> String {
        if numeric && col.cast_type == "blob" {
            as_number(ph)
        } else {
            ph.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(cast_type: &str) -> ColumnInfo {
        ColumnInfo {
            name: "k".into(),
            data_type: String::new(),
            full_type: String::new(),
            cast_type: cast_type.into(),
            nullable: true,
            default: None,
            is_primary_key: false,
            read_only: false,
        }
    }

    #[test]
    fn sqlite_dialect_basics() {
        let d = SqliteDialect;
        assert_eq!(d.quote_ident("Order \"Items\""), "\"Order \"\"Items\"\"\"");
        assert_eq!(d.placeholder(3), "?3");
        assert_eq!(d.cast_param("?1", "integer"), "?1");
        assert_eq!(
            d.text_expr("\"k\""),
            "CASE WHEN typeof(\"k\") = 'blob' THEN '\\x' || lower(hex(\"k\")) ELSE CAST(\"k\" AS TEXT) END"
        );
    }

    #[test]
    fn affinity_follows_sqlite_rules_in_order() {
        for (declared, expected) in [
            ("INTEGER", "integer"),
            ("int", "integer"),
            ("BIGINT", "integer"),
            ("UNSIGNED BIG INT", "integer"),
            // INT comes first: "POINT" and "CHARINT" are integer.
            ("FLOATING POINT", "integer"),
            ("CHARINT", "integer"),
            ("VARCHAR(20)", "text"),
            ("nchar(5)", "text"),
            ("CLOB", "text"),
            ("text", "text"),
            ("BLOB", "blob"),
            ("", "blob"),
            ("  ", "blob"),
            ("REAL", "real"),
            ("float", "real"),
            ("Double Precision", "real"),
            ("NUMERIC", "numeric"),
            ("DECIMAL(10,5)", "numeric"),
            ("BOOLEAN", "numeric"),
            ("DATE", "numeric"),
            ("DATETIME", "numeric"),
        ] {
            assert_eq!(affinity(declared), expected, "{declared}");
        }
    }

    #[test]
    fn typed_param_makes_numbers_numeric_only_without_affinity() {
        let d: &dyn Dialect = &SqliteDialect;
        let blob = col("blob");
        let int = Some(CellType::Integer);
        let real = Some(CellType::Real);
        assert_eq!(d.typed_param("?1", &blob, Some("5"), int), "(?1 + 0)");
        assert_eq!(d.typed_param("?1", &blob, Some("-2.5"), real), "(?1 + 0)");
        assert_eq!(d.typed_param("?1", &blob, Some("1e3"), int), "(?1 + 0)");
        // Not a number, or not a finite one.
        for v in ["abc", "", " 5", "inf", "NaN", "1e999"] {
            assert_eq!(d.typed_param("?1", &blob, Some(v), int), "?1", "{v}");
        }
        assert_eq!(d.typed_param("?1", &blob, None, int), "?1");
        // The cell held text, a blob, NULL, or the page had no types.
        for original in [Some(CellType::Text), Some(CellType::Blob), Some(CellType::Null), None] {
            assert_eq!(d.typed_param("?1", &blob, Some("5"), original), "?1", "{original:?}");
        }
        // Columns with an affinity convert by themselves.
        for cast in ["integer", "real", "numeric", "text"] {
            assert_eq!(d.typed_param("?2", &col(cast), Some("5"), int), "?2", "{cast}");
        }
    }

    #[test]
    fn preview_param_shows_what_typed_param_binds() {
        let d: &dyn Dialect = &SqliteDialect;
        let int = Some(CellType::Integer);
        assert_eq!(d.preview_param(&col("blob"), Some("5"), int), "5");
        assert_eq!(d.preview_param(&col("blob"), Some("-2.5"), Some(CellType::Real)), "-2.5");
        assert_eq!(d.preview_param(&col("blob"), Some("5"), Some(CellType::Text)), "'5'");
        assert_eq!(d.preview_param(&col("blob"), Some("abc"), int), "'abc'");
        assert_eq!(d.preview_param(&col("integer"), Some("5"), int), "'5'");
        assert_eq!(d.preview_param(&col("numeric"), Some("5"), None), "'5'");
        assert_eq!(d.preview_param(&col("text"), Some("it's"), None), "'it''s'");
        assert_eq!(d.preview_param(&col("blob"), None, int), "NULL");
    }

    #[test]
    fn literal_param_makes_number_literals_numeric_only_without_affinity() {
        let d: &dyn Dialect = &SqliteDialect;
        assert_eq!(d.literal_param("?1", &col("blob"), true), "(?1 + 0)");
        assert_eq!(d.literal_param("?1", &col("blob"), false), "?1");
        for cast in ["integer", "real", "numeric", "text"] {
            assert_eq!(d.literal_param("?3", &col(cast), true), "?3", "{cast}");
        }
    }
}
