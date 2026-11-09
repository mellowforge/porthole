//! Values read through the text protocol: their text and cell type, decided by the result column.

use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Column, Value};

use crate::model::CellType;

/// The character set number MySQL gives binary strings, blobs and other byte values.
const BINARY_CHARSET: u16 = 63;

/// A column whose values are bytes, shown as `\x` and hex: a binary string or blob, or GEOMETRY.
fn is_binary(col: &Column) -> bool {
    use ColumnType::*;
    match col.column_type() {
        MYSQL_TYPE_GEOMETRY => true,
        MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR | MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => col.character_set() == BINARY_CHARSET,
        _ => false,
    }
}

/// The cell type of every value in `col`, NULL aside.
fn cell_type(col: &Column) -> CellType {
    use ColumnType::*;
    match col.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_YEAR => CellType::Integer,
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => CellType::Real,
        MYSQL_TYPE_BIT => CellType::Blob,
        _ if is_binary(col) => CellType::Blob,
        _ => CellType::Text,
    }
}

/// `\x` and the bytes in lowercase hex, as Postgres prints `bytea`.
fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + 2 * b.len());
    s.push_str("\\x");
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// A BIT(`width`) value (big-endian bytes) as `width` binary digits.
fn bits(b: &[u8], width: usize) -> String {
    let all: String = b.iter().map(|byte| format!("{byte:08b}")).collect();
    if all.len() >= width {
        all[all.len() - width..].to_string()
    } else {
        format!("{}{all}", "0".repeat(width - all.len()))
    }
}

/// The text of a value the text protocol never sends (it sends bytes or NULL); for completeness.
fn other_text(v: &Value) -> String {
    match v {
        Value::NULL => String::new(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Date(y, mo, d, h, mi, s, us) => {
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}{}", fraction(*us))
        }
        Value::Time(neg, d, h, mi, s, us) => {
            let hours = *d as u64 * 24 + *h as u64;
            format!("{}{hours:02}:{mi:02}:{s:02}{}", if *neg { "-" } else { "" }, fraction(*us))
        }
    }
}

fn fraction(us: u32) -> String {
    if us == 0 { String::new() } else { format!(".{us:06}") }
}

/// The text and cell type of `v`, a value of result column `col`: the server's text, decoded as
/// UTF-8 (lossy), except binary values (`\x` and hex) and BIT(n) (`n` binary digits).
pub fn cell(v: &Value, col: &Column) -> (Option<String>, CellType) {
    let Value::Bytes(b) = v else {
        return match v {
            Value::NULL => (None, CellType::Null),
            other => (Some(other_text(other)), cell_type(col)),
        };
    };
    let text = if col.column_type() == ColumnType::MYSQL_TYPE_BIT {
        bits(b, col.column_length() as usize)
    } else if is_binary(col) {
        hex(b)
    } else {
        String::from_utf8_lossy(b).into_owned()
    };
    (Some(text), cell_type(col))
}

