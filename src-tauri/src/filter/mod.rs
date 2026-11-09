//! Quick filter: a small expression language parsed into an [`Expr`] and
//! compiled to a parameterized SQL fragment (see [`compile`]).

mod compile;

pub use compile::{compile, compile_typed};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Num(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    Like,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp { column: String, column_pos: usize, op: CmpOp, value: Literal },
    In { column: String, column_pos: usize, values: Vec<Literal> },
    IsNull { column: String, column_pos: usize, negated: bool },
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// Bare identifier (may be a keyword).
    Word(String),
    /// Backtick-quoted identifier (never a keyword).
    Quoted(String),
    Str(String),
    Num(String),
    Op(CmpOp),
    LParen,
    RParen,
    Comma,
    Eof,
}

struct Token {
    tok: Tok,
    pos: usize,
}

fn err<T>(message: impl Into<String>, position: usize) -> AppResult<T> {
    Err(AppError::Filter { message: message.into(), position })
}

fn lex(input: &str) -> AppResult<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let tok = match c {
            '(' => { i += 1; Tok::LParen }
            ')' => { i += 1; Tok::RParen }
            ',' => { i += 1; Tok::Comma }
            '=' => { i += 1; Tok::Op(CmpOp::Eq) }
            '!' if chars.get(i + 1) == Some(&'=') => { i += 2; Tok::Op(CmpOp::Ne) }
            '<' if chars.get(i + 1) == Some(&'>') => { i += 2; Tok::Op(CmpOp::Ne) }
            '<' if chars.get(i + 1) == Some(&'=') => { i += 2; Tok::Op(CmpOp::Le) }
            '<' => { i += 1; Tok::Op(CmpOp::Lt) }
            '>' if chars.get(i + 1) == Some(&'=') => { i += 2; Tok::Op(CmpOp::Ge) }
            '>' => { i += 1; Tok::Op(CmpOp::Gt) }
            '"' | '\'' => {
                i += 1;
                let mut s = String::new();
                loop {
                    match chars.get(i) {
                        None => return err("Unclosed string", start),
                        Some(&q) if q == c => {
                            if chars.get(i + 1) == Some(&c) {
                                s.push(c);
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                Tok::Str(s)
            }
            '`' => {
                i += 1;
                let mut s = String::new();
                loop {
                    match chars.get(i) {
                        None => return err("Unclosed backtick name", start),
                        Some('`') => {
                            i += 1;
                            break;
                        }
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                if s.is_empty() {
                    return err("Empty column name", start);
                }
                Tok::Quoted(s)
            }
            c if c.is_ascii_digit() || (c == '-' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) => {
                i += 1;
                while chars.get(i).is_some_and(|d| d.is_ascii_digit()) {
                    i += 1;
                }
                if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                    i += 1;
                    while chars.get(i).is_some_and(|d| d.is_ascii_digit()) {
                        i += 1;
                    }
                }
                Tok::Num(chars[start..i].iter().collect())
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                i += 1;
                while chars.get(i).is_some_and(|d| d.is_ascii_alphanumeric() || *d == '_' || *d == '$') {
                    i += 1;
                }
                Tok::Word(chars[start..i].iter().collect())
            }
            other => return err(format!("Unexpected character '{other}'"), start),
        };
        out.push(Token { tok, pos: start });
    }
    out.push(Token { tok: Tok::Eof, pos: chars.len() });
    Ok(out)
}

const KEYWORDS: [&str; 7] = ["and", "or", "in", "is", "not", "null", "like"];

/// Maximum nesting of parentheses; deeper input is rejected before it can
/// exhaust the stack.
const MAX_DEPTH: usize = 64;

struct Parser {
    toks: Vec<Token>,
    idx: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.toks[self.idx]
    }

    fn bump(&mut self) -> &Token {
        let t = &self.toks[self.idx];
        if self.idx + 1 < self.toks.len() {
            self.idx += 1;
        }
        t
    }

    fn peek_kw(&self, kw: &str) -> bool {
        matches!(&self.peek().tok, Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        let hit = self.peek_kw(kw);
        if hit {
            self.bump();
        }
        hit
    }

    fn expect_kw(&mut self, kw: &str) -> AppResult<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            err(format!("Expected \"{kw}\""), self.peek().pos)
        }
    }

    fn or(&mut self) -> AppResult<Expr> {
        let mut left = self.and()?;
        while self.eat_kw("or") {
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> AppResult<Expr> {
        let mut left = self.unary()?;
        while self.eat_kw("and") {
            left = Expr::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> AppResult<Expr> {
        if self.peek().tok == Tok::LParen {
            if self.depth == MAX_DEPTH {
                return err("Too deeply nested", self.peek().pos);
            }
            self.bump();
            self.depth += 1;
            let e = self.or()?;
            self.depth -= 1;
            if self.peek().tok != Tok::RParen {
                return err("Expected \")\"", self.peek().pos);
            }
            self.bump();
            return Ok(e);
        }
        self.cond()
    }

    fn ident(&mut self) -> AppResult<(String, usize)> {
        let t = self.peek();
        let pos = t.pos;
        match &t.tok {
            Tok::Quoted(s) => {
                let s = s.clone();
                self.bump();
                Ok((s, pos))
            }
            Tok::Word(w) if KEYWORDS.iter().any(|k| w.eq_ignore_ascii_case(k)) => {
                err(format!("\"{w}\" is a keyword; quote the column name with backticks"), t.pos)
            }
            Tok::Word(w) => {
                let w = w.clone();
                self.bump();
                Ok((w, pos))
            }
            _ => err("Expected a column name", t.pos),
        }
    }

    fn value(&mut self) -> AppResult<Literal> {
        let t = self.peek();
        let v = match &t.tok {
            Tok::Str(s) => Literal::Str(s.clone()),
            Tok::Num(n) => Literal::Num(n.clone()),
            _ => return err("Expected a value (a quoted string or a number)", t.pos),
        };
        self.bump();
        Ok(v)
    }

    fn cond(&mut self) -> AppResult<Expr> {
        let (column, column_pos) = self.ident()?;
        if self.eat_kw("is") {
            let negated = self.eat_kw("not");
            self.expect_kw("null")?;
            return Ok(Expr::IsNull { column, column_pos, negated });
        }
        if self.eat_kw("in") {
            if self.peek().tok != Tok::LParen {
                return err("Expected \"(\"", self.peek().pos);
            }
            self.bump();
            let mut values = vec![self.value()?];
            while self.peek().tok == Tok::Comma {
                self.bump();
                values.push(self.value()?);
            }
            if self.peek().tok != Tok::RParen {
                return err("Expected \",\" or \")\"", self.peek().pos);
            }
            self.bump();
            return Ok(Expr::In { column, column_pos, values });
        }
        let op = if self.eat_kw("like") {
            CmpOp::Like
        } else if let Tok::Op(op) = self.peek().tok {
            self.bump();
            op
        } else {
            return err("Expected an operator (=, !=, >, <, like, in, is null)", self.peek().pos);
        };
        let value = self.value()?;
        Ok(Expr::Cmp { column, column_pos, op, value })
    }
}

pub fn parse(input: &str) -> AppResult<Expr> {
    let mut p = Parser { toks: lex(input)?, idx: 0, depth: 0 };
    let e = p.or()?;
    match &p.peek().tok {
        Tok::Eof => Ok(e),
        _ => err("Expected \"and\", \"or\" or end of filter", p.peek().pos),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(c: &str, pos: usize, op: CmpOp, v: Literal) -> Expr {
        Expr::Cmp { column: c.into(), column_pos: pos, op, value: v }
    }
    fn s(v: &str) -> Literal {
        Literal::Str(v.into())
    }
    fn n(v: &str) -> Literal {
        Literal::Num(v.into())
    }
    fn b(e: Expr) -> Box<Expr> {
        Box::new(e)
    }

    #[test]
    fn parses_spec_example() {
        assert_eq!(
            parse("status = \"active\" and name like \"%ann%\"").unwrap(),
            Expr::And(
                b(cmp("status", 0, CmpOp::Eq, s("active"))),
                b(cmp("name", 22, CmpOp::Like, s("%ann%")))
            )
        );
    }

    #[test]
    fn or_binds_looser_than_and() {
        assert_eq!(
            parse("a = 1 or b = 2 and c = 3").unwrap(),
            Expr::Or(
                b(cmp("a", 0, CmpOp::Eq, n("1"))),
                b(Expr::And(b(cmp("b", 9, CmpOp::Eq, n("2"))), b(cmp("c", 19, CmpOp::Eq, n("3")))))
            )
        );
    }

    #[test]
    fn parentheses_override() {
        assert_eq!(
            parse("(a = 1 or b = 2) and c = 3").unwrap(),
            Expr::And(
                b(Expr::Or(b(cmp("a", 1, CmpOp::Eq, n("1"))), b(cmp("b", 10, CmpOp::Eq, n("2"))))),
                b(cmp("c", 21, CmpOp::Eq, n("3")))
            )
        );
    }

    #[test]
    fn in_list() {
        assert_eq!(
            parse("plan in (\"pro\", 'team')").unwrap(),
            Expr::In { column: "plan".into(), column_pos: 0, values: vec![s("pro"), s("team")] }
        );
    }

    #[test]
    fn is_not_null() {
        assert_eq!(
            parse("plan IS NOT NULL").unwrap(),
            Expr::IsNull { column: "plan".into(), column_pos: 0, negated: true }
        );
    }

    #[test]
    fn single_quotes_and_escape() {
        assert_eq!(parse("name = 'O''Brien'").unwrap(), cmp("name", 0, CmpOp::Eq, s("O'Brien")));
    }

    #[test]
    fn backtick_ident() {
        assert_eq!(parse("`Order Id` >= 10.5").unwrap(), cmp("Order Id", 0, CmpOp::Ge, n("10.5")));
    }

    #[test]
    fn error_position_points_at_problem() {
        match parse("status = ").unwrap_err() {
            AppError::Filter { message, position } => {
                assert_eq!(position, 9);
                assert!(message.contains("value"), "{message}");
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn deep_nesting_is_rejected() {
        let input = format!("{}a = 1", "(".repeat(1000));
        match parse(&input).unwrap_err() {
            AppError::Filter { message, position } => {
                assert_eq!(message, "Too deeply nested");
                assert_eq!(position, MAX_DEPTH);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn nesting_up_to_the_limit_parses() {
        let input = format!("{}a = 1{}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
        assert_eq!(parse(&input).unwrap(), cmp("a", MAX_DEPTH, CmpOp::Eq, n("1")));
    }

    #[test]
    fn error_unclosed_string() {
        match parse("name = \"abc").unwrap_err() {
            AppError::Filter { position, .. } => assert_eq!(position, 7),
            e => panic!("{e:?}"),
        }
    }
}
