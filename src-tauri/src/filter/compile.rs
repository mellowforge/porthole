use super::{CmpOp, Expr, Literal};
use crate::driver::Dialect;
use crate::error::{AppError, AppResult};
use crate::model::ColumnInfo;

/// Compiles a filter expression into a SQL fragment (without `WHERE`) and
/// its text parameters, numbered from `first_param`.
pub fn compile(
    expr: &Expr,
    columns: &[ColumnInfo],
    dialect: &dyn Dialect,
    first_param: usize,
) -> AppResult<(String, Vec<String>)> {
    let (sql, params, _) = compile_typed(expr, columns, dialect, first_param)?;
    Ok((sql, params))
}

/// As `compile`, with the `data_type` of the column each parameter is compared with, in the
/// same order: a driver that binds by the column's type (MySQL) needs it.
pub fn compile_typed(
    expr: &Expr,
    columns: &[ColumnInfo],
    dialect: &dyn Dialect,
    first_param: usize,
) -> AppResult<(String, Vec<String>, Vec<String>)> {
    let mut c = Compiler { columns, dialect, next: first_param, params: Vec::new(), types: Vec::new() };
    let sql = c.expr(expr)?;
    Ok((sql, c.params, c.types))
}

struct Compiler<'a> {
    columns: &'a [ColumnInfo],
    dialect: &'a dyn Dialect,
    next: usize,
    params: Vec<String>,
    /// The `data_type` of each parameter's column.
    types: Vec<String>,
}

impl<'a> Compiler<'a> {
    fn expr(&mut self, e: &Expr) -> AppResult<String> {
        Ok(match e {
            Expr::And(a, b) => format!("({} AND {})", self.expr(a)?, self.expr(b)?),
            Expr::Or(a, b) => format!("({} OR {})", self.expr(a)?, self.expr(b)?),
            Expr::Cmp { column, column_pos, op, value } => {
                let col = self.resolve(column, *column_pos)?;
                let quoted = self.dialect.quote_ident(&col.name);
                if *op == CmpOp::Like {
                    let ph = self.bind(value, col);
                    format!("{} LIKE {}", self.dialect.text_expr(&quoted), ph)
                } else {
                    let ph = self.bind_cast(value, col);
                    format!("{quoted} {} {ph}", op_sql(*op))
                }
            }
            Expr::In { column, column_pos, values } => {
                let col = self.resolve(column, *column_pos)?;
                let quoted = self.dialect.quote_ident(&col.name);
                let items: Vec<String> = values.iter().map(|v| self.bind_cast(v, col)).collect();
                format!("{quoted} IN ({})", items.join(", "))
            }
            Expr::IsNull { column, column_pos, negated } => {
                let col = self.resolve(column, *column_pos)?;
                let quoted = self.dialect.quote_ident(&col.name);
                format!("{quoted} IS {}NULL", if *negated { "NOT " } else { "" })
            }
        })
    }

    /// Exact (case-sensitive) match first, then a unique case-insensitive match.
    fn resolve(&self, name: &str, pos: usize) -> AppResult<&'a ColumnInfo> {
        let columns = self.columns;
        if let Some(c) = columns.iter().find(|c| c.name == name) {
            return Ok(c);
        }
        let mut loose = columns.iter().filter(|c| c.name.eq_ignore_ascii_case(name));
        match (loose.next(), loose.next()) {
            (Some(c), None) => Ok(c),
            _ => Err(AppError::Filter { message: format!("Unknown column \"{name}\""), position: pos }),
        }
    }

    fn bind(&mut self, v: &Literal, col: &ColumnInfo) -> String {
        let (Literal::Str(s) | Literal::Num(s)) = v;
        self.params.push(s.clone());
        self.types.push(col.data_type.clone());
        let ph = self.dialect.placeholder(self.next);
        self.next += 1;
        ph
    }

    fn bind_cast(&mut self, v: &Literal, col: &ColumnInfo) -> String {
        let ph = self.bind(v, col);
        self.dialect.literal_param(&ph, col, matches!(v, Literal::Num(_)))
    }
}

