//! Statements with values whose rows still come through the text protocol, so a value reads the
//! same whether or not the statement had parameters.
//!
//! A prepared statement executed with `COM_STMT_EXECUTE` returns binary rows (doubles as IEEE
//! numbers, dates as structs). Instead the values go into user variables through one prepared
//! `SET`, and the statement runs as SQL `PREPARE` / `EXECUTE … USING`, whose rows are text.

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Params, QueryResult, TextProtocol, Value};

use crate::query::bind::{bind_value, BindValue};

/// The name of the SQL prepared statement `query_text` runs. It is replaced on every run and
/// never deallocated (after a KILL during `EXECUTE`, `DEALLOCATE` fails).
const STATEMENT: &str = "ts_q";

/// A text parameter as the value it binds as (`bind::bind_value`).
pub(crate) fn to_value(v: Option<&str>) -> Value {
    match bind_value(v) {
        BindValue::Null => Value::NULL,
        BindValue::Int(i) => Value::Int(i),
        BindValue::UInt(u) => Value::UInt(u),
        BindValue::Text(s) => Value::Bytes(s.into_bytes()),
    }
}

/// Whether a column of `data_type` (`information_schema.COLUMNS.DATA_TYPE`, which has no
/// `unsigned`) takes canonical integer text as an integer: the integer types, `year` and
/// `decimal`. Any other column compares and stores it as text, as it would a string.
fn takes_integers(data_type: &str) -> bool {
    let t = data_type.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    super::catalog::INTEGER_TYPES.contains(&t.as_str()) || matches!(t.as_str(), "integer" | "year" | "decimal" | "numeric")
}

/// A grid or filter parameter as the value it binds as, by the column it goes into or is compared
/// with: canonical integer text is an integer only for a column that `takes_integers`
/// (so a key above 2^53 still compares exactly); for any other column it is text, so `k = 5` on a
/// text column matches `5` only, not `05`. Query-tab parameters, whose column isn't known, keep
/// the shared rule (`to_value`).
pub(crate) fn to_column_value(v: Option<&str>, data_type: &str) -> Value {
    match v {
        Some(s) if !takes_integers(data_type) => Value::Bytes(s.as_bytes().to_vec()),
        other => to_value(other),
    }
}

/// `SET @ts_sql = ?, @ts_p1 = ?, …` for `n` values.
fn set_sql(n: usize) -> String {
    let mut sql = String::from("SET @ts_sql = ?");
    for i in 1..=n {
        sql.push_str(&format!(", @ts_p{i} = ?"));
    }
    sql
}

/// `EXECUTE ts_q USING @ts_p1, …` for `n` values.
fn execute_sql(n: usize) -> String {
    let vars: Vec<String> = (1..=n).map(|i| format!("@ts_p{i}")).collect();
    format!("EXECUTE {STATEMENT} USING {}", vars.join(", "))
}

/// One statement with `?` placeholders and the values for them, in order; rows through the text
/// protocol. With no values, a plain query.
pub async fn query_text<'a>(
    conn: &'a mut Conn,
    sql: &str,
    values: &[Option<String>],
) -> mysql_async::Result<QueryResult<'a, 'static, TextProtocol>> {
    query_values(conn, sql, values.iter().map(|v| to_value(v.as_deref())).collect()).await
}

/// As `query_text`, with values already bound (`to_column_value`).
pub(crate) async fn query_values<'a>(
    conn: &'a mut Conn,
    sql: &str,
    values: Vec<Value>,
) -> mysql_async::Result<QueryResult<'a, 'static, TextProtocol>> {
    if values.is_empty() {
        return conn.query_iter(sql.to_string()).await;
    }
    let n = values.len();
    let mut params = Vec::with_capacity(n + 1);
    params.push(Value::Bytes(sql.as_bytes().to_vec()));
    params.extend(values);
    conn.exec_drop(set_sql(n), Params::Positional(params)).await?;
    conn.query_drop(format!("PREPARE {STATEMENT} FROM @ts_sql")).await?;
    conn.query_iter(execute_sql(n)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_text_path_statements() {
        assert_eq!(set_sql(2), "SET @ts_sql = ?, @ts_p1 = ?, @ts_p2 = ?");
        assert_eq!(execute_sql(2), "EXECUTE ts_q USING @ts_p1, @ts_p2");
        assert_eq!(to_value(Some("18446744073709551615")), Value::UInt(u64::MAX));
        assert_eq!(to_value(Some("-3")), Value::Int(-3));
        assert_eq!(to_value(Some("007")), Value::Bytes(b"007".to_vec()));
        assert_eq!(to_value(None), Value::NULL);
    }

    #[test]
    fn mysql_grid_values_bind_by_their_column() {
        let text = |s: &str| Value::Bytes(s.as_bytes().to_vec());
        for t in ["tinyint", "smallint", "mediumint", "int", "bigint", "year", "decimal", "BIGINT"] {
            assert_eq!(to_column_value(Some("5"), t), Value::Int(5), "{t}");
            assert_eq!(to_column_value(Some("18446744073709551615"), t), Value::UInt(u64::MAX), "{t}");
            assert_eq!(to_column_value(Some("05"), t), text("05"), "{t}");
        }
        for t in ["varchar", "char", "text", "enum", "set", "double", "float", "bit", "date", "json", "binary"] {
            assert_eq!(to_column_value(Some("5"), t), text("5"), "{t}");
        }
        assert_eq!(to_column_value(None, "varchar"), Value::NULL);
        assert_eq!(to_column_value(None, "int"), Value::NULL);
    }
}
