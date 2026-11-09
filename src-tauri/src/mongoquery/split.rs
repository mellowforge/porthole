//! Statements of a MongoDB query tab, their parameters, and the editor's analysis of the text.

use super::call::classify_tokens;
use super::lex::{self, Tok, Token};
use crate::sqltext::{SqlAnalysis, StatementKind, StatementSpan, Utf16Index};

/// One statement's place in the text, in char offsets (the end exclusive), and its
/// classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MongoSpan {
    pub start: usize,
    pub end: usize,
    pub kind: StatementKind,
    pub verb: String,
}

/// The statements of `text`, separated by `;` or by a line break, both outside brackets; a line
/// that starts with `.` continues the chain above it. Never fails: from a character that
/// doesn't lex, the statement runs to the end of the text.
pub fn split(text: &str) -> Vec<MongoSpan> {
    let (tokens, bad) = lex::tokenize_lenient(text);
    spans(text, &tokens, bad)
}

/// The parameters (`:name` in value positions) of `stmt`, distinct, in first-use order.
pub fn find_params(stmt: &str) -> Vec<String> {
    params(&lex::tokenize_lenient(stmt).0)
}

/// The statements and parameters of `text`, in UTF-16 offsets, as `sqltext::analyze_in` gives
/// them for SQL.
pub fn analyze(text: &str) -> SqlAnalysis {
    let ix = Utf16Index::new(text);
    let (tokens, bad) = lex::tokenize_lenient(text);
    let statements = spans(text, &tokens, bad)
        .into_iter()
        .map(|s| StatementSpan {
            from: ix.to_utf16(s.start),
            to: ix.to_utf16(s.end),
            kind: s.kind,
            verb: s.verb,
        })
        .collect();
    SqlAnalysis {
        statements,
        params: params(&tokens),
    }
}

fn params(tokens: &[Token]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for t in tokens {
        if let Tok::Param(name) = &t.kind
            && !names.contains(name)
        {
            names.push(name.clone());
        }
    }
    names
}

/// `tokens` lexed from `text`, up to `bad` where lexing failed.
fn spans(text: &str, tokens: &[Token], bad: Option<usize>) -> Vec<MongoSpan> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut first = 0;
    for (i, t) in tokens.iter().enumerate() {
        let ends = match t.kind {
            Tok::LBrace | Tok::LBracket | Tok::LParen => {
                depth += 1;
                false
            }
            Tok::RBrace | Tok::RBracket | Tok::RParen => {
                depth = depth.saturating_sub(1);
                false
            }
            Tok::Semi => depth == 0,
            // The tail after a lexing error decides for a trailing line break, below.
            Tok::Newline => depth == 0 && tokens.get(i + 1).is_some_and(|n| n.kind != Tok::Dot),
            _ => false,
        };
        if ends {
            push(&mut out, &tokens[first..i], None);
            first = i + 1;
        }
    }
    let open = &tokens[first..];
    let Some(at) = bad else {
        push(&mut out, open, None);
        return out;
    };
    // From the bad character the text is one statement to its end: the open one, unless a
    // line break outside brackets comes first.
    let end = text.trim_end().chars().count();
    let prev_end = open
        .iter()
        .rev()
        .find(|t| t.kind != Tok::Newline)
        .map_or(0, |t| t.end);
    let new_line = chars[prev_end.min(at)..at]
        .iter()
        .any(|c| matches!(c, '\n' | '\r'));
    if open.iter().all(|t| t.kind == Tok::Newline) || (depth == 0 && new_line && chars[at] != '.') {
        push(&mut out, open, None);
        push(&mut out, &[], Some((at, end)));
    } else {
        push(&mut out, open, Some((at, end)));
    }
    out
}

