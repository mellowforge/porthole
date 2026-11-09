//! Quick filter and grid sort compiled to MQL.

use mongodb::bson::oid::ObjectId;
use mongodb::bson::{doc, Bson, Decimal128, Document};

use super::values::parse_date;
use crate::error::AppError;
use crate::filter::{CmpOp, Expr, Literal};
use crate::model::SortSpec;

/// A field name starting with `$` would be sent as an operator.
pub const DOLLAR_FIELD: &str = "A field name can't start with $.";

/// Compiles the shared filter AST into a MongoDB query document.
pub fn to_mql(expr: &Expr) -> Result<Document, AppError> {
    match expr {
        Expr::And(a, b) => Ok(doc! { "$and": [to_mql(a)?, to_mql(b)?] }),
        Expr::Or(a, b) => Ok(doc! { "$or": [to_mql(a)?, to_mql(b)?] }),
        Expr::IsNull { column, column_pos, .. } | Expr::In { column, column_pos, .. } | Expr::Cmp { column, column_pos, .. }
            if column.starts_with('$') =>
        {
            Err(AppError::Filter { message: DOLLAR_FIELD.into(), position: *column_pos })
        }
        Expr::IsNull { column, negated, .. } => {
            let mut d = Document::new();
            if *negated {
                d.insert(column.clone(), doc! { "$ne": Bson::Null });
            } else {
                d.insert(column.clone(), Bson::Null);
            }
            Ok(d)
        }
        Expr::In { column, column_pos, values } => {
            let mut items: Vec<Bson> = Vec::new();
            for v in values {
                items.extend(forms(v, *column_pos)?);
            }
            let mut d = Document::new();
            d.insert(column.clone(), doc! { "$in": items });
            Ok(d)
        }
        Expr::Cmp { column, column_pos, op, value } => cmp(column, *column_pos, *op, value),
    }
}

/// Sort document: the chosen column, then `_id` ascending as a tie-break.
pub fn sort_doc(sort: Option<&SortSpec>) -> Document {
    let mut d = Document::new();
    if let Some(s) = sort {
        d.insert(s.column.clone(), if s.descending { -1 } else { 1 });
        if s.column == "_id" {
            return d;
        }
    }
    d.insert("_id", 1);
    d
}

fn cmp(column: &str, pos: usize, op: CmpOp, value: &Literal) -> Result<Document, AppError> {
    let mut d = Document::new();
    if op == CmpOp::Like {
        let text = match value {
            Literal::Str(s) | Literal::Num(s) => s,
        };
        d.insert(column, doc! { "$regex": like_regex(text), "$options": "i" });
        return Ok(d);
    }
    let all = forms(value, pos)?;
    let dual = all.len() > 1;
    match op {
        CmpOp::Eq if !dual => {
            d.insert(column, all.into_iter().next().unwrap_or(Bson::Null));
        }
        CmpOp::Eq => {
            d.insert(column, doc! { "$in": all });
        }
        CmpOp::Ne if !dual => {
            d.insert(column, doc! { "$ne": all.into_iter().next().unwrap_or(Bson::Null) });
        }
        CmpOp::Ne => {
            d.insert(column, doc! { "$nin": all });
        }
        _ => {
            let name = match op {
                CmpOp::Gt => "$gt",
                CmpOp::Lt => "$lt",
                CmpOp::Ge => "$gte",
                _ => "$lte",
            };
            if dual {
                let branches: Vec<Bson> = all
                    .into_iter()
                    .map(|v| {
                        let mut b = Document::new();
                        b.insert(column, doc! { name: v });
                        Bson::Document(b)
                    })
                    .collect();
                d.insert("$or", branches);
            } else {
                d.insert(column, doc! { name: all.into_iter().next().unwrap_or(Bson::Null) });
            }
        }
    }
    Ok(d)
}

/// The BSON values a literal stands for: a string first, then any typed reading of it.
fn forms(lit: &Literal, pos: usize) -> Result<Vec<Bson>, AppError> {
    match lit {
        Literal::Num(t) => Ok(vec![num(t, pos)?]),
        Literal::Str(s) => {
            let mut out = vec![Bson::String(s.clone())];
            if let Ok(oid) = ObjectId::parse_str(s) {
                out.push(Bson::ObjectId(oid));
            }
            let plain = !s.starts_with('{') && s.trim() == s;
            if let Some(d) = plain.then(|| parse_date(s)).flatten() {
                out.push(d);
            }
            match s.as_str() {
                "true" => out.push(Bson::Boolean(true)),
                "false" => out.push(Bson::Boolean(false)),
                _ => {}
            }
            Ok(out)
        }
    }
}

fn num(t: &str, pos: usize) -> Result<Bson, AppError> {
    let digits = t.strip_prefix('-').unwrap_or(t);
    let integer = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    let bad = || AppError::Filter { message: format!("Not a number: {t}"), position: pos };
    if integer {
        if let Ok(i) = t.parse::<i64>() {
            return Ok(Bson::Int64(i));
        }
        return t.parse::<Decimal128>().map(Bson::Decimal128).map_err(|_| bad());
    }
    t.parse::<f64>().map(Bson::Double).map_err(|_| bad())
}