fn op_sql(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "!=",
        CmpOp::Gt => ">",
        CmpOp::Lt => "<",
        CmpOp::Ge => ">=",
        CmpOp::Le => "<=",
        CmpOp::Like => "LIKE",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::PostgresDialect;
    use crate::filter::parse;

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            full_type: ty.into(),
            cast_type: ty.into(),
            nullable: true,
            default: None,
            is_primary_key: false,
            read_only: false,
        }
    }

    fn cols() -> Vec<ColumnInfo> {
        vec![col("status", "varchar"), col("id", "int4"), col("Name", "text")]
    }

    fn run(input: &str, first: usize) -> AppResult<(String, Vec<String>)> {
        compile(&parse(input)?, &cols(), &PostgresDialect, first)
    }

    #[test]
    fn compiles_params_with_casts() {
        let (sql, params) = run("status = \"active\" and id > 5", 1).unwrap();
        assert_eq!(sql, "(\"status\" = $1::varchar AND \"id\" > $2::int4)");
        assert_eq!(params, vec!["active", "5"]);
        let (sql, _) = run("status = \"active\" and id > 5", 3).unwrap();
        assert_eq!(sql, "(\"status\" = $3::varchar AND \"id\" > $4::int4)");
    }

    #[test]
    fn casts_use_cast_type_without_typmod() {
        let mut n = col("n", "numeric");
        n.full_type = "numeric(5,2)".into();
        let (sql, _) = compile(&parse("n = 1.499").unwrap(), &[n], &PostgresDialect, 1).unwrap();
        assert!(sql.contains("::numeric") && !sql.contains("(5,2)"), "{sql}");
    }

    #[test]
    fn compile_typed_gives_each_params_column_type() {
        let (sql, params, types) =
            compile_typed(&parse("status = \"a\" and id in (1, 2) or Name like \"%x%\"").unwrap(), &cols(), &PostgresDialect, 1)
                .unwrap();
        assert_eq!((sql, params), run("status = \"a\" and id in (1, 2) or Name like \"%x%\"", 1).unwrap());
        assert_eq!(types, vec!["varchar", "int4", "int4", "text"]);
    }

    #[test]
    fn like_uses_text_cast() {
        let (sql, params) = run("name like \"%ann%\"", 1).unwrap();
        assert_eq!(sql, "\"Name\"::text LIKE $1");
        assert_eq!(params, vec!["%ann%"]);
    }

    #[test]
    fn injection_is_just_a_param() {
        let (sql, params) = run("status = \"x'; drop table users; --\"", 1).unwrap();
        assert!(!sql.to_lowercase().contains("drop"));
        assert_eq!(params, vec!["x'; drop table users; --"]);
    }

    #[test]
    fn unknown_column_errors() {
        assert_eq!(
            run("nope = 1", 1).unwrap_err(),
            AppError::Filter { message: "Unknown column \"nope\"".into(), position: 0 }
        );
    }

    #[test]
    fn unknown_column_position_points_at_token() {
        assert_eq!(
            run("status = 1 and nope = 2", 1).unwrap_err(),
            AppError::Filter { message: "Unknown column \"nope\"".into(), position: 15 }
        );
    }

    /// Shows what `literal_param` was given for each literal.
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
        fn literal_param(&self, ph: &str, col: &ColumnInfo, numeric: bool) -> String {
            format!("{ph}<{}:{numeric}>", col.name)
        }
    }

    #[test]
    fn literals_go_through_literal_param() {
        let expr = parse("id = 5 and status in (\"a\", 7) and name like \"x%\"").unwrap();
        let (sql, params) = compile(&expr, &cols(), &TagDialect, 1).unwrap();
        assert_eq!(
            sql,
            "((\"id\" = $1<id:true> AND \"status\" IN ($2<status:false>, $3<status:true>)) AND \"Name\"::text LIKE $4)"
        );
        assert_eq!(params, vec!["5", "a", "7", "x%"]);
    }
}