/// Adds the statement made of `group`, or, with `tail`, the one from `group`'s first token
/// (else `tail.0`) to `tail.1`. An empty group without a tail adds nothing.
fn push(out: &mut Vec<MongoSpan>, group: &[Token], tail: Option<(usize, usize)>) {
    let mut real = group.iter().filter(|t| t.kind != Tok::Newline);
    let first = real.next();
    let last = real.next_back().or(first);
    let (start, end) = match (first, last, tail) {
        (Some(f), _, Some((_, end))) => (f.start, end),
        (None, _, Some((at, end))) => (at, end),
        (Some(f), Some(l), None) => (f.start, l.end),
        _ => return,
    };
    let (kind, verb) = classify_tokens(group);
    out.push(MongoSpan {
        start,
        end,
        kind,
        verb,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementKind::{Read, Write};

    /// Each statement's text, kind and verb.
    fn statements(text: &str) -> Vec<(String, StatementKind, String)> {
        let chars: Vec<char> = text.chars().collect();
        split(text)
            .into_iter()
            .map(|s| (chars[s.start..s.end].iter().collect(), s.kind, s.verb))
            .collect()
    }

    fn st(text: &str, kind: StatementKind, verb: &str) -> (String, StatementKind, String) {
        (text.into(), kind, verb.into())
    }

    #[test]
    fn split_on_semicolons_and_newlines() {
        assert_eq!(
            statements("db.a.find()\ndb.b.deleteOne({})"),
            vec![
                st("db.a.find()", Read, "find"),
                st("db.b.deleteOne({})", Write, "deleteOne")
            ]
        );
        // A chain continues on the next line.
        let chain = "db.a.find({})\n  .sort({ x: 1 })\n\n  .limit(5)";
        assert_eq!(statements(chain), vec![st(chain, Read, "find")]);
        // A line break inside brackets doesn't end the statement.
        let multi = "db.a.insertOne({\n  a: 1,\n  b: [\n    2\n  ]\n})";
        assert_eq!(statements(multi), vec![st(multi, Write, "insertOne")]);
        // `;` inside a string doesn't split; `;` itself isn't part of a span.
        assert_eq!(
            statements("db.a.insertOne({ s: \"x;y\" }); db.b.find() ;"),
            vec![
                st("db.a.insertOne({ s: \"x;y\" })", Write, "insertOne"),
                st("db.b.find()", Read, "find")
            ]
        );
        // Comments between statements aren't part of a span.
        assert_eq!(
            statements("// head\ndb.a.find() // tail\n/* between\n */\n  db.b.find();\n// end\n"),
            vec![
                st("db.a.find()", Read, "find"),
                st("db.b.find()", Read, "find")
            ]
        );
        // A comment line between a statement and its chain keeps them together.
        let commented = "db.a.find()\n// sorted\n.sort({ a: 1 })";
        assert_eq!(statements(commented), vec![st(commented, Read, "find")]);
        assert_eq!(statements(""), vec![]);
        assert_eq!(statements(" ;; \n // x"), vec![]);
    }

    #[test]
    fn split_never_fails() {
        // From a character that doesn't lex, the statement runs to the end of the text.
        assert_eq!(
            statements("db.a.find()\ndb.b.deleteOne({ a: \"open\ndb.c.find()\n"),
            vec![
                st("db.a.find()", Read, "find"),
                st(
                    "db.b.deleteOne({ a: \"open\ndb.c.find()",
                    Write,
                    "deleteOne"
                )
            ]
        );
        assert_eq!(
            statements("db.a.find(); @ db.b.find()"),
            vec![
                st("db.a.find()", Read, "find"),
                st("@ db.b.find()", Read, "")
            ]
        );
        assert_eq!(
            statements("db.a.find()\n/* open"),
            vec![st("db.a.find()", Read, "find"), st("/* open", Read, "")]
        );
        // An invalid escape fails the whole string.
        assert_eq!(
            statements("db.a.find({ a: \"x\\u12\" })"),
            vec![st("db.a.find({ a: \"x\\u12\" })", Read, "find")]
        );
    }

    #[test]
    fn params_only_in_value_positions() {
        assert_eq!(
            find_params(r#"{ a: :x, b: ":y", c: /:z/ } // :w"#),
            vec!["x".to_string()]
        );
        assert_eq!(
            find_params("db.a.find({ _id: ObjectId(:id) }) /* :c */"),
            vec!["id".to_string()]
        );
        assert_eq!(
            find_params("db.a.find({ _id: ObjectId(\":id\") })"),
            Vec::<String>::new()
        );
        assert_eq!(
            find_params("db.a.find({ a: :b, c: [:a, :b], \"k\": 1 }).limit(:n)"),
            vec!["b".to_string(), "a".into(), "n".into()]
        );
        // Not a key, and a statement that doesn't lex still reports the ones before.
        assert_eq!(
            find_params("db.a.find({ :k: 1, a: :v, s: \"open"),
            vec!["v".to_string()]
        );
    }

    #[test]
    fn analyze_reports_utf16_spans() {
        let first = "db.a.find({ n: \"😀\" })";
        let second = "db.b.updateOne({ a: :a }, { $set: { b: :b } })";
        let text = format!("{first}\n{second};\n");
        let first_to = first.encode_utf16().count();
        assert_eq!(first_to, first.chars().count() + 1);
        let second_from = first_to + 1;
        assert_eq!(
            analyze(&text),
            SqlAnalysis {
                statements: vec![
                    StatementSpan {
                        from: 0,
                        to: first_to,
                        kind: Read,
                        verb: "find".into()
                    },
                    StatementSpan {
                        from: second_from,
                        to: second_from + second.encode_utf16().count(),
                        kind: Write,
                        verb: "updateOne".into()
                    },
                ],
                params: vec!["a".into(), "b".into()],
            }
        );
        assert_eq!(
            analyze(""),
            SqlAnalysis {
                statements: vec![],
                params: vec![]
            }
        );
    }
}
