//! How a text parameter is bound: the one rule every driver shares.

/// What a parameter binds as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindValue {
    Null,
    Int(i64),
    UInt(u64),
    Text(String),
}

/// `None` is NULL. Canonical integer text (optional `-`, no leading zeros, `0` itself) is
/// signed if it fits `i64`, else unsigned if it fits `u64`. Everything else is text.
pub fn bind_value(v: Option<&str>) -> BindValue {
    let Some(s) = v else { return BindValue::Null };
    match (s.parse::<i64>(), s.parse::<u64>()) {
        (Ok(i), _) if i.to_string() == s => BindValue::Int(i),
        (Err(_), Ok(u)) if u.to_string() == s => BindValue::UInt(u),
        _ => BindValue::Text(s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_value_rule() {
        let text = |s: &str| BindValue::Text(s.into());
        assert_eq!(bind_value(Some("42")), BindValue::Int(42));
        assert_eq!(bind_value(Some("-7")), BindValue::Int(-7));
        assert_eq!(bind_value(Some("0")), BindValue::Int(0));
        for s in ["007", "+5", "-0", "1.0", ""] {
            assert_eq!(bind_value(Some(s)), text(s), "{s}");
        }
        assert_eq!(bind_value(Some("9223372036854775808")), BindValue::UInt(9223372036854775808));
        assert_eq!(bind_value(Some("18446744073709551616")), text("18446744073709551616"));
        assert_eq!(bind_value(None), BindValue::Null);
    }
}
