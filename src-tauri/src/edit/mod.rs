use std::collections::BTreeMap;

use crate::driver::Dialect;
use crate::error::{AppError, AppResult};
use crate::model::{CellType, CellValues, ChangeSet, ColumnInfo, Editing, RowChange, TableStructure};

#[derive(Clone, Debug, PartialEq)]
pub struct PlannedStatement {
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// The `data_type` of the column each of `params` is stored in or compared with, in the same
    /// order: a driver that binds by the column's type (MySQL) needs it.
    pub param_types: Vec<String>,
    pub preview: String,
    pub describe: String,
    /// The key as `describe` shows it after its verb (`id = 1`); empty for an insert.
    pub key_label: String,
    pub expect_one_row: bool,
}

/// Builds the SQL text and its display-only preview side by side, so the two
/// can't drift apart. Values only ever go into `params`.
struct Builder<'a> {
    dialect: &'a dyn Dialect,
    /// The change's `original_types`: the cells' types before the edit, by column.
    types: &'a BTreeMap<String, CellType>,
    sql: String,
    preview: String,
    params: Vec<Option<String>>,
    param_types: Vec<String>,
}

/// The `original_types` of an insert: it has no original cells.
static NO_TYPES: BTreeMap<String, CellType> = BTreeMap::new();

impl<'a> Builder<'a> {
    fn new(dialect: &'a dyn Dialect, types: &'a BTreeMap<String, CellType>) -> Self {
        Builder {
            dialect,
            types,
            sql: String::new(),
            preview: String::new(),
            params: Vec::new(),
            param_types: Vec::new(),
        }
    }

    fn text(&mut self, t: &str) {
        self.sql.push_str(t);
        self.preview.push_str(t);
    }

    fn param(&mut self, value: Option<&str>, col: &ColumnInfo) {
        self.params.push(value.map(str::to_string));
        self.param_types.push(col.data_type.clone());
        let ph = self.dialect.placeholder(self.params.len());
        let original = self.types.get(&col.name).copied();
        self.sql
            .push_str(&self.dialect.typed_param(&ph, col, value, original));
        self.preview
            .push_str(&self.dialect.preview_param(col, value, original));
    }

    fn finish(mut self, describe: String, key_label: String, expect_one_row: bool) -> PlannedStatement {
        self.preview.push(';');
        PlannedStatement {
            sql: self.sql,
            params: self.params,
            param_types: self.param_types,
            preview: self.preview,
            describe,
            key_label,
            expect_one_row,
        }
    }

    fn where_key(&mut self, key: &[(&ColumnInfo, &String)]) {
        self.text(" WHERE ");
        for (i, (col, v)) in key.iter().enumerate() {
            if i > 0 {
                self.text(" AND ");
            }
            let q = self.dialect.quote_ident(&col.name);
            self.text(&q);
            self.text(" = ");
            self.param(Some(v), col);
        }
    }
}

pub(crate) fn column<'a>(structure: &'a TableStructure, name: &str) -> AppResult<&'a ColumnInfo> {
    structure
        .columns
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| AppError::Query(format!("Unknown column {name}")))
}

pub(crate) fn editable_values<'a>(
    structure: &'a TableStructure,
    values: &'a CellValues,
) -> AppResult<Vec<(&'a ColumnInfo, Option<&'a str>)>> {
    values
        .iter()
        .map(|(name, v)| {
            let c = column(structure, name)?;
            if c.read_only {
                return Err(AppError::Query(format!("{name} can't be edited")));
            }
            Ok((c, v.as_deref()))
        })
        .collect()
}

pub(crate) fn key_columns<'a>(
    structure: &'a TableStructure,
    key: &'a [String],
) -> AppResult<Vec<(&'a ColumnInfo, &'a String)>> {
    if key.len() != structure.primary_key.len() {
        return Err(AppError::Query(
            "Row key doesn't match the primary key".into(),
        ));
    }
    structure
        .primary_key
        .iter()
        .zip(key)
        .map(|(name, v)| Ok((column(structure, name)?, v)))
        .collect()
}

