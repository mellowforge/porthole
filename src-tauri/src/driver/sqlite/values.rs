//! SQLite values as the grid's text, with their storage type.

use rusqlite::types::ValueRef;

use crate::model::CellType;

/// The text of one value and its storage type: integers in decimal; reals as Rust's shortest
/// round-trip form, which keeps `.0` (`±∞` as `Inf` / `-Inf`); text as stored (invalid UTF-8
/// replaced); blobs as `\x` and lowercase hex, as Postgres prints `bytea`.
pub fn cell(v: ValueRef) -> (Option<String>, CellType) {
    match v {
        ValueRef::Null => (None, CellType::Null),
        ValueRef::Integer(i) => (Some(i.to_string()), CellType::Integer),
        ValueRef::Real(f) => (Some(real_text(f)), CellType::Real),
        ValueRef::Text(t) => (Some(String::from_utf8_lossy(t).into_owned()), CellType::Text),
        ValueRef::Blob(b) => (Some(blob_text(b)), CellType::Blob),
    }
}

fn real_text(f: f64) -> String {
    if f == f64::INFINITY {
        "Inf".into()
    } else if f == f64::NEG_INFINITY {
        "-Inf".into()
    } else {
        format!("{f:?}")
    }
}

fn blob_text(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 + 2 * b.len());
    s.push_str("\\x");
    for byte in b {
        s.push(HEX[usize::from(byte >> 4)] as char);
        s.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(v: ValueRef) -> String {
        cell(v).0.unwrap()
    }

    #[test]
    fn cell_text_rules() {
        assert_eq!(cell(ValueRef::Integer(42)), (Some("42".into()), CellType::Integer));
        assert_eq!(cell(ValueRef::Integer(i64::MIN)).0.as_deref(), Some("-9223372036854775808"));

        assert_eq!(cell(ValueRef::Real(1.0)), (Some("1.0".into()), CellType::Real));
        assert_eq!(text(ValueRef::Real(0.1)), "0.1");
        assert_eq!(text(ValueRef::Real(1e300)), "1e300");
        assert_eq!(text(ValueRef::Real(-0.0)), "-0.0");
        assert_eq!(text(ValueRef::Real(f64::INFINITY)), "Inf");
        assert_eq!(text(ValueRef::Real(f64::NEG_INFINITY)), "-Inf");

        assert_eq!(cell(ValueRef::Blob(&[0xde, 0xad])), (Some("\\xdead".into()), CellType::Blob));
        assert_eq!(text(ValueRef::Blob(&[])), "\\x");
        assert_eq!(text(ValueRef::Blob(&[0x00, 0x0f, 0xf0])), "\\x000ff0");

        assert_eq!(cell(ValueRef::Text(b"")), (Some(String::new()), CellType::Text));
        assert_eq!(text(ValueRef::Text("héllo".as_bytes())), "héllo");
        assert_eq!(cell(ValueRef::Null), (None, CellType::Null));
    }
}