/// The lowercase type of a query result column, e.g. `int`, `varchar`, `varbinary`, `json`.
pub fn type_name(col: &Column) -> String {
    use ColumnType::*;
    let binary = col.character_set() == BINARY_CHARSET;
    let pick = |text: &'static str, bin: &'static str| if binary { bin } else { text };
    let flags = col.flags();
    let name = match col.column_type() {
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        _ if flags.contains(ColumnFlags::ENUM_FLAG) => "enum",
        _ if flags.contains(ColumnFlags::SET_FLAG) => "set",
        MYSQL_TYPE_STRING => pick("char", "binary"),
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => pick("varchar", "varbinary"),
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => pick("text", "blob"),
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_NULL => "null",
        MYSQL_TYPE_VECTOR => "vector",
        MYSQL_TYPE_TYPED_ARRAY | MYSQL_TYPE_UNKNOWN => "unknown",
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::consts::{ColumnFlags, ColumnType};
    use mysql_async::{Column, Value};

    fn col(t: ColumnType, charset: u16) -> Column {
        Column::new(t).with_character_set(charset)
    }
    fn bytes(b: &[u8]) -> Value {
        Value::Bytes(b.to_vec())
    }

    #[test]
    fn cell_rules() {
        let bin = col(ColumnType::MYSQL_TYPE_VAR_STRING, 63);
        assert_eq!(cell(&bytes(&[0xde, 0xad]), &bin), (Some("\\xdead".into()), CellType::Blob));
        assert_eq!(cell(&bytes(&[]), &bin), (Some("\\x".into()), CellType::Blob));
        let bit = col(ColumnType::MYSQL_TYPE_BIT, 63).with_column_length(5);
        assert_eq!(cell(&bytes(&[0x05]), &bit), (Some("00101".into()), CellType::Blob));
        let my_json = col(ColumnType::MYSQL_TYPE_JSON, 63);
        assert_eq!(cell(&bytes(b"{\"a\": [1, 2]}"), &my_json), (Some("{\"a\": [1, 2]}".into()), CellType::Text));
        let maria_json = col(ColumnType::MYSQL_TYPE_BLOB, 224);
        assert_eq!(cell(&bytes(b"null"), &maria_json), (Some("null".into()), CellType::Text));
        let double = col(ColumnType::MYSQL_TYPE_DOUBLE, 63);
        assert_eq!(cell(&bytes(b"1e300"), &double), (Some("1e300".into()), CellType::Real));
        let big = col(ColumnType::MYSQL_TYPE_LONGLONG, 63).with_flags(ColumnFlags::UNSIGNED_FLAG);
        assert_eq!(cell(&bytes(b"18446744073709551615"), &big), (Some("18446744073709551615".into()), CellType::Integer));
        assert_eq!(cell(&Value::NULL, &double), (None, CellType::Null));
    }

    #[test]
    fn mysql_cell_rules_beyond_the_brief() {
        // BIT wider than a byte, and a binary GEOMETRY.
        let bit = col(ColumnType::MYSQL_TYPE_BIT, 63).with_column_length(10);
        assert_eq!(cell(&bytes(&[0x02, 0x01]), &bit).0.as_deref(), Some("1000000001"));
        let geo = col(ColumnType::MYSQL_TYPE_GEOMETRY, 63);
        assert_eq!(cell(&bytes(&[0x01]), &geo), (Some("\\x01".into()), CellType::Blob));
        // Text is decoded lossily; DECIMAL is real; YEAR and TINY are integers; dates are text.
        let text = col(ColumnType::MYSQL_TYPE_VAR_STRING, 224);
        assert_eq!(cell(&bytes(b"caf\xc3\xa9"), &text), (Some("café".into()), CellType::Text));
        assert_eq!(cell(&bytes(&[0xff]), &text).0.as_deref(), Some("\u{fffd}"));
        assert_eq!(cell(&bytes(b"12.30"), &col(ColumnType::MYSQL_TYPE_NEWDECIMAL, 63)).1, CellType::Real);
        assert_eq!(cell(&bytes(b"2026"), &col(ColumnType::MYSQL_TYPE_YEAR, 63)).1, CellType::Integer);
        assert_eq!(cell(&bytes(b"1"), &col(ColumnType::MYSQL_TYPE_TINY, 63)).1, CellType::Integer);
        assert_eq!(cell(&bytes(b"2026-01-02"), &col(ColumnType::MYSQL_TYPE_DATE, 63)).1, CellType::Text);
        // A text BLOB (TEXT) is text, not binary.
        assert_eq!(cell(&bytes(b"x"), &col(ColumnType::MYSQL_TYPE_BLOB, 224)), (Some("x".into()), CellType::Text));
    }

    #[test]
    fn mysql_type_names() {
        use ColumnType::*;
        let name = |t, cs| type_name(&col(t, cs));
        assert_eq!(name(MYSQL_TYPE_LONG, 63), "int");
        assert_eq!(name(MYSQL_TYPE_INT24, 63), "mediumint");
        assert_eq!(name(MYSQL_TYPE_LONGLONG, 63), "bigint");
        assert_eq!(name(MYSQL_TYPE_NEWDECIMAL, 63), "decimal");
        assert_eq!(name(MYSQL_TYPE_DATETIME2, 63), "datetime");
        assert_eq!(name(MYSQL_TYPE_JSON, 63), "json");
        assert_eq!(name(MYSQL_TYPE_VAR_STRING, 224), "varchar");
        assert_eq!(name(MYSQL_TYPE_VAR_STRING, 63), "varbinary");
        assert_eq!(name(MYSQL_TYPE_STRING, 224), "char");
        assert_eq!(name(MYSQL_TYPE_STRING, 63), "binary");
        assert_eq!(name(MYSQL_TYPE_BLOB, 224), "text");
        assert_eq!(name(MYSQL_TYPE_BLOB, 63), "blob");
        assert_eq!(type_name(&col(MYSQL_TYPE_STRING, 224).with_flags(ColumnFlags::ENUM_FLAG)), "enum");
        assert_eq!(type_name(&col(MYSQL_TYPE_STRING, 224).with_flags(ColumnFlags::SET_FLAG)), "set");
        assert_eq!(name(MYSQL_TYPE_GEOMETRY, 63), "geometry");
    }
}