fn key_label(key: &[(&ColumnInfo, &String)]) -> String {
    let parts: Vec<String> = key
        .iter()
        .map(|(c, v)| format!("{} = {v}", c.name))
        .collect();
    parts.join(", ")
}

fn describe_key(verb: &str, key: &[(&ColumnInfo, &String)]) -> String {
    format!("{verb} {}", key_label(key))
}

/// A row whose key holds NULL: SQLite allows it outside an INTEGER PRIMARY KEY, but the key
/// can't name the row.
pub const EMPTY_KEY: &str = "Rows with an empty key can't be edited.";

/// The checks of a save that come before any value is read: the table can be edited, and no
/// row is named twice or by an empty key. Shared by every planner (SQL and MongoDB).
pub(crate) fn check_rows(structure: &TableStructure, set: &ChangeSet) -> AppResult<()> {
    if structure.editing != Editing::Allowed {
        return Err(AppError::Query("This table can't be edited.".into()));
    }
    // A row named twice (two updates, or an update and a delete) has no single meaning: refuse.
    // A row is its key's text and, where the page had cell types, its key cells' types: the
    // integer 5 and the text '5' are two rows. A key holding NULL names no row reliably.
    let mut seen = std::collections::HashSet::new();
    for change in &set.changes {
        if let RowChange::Update { key, original_types, .. } | RowChange::Delete { key, original_types } = change {
            let key_types: Vec<Option<CellType>> =
                structure.primary_key.iter().map(|k| original_types.get(k).copied()).collect();
            if key_types.contains(&Some(CellType::Null)) {
                return Err(AppError::Query(EMPTY_KEY.into()));
            }
            if !seen.insert((key.as_slice(), key_types)) {
                return Err(AppError::Query(
                    "The same row appears more than once in one save.".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Turns the UI's row changes into parameterized statements. Validates the
/// whole set first: any problem fails it and plans nothing.
pub fn plan_changes(
    dialect: &dyn Dialect,
    structure: &TableStructure,
    set: &ChangeSet,
) -> AppResult<Vec<PlannedStatement>> {
    check_rows(structure, set)?;
    let table = format!(
        "{}.{}",
        dialect.quote_ident(&set.table.schema),
        dialect.quote_ident(&set.table.name)
    );
    let mut out = Vec::new();
    for change in &set.changes {
        match change {
            RowChange::Update { key, values, original_types } => {
                let key = key_columns(structure, key)?;
                let cols = editable_values(structure, values)?;
                if cols.is_empty() {
                    continue;
                }
                let mut b = Builder::new(dialect, original_types);
                b.text(&format!("UPDATE {table} SET "));
                for (i, (c, v)) in cols.iter().enumerate() {
                    if i > 0 {
                        b.text(", ");
                    }
                    b.text(&format!("{} = ", dialect.quote_ident(&c.name)));
                    b.param(*v, c);
                }
                b.where_key(&key);
                out.push(b.finish(describe_key("UPDATE", &key), key_label(&key), true));
            }
            RowChange::Insert { values } => {
                let cols = editable_values(structure, values)?;
                let mut b = Builder::new(dialect, &NO_TYPES);
                if cols.is_empty() {
                    b.text(&dialect.insert_defaults(&table));
                } else {
                    let names: Vec<String> = cols
                        .iter()
                        .map(|(c, _)| dialect.quote_ident(&c.name))
                        .collect();
                    b.text(&format!(
                        "INSERT INTO {table} ({}) VALUES (",
                        names.join(", ")
                    ));
                    for (i, (c, v)) in cols.iter().enumerate() {
                        if i > 0 {
                            b.text(", ");
                        }
                        b.param(*v, c);
                    }
                    b.text(")");
                }
                out.push(b.finish("INSERT".into(), String::new(), false));
            }
            RowChange::Delete { key, original_types } => {
                let key = key_columns(structure, key)?;
                let mut b = Builder::new(dialect, original_types);
                b.text(&format!("DELETE FROM {table}"));
                b.where_key(&key);
                out.push(b.finish(describe_key("DELETE", &key), key_label(&key), true));
            }
        }
    }
    Ok(out)
}

/// The failure of an UPDATE or DELETE that didn't hit exactly one row. `key_label` is the
/// statement's (`id = 1`); the message names it.
pub fn row_changed_error(key_label: &str) -> AppError {
    AppError::Query(format!("The row with {key_label} was changed or deleted by someone else."))
}

/// Display-only rendering of a value for the SQL preview. Never executed.
pub fn render_literal(value: Option<&str>, data_type: &str) -> String {
    let Some(v) = value else {
        return "NULL".into();
    };
    let bare = match data_type {
        "int2" | "int4" | "int8" | "numeric" | "float4" | "float8" | "oid" => is_number(v),
        "bool" => v == "true" || v == "false",
        _ => false,
    };
    if bare {
        v.to_string()
    } else {
        format!("'{}'", v.replace('\'', "''"))
    }
}

/// Plain decimal numbers only: `NaN`/`inf` parse as floats but aren't SQL literals.
pub(crate) fn is_number(v: &str) -> bool {
    let b = v.strip_prefix(['-', '+']).unwrap_or(v);
    let (mantissa, exp) = match b.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (b, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((i, f)) => {
            (digits(i) || i.is_empty())
                && (digits(f) || f.is_empty())
                && !(i.is_empty() && f.is_empty())
        }
        None => digits(mantissa),
    };
    let exp_ok = exp.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)));
    mantissa_ok && exp_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::PostgresDialect;
    use crate::error::AppError;
    use crate::model::*;

    fn col(name: &str, data_type: &str, cast: &str, pk: bool, ro: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            full_type: cast.into(),
            cast_type: cast.into(),
            nullable: true,
            default: None,
            is_primary_key: pk,
            read_only: ro,
        }
    }

    fn structure(columns: Vec<ColumnInfo>, pk: &[&str]) -> TableStructure {
        TableStructure {
            columns,
            primary_key: pk.iter().map(|s| s.to_string()).collect(),
            relation: RelationKind::Table,
            editing: Editing::Allowed,
            indexes: vec![],
            foreign_keys: vec![],
        }
    }

    fn users() -> TableStructure {
        structure(
            vec![
                col("id", "int4", "integer", true, false),
                col("plan", "text", "text", false, false),
                col("seats", "int4", "integer", false, false),
                col("gen", "text", "text", false, true),
            ],
            &["id"],
        )
    }

    fn pair() -> TableStructure {
        structure(
            vec![
                col("Key A", "text", "text", true, false),
                col("select", "int4", "integer", true, false),
                col("val", "text", "text", false, false),
            ],
            &["select", "Key A"],
        )
    }

    fn tref(name: &str) -> TableRef {
        TableRef {
            schema: "fx".into(),
            name: name.into(),
        }
    }

    fn vals(pairs: &[(&str, Option<&str>)]) -> CellValues {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
            .collect()
    }

    fn keys(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    fn plan(
        s: &TableStructure,
        table: &str,
        changes: Vec<RowChange>,
    ) -> AppResult<Vec<PlannedStatement>> {
        plan_changes(
            &PostgresDialect,
            s,
            &ChangeSet {
                table: tref(table),
                changes,
            },
        )
    }

    fn some(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn update_sets_only_changed_columns_with_cast_type_casts() {
        let out = plan(
            &users(),
            "users",
            vec![RowChange::Update {
                key: keys(&["1151"]),
                values: vals(&[("plan", Some("pro")), ("seats", Some("5"))]),
                original_types: BTreeMap::new(),
            }],
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        let st = &out[0];
        assert_eq!(
            st.sql,
            r#"UPDATE "fx"."users" SET "plan" = $1::text, "seats" = $2::integer WHERE "id" = $3::integer"#
        );
        assert_eq!(st.params, vec![some("pro"), some("5"), some("1151")]);
        assert_eq!(
            st.preview,
            r#"UPDATE "fx"."users" SET "plan" = 'pro', "seats" = 5 WHERE "id" = 1151;"#
        );
        assert_eq!(st.describe, "UPDATE id = 1151");
        assert!(st.expect_one_row);
    }

    #[test]
    fn update_matches_original_key() {
        let out = plan(
            &users(),
            "users",
            vec![RowChange::Update {
                key: keys(&["7"]),
                values: vals(&[("id", Some("8"))]),
                original_types: BTreeMap::new(),
            }],
        )
        .unwrap();
        assert_eq!(
            out[0].sql,
            r#"UPDATE "fx"."users" SET "id" = $1::integer WHERE "id" = $2::integer"#
        );
        assert_eq!(out[0].params, vec![some("8"), some("7")]);
    }

    #[test]
    fn composite_key_and_awkward_identifiers_are_quoted() {
        let out = plan(
            &pair(),
            "pair",
            vec![RowChange::Update {
                key: keys(&["1", "it's"]),
                values: vals(&[("val", Some("v"))]),
                original_types: BTreeMap::new(),
            }],
        )
        .unwrap();
        assert_eq!(
            out[0].sql,
            r#"UPDATE "fx"."pair" SET "val" = $1::text WHERE "select" = $2::integer AND "Key A" = $3::text"#
        );
        assert_eq!(out[0].params, vec![some("v"), some("1"), some("it's")]);
        assert_eq!(
            out[0].preview,
            r#"UPDATE "fx"."pair" SET "val" = 'v' WHERE "select" = 1 AND "Key A" = 'it''s';"#
        );
    }

    #[test]
    fn row_changed_error_uses_the_key_label() {
        assert_eq!(
            row_changed_error("name = a b"),
            AppError::Query("The row with name = a b was changed or deleted by someone else.".into())
        );
    }

    #[test]
    fn row_changed_names_the_key() {
        assert_eq!(
            row_changed_error("id = 1"),
            AppError::Query("The row with id = 1 was changed or deleted by someone else.".into())
        );
        assert_eq!(
            row_changed_error("a = 1, b = x y"),
            AppError::Query("The row with a = 1, b = x y was changed or deleted by someone else.".into())
        );
        assert_eq!(
            row_changed_error("DELETE"),
            AppError::Query("The row with DELETE was changed or deleted by someone else.".into())
        );
    }

    #[test]
    fn insert_with_values_and_default_values() {
        let out = plan(
            &users(),
            "users",
            vec![
                RowChange::Insert {
                    values: vals(&[("id", Some("9")), ("plan", None)]),
                },
                RowChange::Insert {
                    values: CellValues::new(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            out[0].sql,
            r#"INSERT INTO "fx"."users" ("id", "plan") VALUES ($1::integer, $2::text)"#
        );
        assert_eq!(out[0].params, vec![some("9"), None]);
        assert_eq!(
            out[0].preview,
            r#"INSERT INTO "fx"."users" ("id", "plan") VALUES (9, NULL);"#
        );
        assert_eq!(out[0].describe, "INSERT");
        assert!(!out[0].expect_one_row);
        assert_eq!(out[1].sql, r#"INSERT INTO "fx"."users" DEFAULT VALUES"#);
        assert!(out[1].params.is_empty());
        assert_eq!(
            out[1].preview,
            r#"INSERT INTO "fx"."users" DEFAULT VALUES;"#
        );
    }

    #[test]
    fn delete_by_key() {
        let out = plan(
            &users(),
            "users",
            vec![RowChange::Delete {
                key: keys(&["1299"]),
                original_types: BTreeMap::new(),
            }],
        )
        .unwrap();
        assert_eq!(
            out[0].sql,
            r#"DELETE FROM "fx"."users" WHERE "id" = $1::integer"#
        );
        assert_eq!(out[0].params, vec![some("1299")]);
        assert_eq!(
            out[0].preview,
            r#"DELETE FROM "fx"."users" WHERE "id" = 1299;"#
        );
        assert_eq!(out[0].describe, "DELETE id = 1299");
        assert!(out[0].expect_one_row);
    }

    #[test]
    fn null_value_binds_none() {
        let out = plan(
            &users(),
            "users",
            vec![RowChange::Update {
                key: keys(&["1"]),
                values: vals(&[("plan", None)]),
                original_types: BTreeMap::new(),
            }],
        )
        .unwrap();
        assert_eq!(out[0].params, vec![None, some("1")]);
        assert_eq!(
            out[0].preview,
            r#"UPDATE "fx"."users" SET "plan" = NULL WHERE "id" = 1;"#
        );
    }

    #[test]
    fn hostile_value_is_bound_not_inlined() {
        let evil = "x'); drop table fx.users; --";
        let out = plan(
            &users(),
            "users",
            vec![RowChange::Update {
                key: keys(&["1"]),
                values: vals(&[("plan", Some(evil))]),
                original_types: BTreeMap::new(),
            }],
        )
        .unwrap();
        assert!(!out[0].sql.contains("drop table"));
        assert_eq!(out[0].params[0], some(evil));
        assert!(out[0].preview.contains("'x''); drop table fx.users; --'"));
    }

    #[test]
    fn render_literal_rules() {
        assert_eq!(render_literal(Some("42"), "int4"), "42");
        assert_eq!(render_literal(Some("-1.5"), "numeric"), "-1.5");
        assert_eq!(render_literal(Some("1e3"), "float8"), "1e3");
        assert_eq!(render_literal(Some("abc"), "int4"), "'abc'");
        assert_eq!(render_literal(Some(""), "int4"), "''");
        assert_eq!(render_literal(Some("NaN"), "float8"), "'NaN'");
        assert_eq!(render_literal(Some("7"), "text"), "'7'");
        assert_eq!(render_literal(Some("true"), "bool"), "true");
        assert_eq!(render_literal(Some("yes"), "bool"), "'yes'");
        assert_eq!(render_literal(None, "text"), "NULL");
        assert_eq!(render_literal(Some("it's"), "text"), "'it''s'");
    }

    #[test]
    fn rejects_read_only_column_unknown_column_bad_key_and_not_editable() {
        let msg = |r: AppResult<Vec<PlannedStatement>>| match r {
            Err(AppError::Query(m)) => m,
            other => panic!("expected Query error, got {other:?}"),
        };
        let upd = |c: &str| RowChange::Update {
            key: keys(&["1"]),
            values: vals(&[(c, Some("x"))]),
            original_types: BTreeMap::new(),
        };
        assert_eq!(
            msg(plan(&users(), "users", vec![upd("gen")])),
            "gen can't be edited"
        );
        assert_eq!(
            msg(plan(
                &users(),
                "users",
                vec![RowChange::Insert {
                    values: vals(&[("gen", None)])
                }]
            )),
            "gen can't be edited"
        );
        assert_eq!(
            msg(plan(&users(), "users", vec![upd("nope")])),
            "Unknown column nope"
        );
        assert_eq!(
            msg(plan(
                &pair(),
                "pair",
                vec![RowChange::Delete { key: keys(&["1"]), original_types: BTreeMap::new() }]
            )),
            "Row key doesn't match the primary key"
        );
        // One bad change fails the whole set.
        assert!(
            plan(
                &users(),
                "users",
                vec![RowChange::Delete { key: keys(&["1"]), original_types: BTreeMap::new() }, upd("nope")]
            )
            .is_err()
        );
        let mut ro = users();
        ro.editing = Editing::View;
        assert_eq!(
            msg(plan(
                &ro,
                "users",
                vec![RowChange::Delete { key: keys(&["1"]), original_types: BTreeMap::new() }]
            )),
            "This table can't be edited."
        );
        // An empty update is skipped, not an error.
        let out = plan(
            &users(),
            "users",
            vec![RowChange::Update {
                key: keys(&["1"]),
                values: CellValues::new(),
                original_types: BTreeMap::new(),
            }],
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn rejects_the_same_row_twice() {
        let dup = "The same row appears more than once in one save.";
        let msg = |r: AppResult<Vec<PlannedStatement>>| match r {
            Err(AppError::Query(m)) => m,
            other => panic!("expected Query error, got {other:?}"),
        };
        let upd = |k: &str, c: &str| RowChange::Update {
            key: keys(&[k]),
            values: vals(&[(c, Some("x"))]),
            original_types: BTreeMap::new(),
        };
        let del = |k: &str| RowChange::Delete { key: keys(&[k]), original_types: BTreeMap::new() };
        assert_eq!(msg(plan(&users(), "users", vec![upd("1", "plan"), upd("1", "seats")])), dup);
        assert_eq!(msg(plan(&users(), "users", vec![del("1"), upd("1", "plan")])), dup);
        assert_eq!(msg(plan(&users(), "users", vec![del("1"), del("1")])), dup);
        // Composite keys compare whole; inserts don't count; an empty text key is a real key.
        let pair_del = |a: &str, b: &str| RowChange::Delete { key: keys(&[a, b]), original_types: BTreeMap::new() };
        assert_eq!(plan(&pair(), "pair", vec![pair_del("1", "a"), pair_del("1", "b")]).unwrap().len(), 2);
        assert_eq!(msg(plan(&pair(), "pair", vec![pair_del("1", ""), pair_del("1", "")])), dup);
        assert_eq!(plan(&pair(), "pair", vec![pair_del("1", "")]).unwrap().len(), 1);
        let ins = || RowChange::Insert { values: vals(&[("id", Some("1"))]) };
        assert_eq!(plan(&users(), "users", vec![del("1"), ins(), ins()]).unwrap().len(), 3);
    }

    /// `loose`: the integer 5 and the text '5' have the same key text but are two rows.
    #[test]
    fn the_same_key_text_with_other_types_is_another_row() {
        let typed = |t: CellType| -> BTreeMap<String, CellType> { [("id".to_string(), t)].into() };
        let del = |t| RowChange::Delete { key: keys(&["5"]), original_types: typed(t) };
        let upd = |t| RowChange::Update { key: keys(&["5"]), values: vals(&[("plan", Some("x"))]), original_types: typed(t) };
        assert_eq!(plan(&users(), "users", vec![del(CellType::Integer), del(CellType::Text)]).unwrap().len(), 2);
        assert_eq!(plan(&users(), "users", vec![upd(CellType::Integer), upd(CellType::Text)]).unwrap().len(), 2);
        // The same types: the same row.
        let dup = Err(AppError::Query("The same row appears more than once in one save.".into()));
        assert_eq!(plan(&users(), "users", vec![del(CellType::Text), upd(CellType::Text)]), dup);
        // Only the key columns' types count: an edited column's type doesn't make another row.
        let with_plan = |t| {
            let mut types = typed(CellType::Integer);
            types.insert("plan".into(), t);
            RowChange::Update { key: keys(&["5"]), values: vals(&[("plan", Some("x"))]), original_types: types }
        };
        assert_eq!(plan(&users(), "users", vec![with_plan(CellType::Text), with_plan(CellType::Null)]), dup);
    }

    #[test]
    fn a_key_holding_null_is_refused() {
        let empty = Err(AppError::Query("Rows with an empty key can't be edited.".into()));
        let null_b: BTreeMap<String, CellType> =
            [("select".to_string(), CellType::Integer), ("Key A".to_string(), CellType::Null)].into();
        let del = RowChange::Delete { key: keys(&["1", ""]), original_types: null_b.clone() };
        assert_eq!(plan(&pair(), "pair", vec![del]), empty);
        let upd = RowChange::Update { key: keys(&["1", ""]), values: vals(&[("val", Some("v"))]), original_types: null_b };
        assert_eq!(plan(&pair(), "pair", vec![RowChange::Insert { values: vals(&[("val", Some("n"))]) }, upd]), empty);
        // A NULL in an edited column that isn't the key is fine.
        let types: BTreeMap<String, CellType> = [("id".to_string(), CellType::Integer), ("plan".to_string(), CellType::Null)].into();
        let upd = RowChange::Update { key: keys(&["1"]), values: vals(&[("plan", Some("x"))]), original_types: types };
        assert_eq!(plan(&users(), "users", vec![upd]).unwrap().len(), 1);
    }

    #[test]
    fn describe_formats_composite_key() {
        let out = plan(
            &pair(),
            "pair",
            vec![RowChange::Update {
                key: keys(&["1", "it's"]),
                values: vals(&[("val", Some("v"))]),
                original_types: BTreeMap::new(),
            }],
        )
        .unwrap();
        assert_eq!(out[0].describe, "UPDATE select = 1, Key A = it's");
    }

    /// Shows what `typed_param` was given for each parameter.
    struct TagDialect;

    impl Dialect for TagDialect {
        fn quote_ident(&self, s: &str) -> String {
            PostgresDialect.quote_ident(s)
        }
        fn placeholder(&self, n: usize) -> String {
            PostgresDialect.placeholder(n)
        }
        fn cast_param(&self, placeholder: &str, cast_type: &str) -> String {
            PostgresDialect.cast_param(placeholder, cast_type)
        }
        fn text_expr(&self, quoted_col: &str) -> String {
            PostgresDialect.text_expr(quoted_col)
        }
        fn typed_param(&self, ph: &str, col: &ColumnInfo, value: Option<&str>, original: Option<CellType>) -> String {
            format!("{ph}<{}:{}:{}>", col.name, value.unwrap_or("NULL"), original.map_or('-', CellType::code))
        }
    }

    #[test]
    fn params_go_through_typed_param_with_the_original_types() {
        let types: BTreeMap<String, CellType> =
            [("id".to_string(), CellType::Integer), ("plan".to_string(), CellType::Text)].into();
        let set = ChangeSet {
            table: tref("users"),
            changes: vec![
                RowChange::Delete { key: keys(&["2"]), original_types: [("id".to_string(), CellType::Text)].into() },
                RowChange::Update {
                    key: keys(&["1"]),
                    values: vals(&[("plan", Some("pro")), ("seats", None)]),
                    original_types: types,
                },
                RowChange::Insert { values: vals(&[("plan", Some("x"))]) },
            ],
        };
        let out = plan_changes(&TagDialect, &users(), &set).unwrap();
        let sql: Vec<&str> = out.iter().map(|s| s.sql.as_str()).collect();
        assert_eq!(
            sql,
            vec![
                r#"DELETE FROM "fx"."users" WHERE "id" = $1<id:2:t>"#,
                r#"UPDATE "fx"."users" SET "plan" = $1<plan:pro:t>, "seats" = $2<seats:NULL:-> WHERE "id" = $3<id:1:i>"#,
                r#"INSERT INTO "fx"."users" ("plan") VALUES ($1<plan:x:->)"#,
            ]
        );
        // The preview doesn't depend on the dialect's casts.
        assert_eq!(out[1].preview, r#"UPDATE "fx"."users" SET "plan" = 'pro', "seats" = NULL WHERE "id" = 1;"#);
    }

    #[test]
    fn each_param_carries_its_columns_type() {
        let set = ChangeSet {
            table: tref("users"),
            changes: vec![RowChange::Update {
                key: keys(&["1"]),
                values: vals(&[("plan", Some("pro")), ("seats", None)]),
                original_types: BTreeMap::new(),
            }],
        };
        let out = plan_changes(&TagDialect, &users(), &set).unwrap();
        let by_name = |n: &str| users().columns.iter().find(|c| c.name == n).unwrap().data_type.clone();
        assert_eq!(out[0].param_types, vec![by_name("plan"), by_name("seats"), by_name("id")]);
        assert_eq!(out[0].param_types.len(), out[0].params.len());
    }
}