fn like_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    for c in pattern.chars() {
        match c {
            '%' => out.push_str(".*"),
            '_' => out.push('.'),
            c if c.is_ascii_alphanumeric() || c.is_whitespace() || !c.is_ascii() => out.push(c),
            c => {
                out.push('\\');
                out.push(c);
            }
        }
    }
    out.push('$');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::parse;
    use mongodb::bson::DateTime;

    fn q(text: &str) -> Document {
        to_mql(&parse(text).unwrap()).unwrap()
    }

    const OID: &str = "65a1b2c3d4e5f60718293a4b";

    #[test]
    fn num_literal_is_int64_or_double() {
        assert_eq!(q("age = 30"), doc! { "age": 30_i64 });
        assert_eq!(q("x > 1.5"), doc! { "x": { "$gt": 1.5 } });
        assert_eq!(q("x != 3"), doc! { "x": { "$ne": 3_i64 } });
        let big = "1234567890123456789012345";
        let d = q(&format!("x = {big}"));
        assert_eq!(d, doc! { "x": big.parse::<Decimal128>().unwrap() });
    }

    #[test]
    fn str_that_reads_as_objectid_matches_both() {
        let oid = ObjectId::parse_str(OID).unwrap();
        assert_eq!(q(&format!("_id = '{OID}'")), doc! { "_id": { "$in": [OID, oid] } });
        assert_eq!(q(&format!("_id != '{OID}'")), doc! { "_id": { "$nin": [OID, oid] } });
        assert_eq!(q("_id = 'abc'"), doc! { "_id": "abc" });
        assert_eq!(q("_id != 'abc'"), doc! { "_id": { "$ne": "abc" } });
    }

    #[test]
    fn str_that_reads_as_date_compares_as_both() {
        let d = parse_date("2026-01-01").unwrap();
        assert_eq!(
            q("created > '2026-01-01'"),
            doc! { "$or": [ { "created": { "$gt": "2026-01-01" } }, { "created": { "$gt": d.clone() } } ] }
        );
        assert_eq!(
            q("created = '2026-01-01'"),
            doc! { "created": { "$in": ["2026-01-01", d] } }
        );
        let t = Bson::DateTime(DateTime::parse_rfc3339_str("2026-01-01T10:00:00Z").unwrap());
        assert_eq!(
            q("c <= '2026-01-01T10:00:00Z'"),
            doc! { "$or": [ { "c": { "$lte": "2026-01-01T10:00:00Z" } }, { "c": { "$lte": t } } ] }
        );
    }

    #[test]
    fn str_that_reads_as_bool_matches_both() {
        assert_eq!(q("ok = 'true'"), doc! { "ok": { "$in": ["true", true] } });
        assert_eq!(q("ok != 'false'"), doc! { "ok": { "$nin": ["false", false] } });
        assert_eq!(q("ok = 'True'"), doc! { "ok": "True" });
    }

    #[test]
    fn in_lists_flatten_both_forms() {
        let oid = ObjectId::parse_str(OID).unwrap();
        assert_eq!(
            q(&format!("x in ('a', 2, '{OID}')")),
            doc! { "x": { "$in": ["a", 2_i64, OID, oid] } }
        );
    }

    #[test]
    fn like_is_an_anchored_case_insensitive_regex() {
        assert_eq!(
            q("name like 'a%b_c.'"),
            doc! { "name": { "$regex": "^a.*b.c\\.$", "$options": "i" } }
        );
        assert_eq!(
            q("name like '(x)+[y]'"),
            doc! { "name": { "$regex": "^\\(x\\)\\+\\[y\\]$", "$options": "i" } }
        );
        assert_eq!(q("n like 12"), doc! { "n": { "$regex": "^12$", "$options": "i" } });
        // Every other regex character is literal too (a filter string has no escapes).
        assert_eq!(
            q(r"name like 'a\^$|?*{b'"),
            doc! { "name": { "$regex": r"^a\\\^\$\|\?\*\{b$", "$options": "i" } }
        );
    }

    #[test]
    fn a_dollar_field_is_a_filter_error_at_its_name() {
        let err = |text: &str| to_mql(&parse(text).unwrap()).unwrap_err();
        let at = |position: usize| AppError::Filter { message: DOLLAR_FIELD.into(), position };
        assert_eq!(err("`$where` = 'x'"), at(0));
        assert_eq!(err("a = 1 and `$c` is null"), at(10));
        assert_eq!(err("`$c` in (1, 2)"), at(0));
        assert_eq!(err("`$c` like 'x%'"), at(0));
        // Only a leading `$`: inside or after a dot it is part of the name.
        assert_eq!(q("`a$` = 1"), doc! { "a$": 1_i64 });
    }

    #[test]
    fn null_checks() {
        assert_eq!(q("x is null"), doc! { "x": Bson::Null });
        assert_eq!(q("x is not null"), doc! { "x": { "$ne": Bson::Null } });
    }

    #[test]
    fn dotted_path_in_backticks() {
        assert_eq!(q("`address.city` = 'Oslo'"), doc! { "address.city": "Oslo" });
    }

    #[test]
    fn and_or_nest() {
        assert_eq!(
            q("a = 1 and (b = 2 or c = 'z')"),
            doc! { "$and": [
                { "a": 1_i64 },
                { "$or": [ { "b": 2_i64 }, { "c": "z" } ] },
            ] }
        );
    }

    #[test]
    fn sort_adds_id_tiebreak() {
        assert_eq!(sort_doc(None), doc! { "_id": 1 });
        let s = |c: &str, d: bool| SortSpec { column: c.into(), descending: d };
        // `Document` equality ignores key order, and a sort's order is its meaning.
        let keys = |d: &Document| d.keys().cloned().collect::<Vec<_>>();
        let by_age = sort_doc(Some(&s("age", true)));
        assert_eq!(by_age, doc! { "age": -1, "_id": 1 });
        assert_eq!(keys(&by_age), ["age", "_id"]);
        let by_age = sort_doc(Some(&s("age", false)));
        assert_eq!(by_age, doc! { "age": 1, "_id": 1 });
        assert_eq!(keys(&by_age), ["age", "_id"]);
        assert_eq!(sort_doc(Some(&s("_id", true))), doc! { "_id": -1 });
        assert_eq!(keys(&sort_doc(Some(&s("_id", true)))), ["_id"]);
    }
}
