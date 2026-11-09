//! SQL text rules: statement splitting, `:name` parameters and read/write
//! classification. All of them share one tokenizer, so they can never
//! disagree about what is a string, a comment or a quoted identifier.
//!
//! Offsets are char offsets unless a name says utf16.

use serde::Serialize;

/// The SQL flavour a statement is lexed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlDialect {
    Postgres,
    Sqlite,
    Mysql,
}

/// The tokens of `chars` under `d`'s lexing rules.
fn tokenize_in(d: SqlDialect, chars: &[char]) -> Vec<Token> {
    tokenize(d, chars)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StatementKind {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub start: usize,
    pub end: usize,
    pub kind: StatementKind,
    pub verb: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// Bare word, uppercased.
    Word(String),
    Param(String),
    Open,
    Close,
    Comma,
    Semi,
    /// MySQL: a statement end no compound body absorbs — a custom delimiter,
    /// or a whole `DELIMITER x` line.
    Delim,
    /// MySQL: a `;` under a custom delimiter, where it ends nothing (the server still runs the
    /// statements on either side of it, one after the other).
    InnerSemi,
    /// MySQL: a `/*! … */` comment. The server runs its text, so it belongs to a
    /// statement, but classification looks only at what is outside it.
    Versioned,
    /// A bare `=` (SQLite classification needs it for `PRAGMA x = v`).
    Eq,
    /// Strings, quoted identifiers, numbers, operators.
    Other,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}

fn is_param_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Index just past a quoted run that starts at `i` (the opening quote).
/// `doubled` quotes escape the quote; `backslash` also lets `\` escape.
fn skip_quoted(chars: &[char], i: usize, backslash: bool) -> usize {
    let q = chars[i];
    let mut j = i + 1;
    while j < chars.len() {
        let c = chars[j];
        if backslash && c == '\\' {
            j += 2;
        } else if c == q {
            if chars.get(j + 1) == Some(&q) {
                j += 2;
            } else {
                return j + 1;
            }
        } else {
            j += 1;
        }
    }
    chars.len()
}

/// If a dollar-quote tag (`$$` or `$tag$`) opens at `i`, returns its length.
fn dollar_tag_len(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if chars.get(j).is_some_and(|&c| is_ident_start(c)) {
        while chars.get(j).is_some_and(|&c| is_ident_char(c) && c != '$') {
            j += 1;
        }
    }
    (chars.get(j) == Some(&'$')).then_some(j + 1 - i)
}

/// Whether the text before `i` on its line is blank.
fn at_line_start(chars: &[char], i: usize) -> bool {
    chars[..i].iter().rev().take_while(|&&c| c != '\n').all(|c| c.is_whitespace())
}

/// If `chars[i..]` is a MySQL `DELIMITER x` line, returns the line's end and the
/// new delimiter. The word must be the line's first, with something after it.
fn delimiter_line(chars: &[char], i: usize) -> Option<(usize, Vec<char>)> {
    const WORD: &str = "delimiter";
    let word = chars.get(i..i + WORD.len())?;
    if !word.iter().zip(WORD.chars()).all(|(a, b)| a.eq_ignore_ascii_case(&b))
        || !chars.get(i + WORD.len()).is_some_and(|&c| c == ' ' || c == '\t')
        || !at_line_start(chars, i)
    {
        return None;
    }
    let eol = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
    let rest: String = chars[i + WORD.len()..eol].iter().collect();
    let rest = rest.trim();
    (!rest.is_empty()).then(|| (eol, rest.chars().collect()))
}

fn tokenize(d: SqlDialect, chars: &[char]) -> Vec<Token> {
    let lite = d == SqlDialect::Sqlite;
    let my = d == SqlDialect::Mysql;
    let pg = d == SqlDialect::Postgres;
    let n = chars.len();
    let mut out = Vec::new();
    // MySQL: the statement delimiter a `DELIMITER` line set, while it isn't `;`.
    let mut delim: Option<Vec<char>> = None;
    let at_delim = |delim: &Option<Vec<char>>, i: usize| delim.as_ref().is_some_and(|d| chars[i..].starts_with(d));
    let mut i = 0;
    while i < n {
        let c = chars[i];
        let start = i;
        let tok = if c.is_whitespace() {
            i += 1;
            continue;
        } else if at_delim(&delim, i) {
            i += delim.as_ref().map_or(0, Vec::len);
            Tok::Delim
        } else if let Some((eol, new)) = my.then(|| delimiter_line(chars, i)).flatten() {
            delim = (new != [';']).then_some(new);
            i = eol;
            Tok::Delim
        } else if (c == '-' && chars.get(i + 1) == Some(&'-') && (!my || chars.get(i + 2).is_none_or(|x| x.is_whitespace())))
            || (my && c == '#')
        {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            // MySQL runs the text of `/*! … */` (and MariaDB's `/*M! … */`).
            let versioned = my && (chars.get(i + 2) == Some(&'!') || chars[i + 2..].starts_with(&['M', '!']));
            let mut depth = 1;
            i += 2;
            while i < n && depth > 0 {
                if pg && chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if !versioned {
                continue;
            }
            Tok::Versioned
        } else if my && matches!(c, '\'' | '"' | '`') {
            i = skip_quoted(chars, i, c != '`');
            Tok::Other
        } else if c == '\'' || c == '"' || (lite && c == '`') {
            i = skip_quoted(chars, i, false);
            Tok::Other
        } else if lite && c == '[' {
            // Bracket-quoted identifier: no escapes, ends at the first `]`.
            i = chars[i..].iter().position(|&x| x == ']').map_or(n, |p| i + p + 1);
            Tok::Other
        } else if lite && matches!(c, '$' | '@' | '?') {
            let digits = c == '?';
            let ok = |x: char| if digits { x.is_ascii_digit() } else { x.is_ascii_alphabetic() || x == '_' };
            if chars.get(i + 1).is_some_and(|&x| ok(x)) {
                i += 1;
                let name_start = i;
                while i < n && is_param_char(chars[i]) && (!digits || chars[i].is_ascii_digit()) {
                    i += 1;
                }
                Tok::Param(chars[name_start..i].iter().collect())
            } else {
                i += 1;
                Tok::Other
            }
        } else if my && c == '@' {
            // `@user_var`, `@@system_var`, `@'quoted'`: one token, so `@end` is no keyword.
            i += if chars.get(i + 1) == Some(&'@') { 2 } else { 1 };
            match chars.get(i) {
                Some(&q @ ('\'' | '"' | '`')) => i = skip_quoted(chars, i, q != '`'),
                _ => {
                    while i < n && (is_ident_char(chars[i]) || chars[i] == '.') && !at_delim(&delim, i) {
                        i += 1;
                    }
                }
            }
            Tok::Other
        } else if let Some(tag_len) = (pg && c == '$').then(|| dollar_tag_len(chars, i)).flatten() {
            let tag = &chars[i..i + tag_len];
            i += tag_len;
            while i < n && !chars[i..].starts_with(tag) {
                i += 1;
            }
            i = (i + tag_len).min(n);
            Tok::Other
        } else if c == ':' {
            // SQLite has no `::` cast, so `:name` is a parameter wherever it stands.
            // MySQL has no cast either, but `label:BEGIN` is no parameter.
            let prev_ok = lite || i == 0 || {
                let p = chars[i - 1];
                if my {
                    !(is_ident_char(p) || p == '`')
                } else {
                    !(p == ':' || is_param_char(p) || p == ']' || p == ')')
                }
            };
            let next = chars.get(i + 1).copied();
            if prev_ok && next.is_some_and(|x| x.is_ascii_alphabetic() || x == '_') {
                i += 1;
                let name_start = i;
                while i < n && is_param_char(chars[i]) {
                    i += 1;
                }
                Tok::Param(chars[name_start..i].iter().collect())
            } else {
                i += 1;
                Tok::Other
            }
        } else if is_ident_start(c) || (my && c == '$') {
            while i < n && is_ident_char(chars[i]) && !at_delim(&delim, i) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if pg && (word == "e" || word == "E") && chars.get(i) == Some(&'\'') {
                i = skip_quoted(chars, i, true);
                Tok::Other
            } else {
                Tok::Word(word.to_uppercase())
            }
        } else if c.is_ascii_digit() {
            while i < n && is_ident_char(chars[i]) && !at_delim(&delim, i) {
                i += 1;
            }
            Tok::Other
        } else {
            i += 1;
            match c {
                '(' => Tok::Open,
                ')' => Tok::Close,
                ',' => Tok::Comma,
                // Under a custom MySQL delimiter, `;` ends nothing.
                ';' if delim.is_some() => Tok::InnerSemi,
                ';' => Tok::Semi,
                '=' => Tok::Eq,
                _ => Tok::Other,
            }
        };
        out.push(Token { tok, start, end: i });
    }
    out
}

pub fn split_statements(sql: &str) -> Vec<Statement> {
    split_statements_in(SqlDialect::Postgres, sql)
}

pub fn split_statements_in(d: SqlDialect, sql: &str) -> Vec<Statement> {
    let chars: Vec<char> = sql.chars().collect();
    split_tokens(d, &tokenize_in(d, &chars))
}

/// Statement groups: the tokens between top-level `;`. A compound body is not
/// split at its inner `;`: SQLite's `CREATE TRIGGER … BEGIN …; …; END`, and
/// MySQL's `CREATE … {PROCEDURE | FUNCTION | TRIGGER | EVENT} … BEGIN …; END`.
/// A MySQL `Delim` always ends a group.
fn split_groups(d: SqlDialect, tokens: &[Token]) -> Vec<&[Token]> {
    let (lite, my) = (d == SqlDialect::Sqlite, d == SqlDialect::Mysql);
    let mut groups = Vec::new();
    let (mut from, mut in_body, mut case_depth, mut body_ended) = (0, false, 0usize, false);
    // MySQL: the body's open `BEGIN`s and `CASE`s, its own `BEGIN` included.
    let mut depth = 0usize;
    let follows_end = |i: usize| i > 0 && word(tokens.get(i - 1)) == Some("END");
    for (i, t) in tokens.iter().enumerate() {
        match &t.tok {
            Tok::Word(w) if lite && !in_body && w == "BEGIN" && is_create_trigger(&tokens[from..i]) => {
                in_body = true;
            }
            Tok::Word(w) if lite && in_body && w == "CASE" => case_depth += 1,
            Tok::Word(w) if lite && in_body && w == "END" => {
                // An `END` that closes no `CASE` ends the trigger body.
                body_ended = case_depth == 0;
                case_depth = case_depth.saturating_sub(1);
            }
            Tok::Word(w) if my && !in_body && w == "BEGIN" && is_create_routine(&tokens[from..i]) => {
                in_body = true;
                depth = 1;
            }
            // `END CASE`'s `CASE` opens nothing: its `END` closed the `CASE`.
            Tok::Word(w) if my && depth > 0 && (w == "BEGIN" || (w == "CASE" && !follows_end(i))) => depth += 1,
            // `END IF`, `END LOOP`, `END WHILE` and `END REPEAT` close blocks that were never counted.
            Tok::Word(w) if my && depth > 0 && w == "END" => {
                if !matches!(word(tokens.get(i + 1)), Some("IF" | "LOOP" | "WHILE" | "REPEAT")) {
                    depth -= 1;
                }
            }
            Tok::Semi | Tok::Delim => {
                let open = if my { depth > 0 } else { in_body && !body_ended };
                if t.tok == Tok::Delim || !open {
                    groups.push(&tokens[from..i]);
                    from = i + 1;
                    in_body = false;
                    case_depth = 0;
                    body_ended = false;
                    depth = 0;
                }
            }
            _ => body_ended = false,
        }
    }
    groups.push(&tokens[from..]);
    groups
}

/// Whether `head` (the tokens before a `BEGIN`) is `CREATE [TEMP] TRIGGER …`.
fn is_create_trigger(head: &[Token]) -> bool {
    let mut w = head.iter().map(|t| match &t.tok {
        Tok::Word(w) => w.as_str(),
        _ => "",
    });
    w.next() == Some("CREATE")
        && match w.next() {
            Some("TEMP" | "TEMPORARY") => w.next() == Some("TRIGGER"),
            second => second == Some("TRIGGER"),
        }
}

/// Whether `head` (the tokens before a `BEGIN`) is MySQL's
/// `CREATE [OR REPLACE] [DEFINER = user] [AGGREGATE] {PROCEDURE | FUNCTION | TRIGGER | EVENT} …`.
fn is_create_routine(head: &[Token]) -> bool {
    if word(head.first()) != Some("CREATE") {
        return false;
    }
    let mut i = 1;
    if word(head.get(i)) == Some("OR") && word(head.get(i + 1)) == Some("REPLACE") {
        i += 2;
    }
    if word(head.get(i)) == Some("DEFINER") {
        i += 1;
        if head.get(i).is_some_and(|t| t.tok == Tok::Eq) {
            i += 1;
        }
        // The user: a name or `CURRENT_USER`, then any `@host` or `()`.
        i += 1;
        while head.get(i).is_some_and(|t| matches!(t.tok, Tok::Other | Tok::Open | Tok::Close)) {
            i += 1;
        }
    }
    if word(head.get(i)) == Some("AGGREGATE") {
        i += 1;
    }
    matches!(word(head.get(i)), Some("PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT"))
}

fn split_tokens(d: SqlDialect, tokens: &[Token]) -> Vec<Statement> {
    split_groups(d, tokens)
        .into_iter()
        .filter(|g| !g.is_empty())
        .map(|g| {
            let (kind, verb) = classify_tokens(d, g);
            Statement {
                start: g[0].start,
                end: g[g.len() - 1].end,
                kind,
                verb,
            }
        })
        .collect()
}

pub fn find_params(sql: &str) -> Vec<String> {
    find_params_in(SqlDialect::Postgres, sql)
}

pub fn find_params_in(d: SqlDialect, sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut names: Vec<String> = Vec::new();
    for t in tokenize_in(d, &chars) {
        if let Tok::Param(name) = t.tok
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

#[derive(Debug, Clone)]
struct Replacement {
    rewritten: std::ops::Range<usize>,
    original: std::ops::Range<usize>,
}

#[derive(Debug, Clone)]
pub struct Rewritten {
    pub sql: String,
    /// The distinct names, in order of first use.
    pub names: Vec<String>,
    /// The name each placeholder binds, in text order: MySQL's `?` repeats a
    /// name for each use; numbered placeholders bind each name once (`names`).
    pub bind_order: Vec<String>,
    replacements: Vec<Replacement>,
}

impl Rewritten {
    /// Maps a char index in the rewritten text to the char index in the
    /// original. Inside a placeholder it maps to the start of the `:name` it replaced.
    pub fn original_pos(&self, rewritten_char: usize) -> usize {
        let mut shift: isize = 0;
        for r in &self.replacements {
            if rewritten_char < r.rewritten.start {
                break;
            }
            if rewritten_char < r.rewritten.end {
                return r.original.start;
            }
            shift = r.original.end as isize - r.rewritten.end as isize;
        }
        (rewritten_char as isize + shift) as usize
    }
}

pub fn rewrite_params(stmt: &str) -> Rewritten {
    rewrite_params_in(SqlDialect::Postgres, stmt)
}

pub fn rewrite_params_in(d: SqlDialect, stmt: &str) -> Rewritten {
    let chars: Vec<char> = stmt.chars().collect();
    let mut sql = String::new();
    let mut names: Vec<String> = Vec::new();
    let mut bind_order: Vec<String> = Vec::new();
    let mut replacements = Vec::new();
    let mut copied = 0; // chars of the original already emitted
    let mut out_len = 0; // chars of `sql`
    for t in tokenize_in(d, &chars) {
        let Tok::Param(name) = t.tok else { continue };
        if d == SqlDialect::Mysql {
            bind_order.push(name.clone());
        }
        let k = match names.iter().position(|n| *n == name) {
            Some(p) => p + 1,
            None => {
                names.push(name);
                names.len()
            }
        };
        sql.extend(&chars[copied..t.start]);
        out_len += t.start - copied;
        let placeholder = match d {
            SqlDialect::Sqlite => format!("?{k}"),
            SqlDialect::Mysql => "?".to_string(),
            SqlDialect::Postgres => format!("${k}"),
        };
        let len = placeholder.chars().count();
        sql.push_str(&placeholder);
        replacements.push(Replacement {
            rewritten: out_len..out_len + len,
            original: t.start..t.end,
        });
        out_len += len;
        copied = t.end;
    }
    sql.extend(&chars[copied..]);
    if d != SqlDialect::Mysql {
        bind_order = names.clone();
    }
    Rewritten { sql, names, bind_order, replacements }
}

pub fn classify(stmt: &str) -> (StatementKind, String) {
    classify_in(SqlDialect::Postgres, stmt)
}

pub fn classify_in(d: SqlDialect, stmt: &str) -> (StatementKind, String) {
    let chars: Vec<char> = stmt.chars().collect();
    classify_tokens(d, &tokenize_in(d, &chars))
}

/// The statement's first keyword, uppercased, after leading comments and `(`.
pub fn leading_keyword(stmt: &str) -> Option<String> {
    leading_keyword_in(SqlDialect::Postgres, stmt)
}

pub fn leading_keyword_in(d: SqlDialect, stmt: &str) -> Option<String> {
    let chars: Vec<char> = stmt.chars().collect();
    let toks = tokenize_in(d, &chars);
    let first = toks.iter().position(|t| t.tok != Tok::Open && t.tok != Tok::Versioned)?;
    word(toks.get(first)).map(str::to_string)
}

fn word(t: Option<&Token>) -> Option<&str> {
    match t {
        Some(Token { tok: Tok::Word(w), .. }) => Some(w),
        _ => None,
    }
}

/// Index just past the balanced group whose `(` is at `i`.
fn skip_group(toks: &[Token], i: usize) -> usize {
    let mut depth = 0usize;
    for (j, t) in toks.iter().enumerate().skip(i) {
        match t.tok {
            Tok::Open => depth += 1,
            Tok::Close => {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

fn is_analyze(w: &str) -> bool {
    w == "ANALYZE" || w == "ANALYSE"
}

fn classify_tokens(d: SqlDialect, toks: &[Token]) -> (StatementKind, String) {
    let outside: Vec<Token>;
    let toks = if toks.iter().any(|t| t.tok == Tok::Versioned) {
        outside = toks.iter().filter(|t| t.tok != Tok::Versioned).cloned().collect();
        &outside[..]
    } else {
        toks
    };
    let mut i = 0;
    while toks.get(i).is_some_and(|t| t.tok == Tok::Open) {
        i += 1;
    }
    let (kind, verb) = classify_at(d, toks, i);
    if d == SqlDialect::Mysql && holds_several_statements(toks) {
        // Only a custom DELIMITER (or a compound body, which only a `CREATE` has) puts a `;`
        // inside one statement. The server runs everything after it too, so when unsure, ask.
        return (StatementKind::Write, verb);
    }
    (kind, verb)
}

/// Whether a MySQL statement's tokens hold a `;` with more after it: a trailing `;` ends
/// nothing that follows.
fn holds_several_statements(toks: &[Token]) -> bool {
    let is_semi = |t: &Token| matches!(t.tok, Tok::Semi | Tok::InnerSemi);
    toks.iter().rev().skip_while(|t| is_semi(t)).any(is_semi)
}

fn classify_at(d: SqlDialect, toks: &[Token], i: usize) -> (StatementKind, String) {
    use StatementKind::{Read, Write};
    let Some(verb) = word(toks.get(i)) else {
        return (Write, String::new());
    };
    let verb = verb.to_string();
    let kind = match verb.as_str() {
        // SQLite: `PRAGMA x` and `PRAGMA x(arg)` read; `PRAGMA x = v` writes.
        "PRAGMA" if d == SqlDialect::Sqlite => {
            let mut depth = 0isize;
            let (mut sets, mut called) = (false, false);
            let mut name = "";
            for t in &toks[i + 1..] {
                match &t.tok {
                    Tok::Word(w) if depth == 0 && !called => name = w, // last word before `(`: after any `schema.`
                    Tok::Open => {
                        called |= depth == 0;
                        depth += 1;
                    }
                    Tok::Close => depth -= 1,
                    Tok::Eq if depth <= 0 => sets = true,
                    _ => {}
                }
            }
            const QUERY: [&str; 10] = [
                "TABLE_INFO", "TABLE_XINFO", "TABLE_LIST", "INDEX_INFO", "INDEX_XINFO", "INDEX_LIST",
                "FOREIGN_KEY_LIST", "FOREIGN_KEY_CHECK", "INTEGRITY_CHECK", "QUICK_CHECK",
            ];
            const BARE_WRITES: [&str; 4] = ["OPTIMIZE", "WAL_CHECKPOINT", "INCREMENTAL_VACUUM", "SHRINK_MEMORY"];
            let writes = sets
                || if called { !QUERY.contains(&name) } else { BARE_WRITES.contains(&name) };
            if writes { Write } else { Read }
        }
        // SQLite's EXPLAIN never runs the statement (and has no ANALYZE option).
        "EXPLAIN" if d == SqlDialect::Sqlite => Read,
        // These are Postgres-only; SQLite would reject them, so they stay Write there.
        "TABLE" | "SHOW" | "SET" | "RESET" | "START" | "ABORT" if d == SqlDialect::Sqlite => Write,
        // MySQL: `DESCRIBE` and `DESC` are `EXPLAIN`'s synonyms (`DESC t` describes a table).
        "EXPLAIN" | "DESCRIBE" | "DESC" if d == SqlDialect::Mysql => classify_explain(d, toks, i + 1),
        "HELP" | "USE" if d == SqlDialect::Mysql => Read,
        // MySQL: `RESET MASTER` deletes binary logs; `START REPLICA` and `START
        // GROUP_REPLICATION` are admin commands. Only `START TRANSACTION` reads.
        "RESET" if d == SqlDialect::Mysql => Write,
        "START" if d == SqlDialect::Mysql => {
            if word(toks.get(i + 1)) == Some("TRANSACTION") { Read } else { Write }
        }
        // MySQL's `SET` assigns variables. The rest would fall to `_` anyway; listed as the rule.
        "SET" | "REPLACE" | "LOAD" | "CALL" | "DO" | "HANDLER" | "LOCK" | "UNLOCK" | "FLUSH" | "OPTIMIZE"
        | "REPAIR" | "ANALYZE" | "RENAME" | "TRUNCATE" | "GRANT" | "REVOKE" | "INSTALL" | "KILL"
            if d == SqlDialect::Mysql =>
        {
            Write
        }
        "SELECT" => {
            let mut depth = 0isize;
            let mut into = false;
            for t in &toks[i + 1..] {
                match &t.tok {
                    Tok::Open => depth += 1,
                    Tok::Close => depth -= 1,
                    Tok::Word(w) if w == "INTO" && depth <= 0 => into = true,
                    _ => {}
                }
            }
            if into { Write } else { Read }
        }
        "VALUES" | "TABLE" | "SHOW" | "SET" | "RESET" | "BEGIN" | "START" | "COMMIT" | "END"
        | "ROLLBACK" | "ABORT" | "SAVEPOINT" | "RELEASE" => Read,
        "EXPLAIN" => classify_explain(d, toks, i + 1),
        "WITH" => return classify_with(d, toks, i + 1),
        _ => Write,
    };
    (kind, verb)
}

fn classify_explain(d: SqlDialect, toks: &[Token], mut i: usize) -> StatementKind {
    let my = d == SqlDialect::Mysql;
    let mut analyze = false;
    loop {
        match toks.get(i).map(|t| &t.tok) {
            Some(Tok::Word(w)) if is_analyze(w) => {
                analyze = true;
                i += 1;
            }
            // MySQL: `FORMAT = name`, and MariaDB's `EXTENDED` / `PARTITIONS`.
            Some(Tok::Word(w)) if my && w == "FORMAT" => {
                i += 1;
                if toks.get(i).is_some_and(|t| t.tok == Tok::Eq) {
                    i += 1;
                }
                i += 1;
            }
            Some(Tok::Word(w)) if my && (w == "EXTENDED" || w == "PARTITIONS") => i += 1,
            Some(Tok::Word(w)) if !my && w == "VERBOSE" => i += 1,
            // In MySQL a `(` opens the explained query, not options.
            Some(Tok::Open) if !my => {
                let end = skip_group(toks, i);
                analyze |= toks[i..end]
                    .iter()
                    .any(|t| matches!(&t.tok, Tok::Word(w) if is_analyze(w)));
                i = end;
            }
            _ => break,
        }
    }
    if !analyze {
        return StatementKind::Read;
    }
    // EXPLAIN ANALYZE executes the explained statement.
    let mut j = i;
    while toks.get(j).is_some_and(|t| t.tok == Tok::Open) {
        j += 1;
    }
    classify_at(d, toks, j).0
}

fn classify_with(d: SqlDialect, toks: &[Token], mut i: usize) -> (StatementKind, String) {
    use StatementKind::Write;
    let malformed = || (Write, "WITH".to_string());
    let mut modifies = false;
    if word(toks.get(i)) == Some("RECURSIVE") {
        i += 1;
    }
    loop {
        i += 1; // CTE name
        if toks.get(i).is_some_and(|t| t.tok == Tok::Open) {
            i = skip_group(toks, i); // column list
        }
        if word(toks.get(i)) != Some("AS") {
            return malformed();
        }
        i += 1;
        if word(toks.get(i)) == Some("NOT") {
            i += 1;
        }
        if word(toks.get(i)) == Some("MATERIALIZED") {
            i += 1;
        }
        if toks.get(i).is_none_or(|t| t.tok != Tok::Open) {
            return malformed();
        }
        modifies |= matches!(
            word(toks.get(i + 1)),
            Some("INSERT" | "UPDATE" | "DELETE" | "MERGE")
        );
        i = skip_group(toks, i);
        if toks.get(i).is_some_and(|t| t.tok == Tok::Comma) {
            i += 1;
        } else {
            break;
        }
    }
    let (kind, verb) = classify_at(d, toks, i);
    if verb.is_empty() {
        return malformed();
    }
    (if modifies { Write } else { kind }, verb)
}

/// Converts between char offsets and UTF-16 code-unit offsets of one text.
pub struct Utf16Index {
    /// `offsets[c]` is the UTF-16 offset of char `c`; the last entry is the total.
    offsets: Vec<usize>,
}

impl Utf16Index {
    pub fn new(text: &str) -> Self {
        let mut offsets = Vec::with_capacity(text.len() + 1);
        let mut u = 0;
        for c in text.chars() {
            offsets.push(u);
            u += c.len_utf16();
        }
        offsets.push(u);
        Self { offsets }
    }

    pub fn to_utf16(&self, char_idx: usize) -> usize {
        self.offsets[char_idx.min(self.offsets.len() - 1)]
    }

    /// The char whose UTF-16 span contains `utf16_idx`.
    pub fn to_char(&self, utf16_idx: usize) -> usize {
        let last = self.offsets.len() - 1;
        let p = self.offsets.partition_point(|&o| o <= utf16_idx);
        p.saturating_sub(1).min(last)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementSpan {
    pub from: usize,
    pub to: usize,
    pub kind: StatementKind,
    pub verb: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlAnalysis {
    pub statements: Vec<StatementSpan>,
    pub params: Vec<String>,
}

pub fn analyze(sql: &str) -> SqlAnalysis {
    analyze_in(SqlDialect::Postgres, sql)
}

pub fn analyze_in(d: SqlDialect, sql: &str) -> SqlAnalysis {
    let ix = Utf16Index::new(sql);
    let chars: Vec<char> = sql.chars().collect();
    let tokens = tokenize_in(d, &chars);
    let mut params: Vec<String> = Vec::new();
    for t in &tokens {
        if let Tok::Param(name) = &t.tok
            && !params.contains(name)
        {
            params.push(name.clone());
        }
    }
    let statements = split_tokens(d, &tokens)
        .into_iter()
        .map(|s| StatementSpan {
            from: ix.to_utf16(s.start),
            to: ix.to_utf16(s.end),
            kind: s.kind,
            verb: s.verb,
        })
        .collect();
    SqlAnalysis { statements, params }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn wrappers_equal_postgres_variants() {
        let pg = SqlDialect::Postgres;
        for x in ["select 1; select 2", "-- c\nSELECT 1;\n\n  UPDATE t SET a = 1", "select $$a;b$$; select :a, :b, :a",
                  "x > :since and y = :lim or z = :since", "select ':a'; select x::int", "", ";;",
                  "with a as (select 1) delete from t", "explain analyze select 1", "select 'é' ; select :é"] {
            assert_eq!(split_statements(x), split_statements_in(pg, x), "{x}");
            assert_eq!(find_params(x), find_params_in(pg, x), "{x}");
            assert_eq!(classify(x), classify_in(pg, x), "{x}");
            assert_eq!(leading_keyword(x), leading_keyword_in(pg, x), "{x}");
            assert_eq!(analyze(x), analyze_in(pg, x), "{x}");
            let (a, b) = (rewrite_params(x), rewrite_params_in(pg, x));
            assert_eq!((&a.sql, &a.names), (&b.sql, &b.names), "{x}");
            for i in 0..8 { assert_eq!(a.replacements.len(), b.replacements.len()); assert_eq!(a.original_pos(i), b.original_pos(i)); }
        }
    }

    #[test] fn split_basic() {
        let s = split_statements("select 1; select 2");
        assert_eq!(s.iter().map(|x| (x.start, x.end)).collect::<Vec<_>>(), vec![(0, 8), (10, 18)]);
    }
    #[test] fn split_skips_leading_comments_and_blank() {
        let sql = "-- c\nSELECT 1;\n\n  UPDATE t SET a = 1";
        let s = split_statements(sql);
        assert_eq!(s.len(), 2);
        assert_eq!(&sql[s[0].start..s[0].end], "SELECT 1");
        assert_eq!(s[1].verb, "UPDATE");
    }
    #[test] fn split_ignores_semicolons_in_opaque_text() {
        for sql in ["select ';'", "select \";\"", "select E'it\\'s;'", "select $$a;b$$", "select $f$;$f$",
                    "select 1 /* ; /* ; */ ; */", "select 1 -- ;\n"] {
            assert_eq!(split_statements(sql).len(), 1, "{sql}");
        }
    }
    #[test] fn split_empty_inputs() {
        for sql in ["", "  \n", "-- only", "/* x */", ";;"] { assert!(split_statements(sql).is_empty(), "{sql:?}"); }
    }
    #[test] fn params_found_once_in_order() { assert_eq!(find_params("select :b, :a, :b"), vec!["b", "a"]); }
    #[test] fn params_ignore_lookalikes() {
        for sql in ["select x::int", "select ':a'", "select 1 -- :a", "select /* :a */ 1", "select $$ :a $$",
                    "select $fn$ :a $fn$", "select arr[lo:hi]", "v := 1", "select \"col:a\"", "select E'\\':a'"] {
            assert!(find_params(sql).is_empty(), "{sql}");
        }
        assert_eq!(find_params("where a=:a and (b,:b) and c = (:c)"), vec!["a", "b", "c"]);
    }
    #[test] fn rewrite_numbers_and_maps_back() {
        let r = rewrite_params("x > :since and y = :lim or z = :since");
        assert_eq!(r.sql, "x > $1 and y = $2 or z = $1");
        assert_eq!(r.names, vec!["since", "lim"]);
        let rw = r.sql.find("and").unwrap();
        assert_eq!(r.original_pos(rw), "x > :since and y = :lim or z = :since".find("and").unwrap());
        assert_eq!(r.original_pos(0), 0);
    }
    #[test] fn rewrite_maps_inside_placeholder_to_name_start() {
        let r = rewrite_params("a = :abc");
        assert_eq!(r.sql, "a = $1");
        assert_eq!(r.original_pos(4), 4);
        assert_eq!(r.original_pos(5), 4);
        assert_eq!(r.original_pos(6), 8);
    }
    #[test] fn classify_reads() {
        for sql in ["select 1", "  (select 1)", "with a as (select 1) select * from a", "values (1)", "table t",
                    "show x", "explain select 1", "set x = 1", "begin", "commit", "rollback", "select * from t for update"] {
            assert_eq!(classify(sql).0, StatementKind::Read, "{sql}");
        }
    }
    #[test] fn classify_writes() {
        for sql in ["update t set a=1", "insert into t values (1)", "delete from t", "merge into t using s on true when matched then delete",
                    "truncate t", "create table t()", "drop table t", "alter table t add c int", "call p()", "do $$ begin end $$",
                    "explain analyze delete from t", "explain (analyze, costs off) update t set a = 1",
                    "with d as (delete from t returning *) select * from d", "select * into new_t from t",
                    "copy t from '/x'", "vacuum", "grant select on t to u", "frobnicate"] {
            assert_eq!(classify(sql).0, StatementKind::Write, "{sql}");
        }
    }
    #[test] fn classify_verbs() {
        assert_eq!(classify("with a as (select 1) update t set x = 1").1, "UPDATE");
        assert_eq!(classify("explain analyze update t set a = 1").1, "EXPLAIN");
        assert_eq!(classify("(select 1)").1, "SELECT");
    }
    #[test] fn leading_keyword_skips_comments_and_parens() {
        assert_eq!(leading_keyword("-- c\n /* d */ ((with a as (select 1) select 1))").as_deref(), Some("WITH"));
        assert_eq!(leading_keyword("select 1").as_deref(), Some("SELECT"));
        assert_eq!(leading_keyword("  "), None);
    }
    #[test] fn classify_with_variants() {
        assert_eq!(classify("with recursive r(n) as materialized (select 1), b as not materialized (select 2) select 1").0, StatementKind::Read);
        assert_eq!(classify("with a as (select 1)").0, StatementKind::Write);
        assert_eq!(classify("explain analyze select 1").0, StatementKind::Read);
    }
    #[test] fn utf16_index_roundtrip() {
        let ix = Utf16Index::new("a😀b");
        assert_eq!(ix.to_utf16(2), 3);
        assert_eq!(ix.to_char(3), 2);
        assert_eq!(ix.to_utf16(3), 4);
    }
    #[test] fn analyze_reports_utf16_spans() {
        let a = analyze("select '😀';\nupdate t set a = :v");
        assert_eq!((a.statements[0].from, a.statements[0].to), (0, 11)); // `select '😀'` = 10 chars, 11 UTF-16 units
        assert_eq!(a.statements[1].kind, StatementKind::Write);
        assert_eq!(a.params, vec!["v"]);
    }

    // --- SQLite ---
    fn lite_texts(sql: &str) -> Vec<String> {
        let chars: Vec<char> = sql.chars().collect();
        split_statements_in(SqlDialect::Sqlite, sql).iter().map(|s| chars[s.start..s.end].iter().collect()).collect()
    }
    #[test] fn sqlite_trigger_body_is_one_statement() {
        let t = lite_texts("create trigger t after insert on a begin insert into b values (1); update c set x = case when y then 1 else 2 end; end; select 1;");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], "create trigger t after insert on a begin insert into b values (1); update c set x = case when y then 1 else 2 end; end");
        assert_eq!(t[1], "select 1");
        let s = split_statements_in(SqlDialect::Sqlite, "create temp trigger t after insert on a begin select 1; end; select 2");
        assert_eq!((s.len(), s[0].verb.as_str()), (2, "CREATE"));
    }
    #[test] fn sqlite_begin_transaction_is_its_own_statement() {
        assert_eq!(lite_texts("begin immediate; update a set x = 1; commit;"), vec!["begin immediate", "update a set x = 1", "commit"]);
        assert_eq!(lite_texts("begin; select 1; end;").len(), 3);
        assert_eq!(lite_texts("begin transaction; select 1;").len(), 2);
    }
    #[test] fn sqlite_brackets_and_backticks_hide_semicolons_and_params() {
        let sql = "select [a;b], `c:d` from t where x = :x";
        assert_eq!(split_statements_in(SqlDialect::Sqlite, sql).len(), 1);
        assert_eq!(find_params_in(SqlDialect::Sqlite, sql), vec!["x"]);
        assert_eq!(find_params_in(SqlDialect::Sqlite, "select [a:b], 'c:d', \"e:f\", :g"), vec!["g"]);
    }
    #[test] fn sqlite_rewrite_params_numbers_distinct_names() {
        let r = rewrite_params_in(SqlDialect::Sqlite, "select :a, :b, :a");
        assert_eq!(r.sql, "select ?1, ?2, ?1");
        assert_eq!(r.names, vec!["a", "b"]);
        let r = rewrite_params_in(SqlDialect::Sqlite, "select $a, @b, :a, ?7");
        assert_eq!(r.sql, "select ?1, ?2, ?1, ?3");
        assert_eq!(r.names, vec!["a", "b", "7"]);
    }
    #[test] fn sqlite_double_colon_is_not_a_cast() {
        assert_eq!(find_params_in(SqlDialect::Sqlite, "select :a::b"), vec!["a", "b"]);
        assert_eq!(find_params_in(SqlDialect::Postgres, "select :a::b"), vec!["a"]);
    }
    #[test] fn sqlite_has_no_dollar_quotes_or_e_strings() {
        assert_eq!(lite_texts("select $$a; select 1").len(), 2);
        assert_eq!(lite_texts("select e'a\\'; select 1").len(), 2);
    }
    #[test] fn sqlite_block_comments_do_not_nest() {
        let sql = "/* a /* b */ select 1; select 2";
        assert_eq!(lite_texts(sql), vec!["select 1", "select 2"]);
        assert!(split_statements_in(SqlDialect::Postgres, sql).is_empty());
    }
    #[test] fn sqlite_pragma_calls_that_set_are_writes() {
        use StatementKind::{Read, Write};
        let c = |s: &str| classify_in(SqlDialect::Sqlite, s).0;
        assert_eq!(c("pragma foreign_keys(off)"), Write);
        assert_eq!(c("pragma journal_mode(wal)"), Write);
        assert_eq!(c("pragma main.index_list(users)"), Read);
        assert_eq!(c("pragma optimize"), Write);
        assert_eq!(c("pragma user_version"), Read);
        assert_eq!(c("pragma wal_checkpoint(truncate)"), Write);
    }
    #[test] fn sqlite_pragma_classification() {
        use StatementKind::{Read, Write};
        let c = |s: &str| classify_in(SqlDialect::Sqlite, s);
        assert_eq!(c("pragma table_info(users)"), (Read, "PRAGMA".to_string()));
        assert_eq!(c("pragma journal_mode"), (Read, "PRAGMA".to_string()));
        assert_eq!(c("pragma main.table_info(users)"), (Read, "PRAGMA".to_string()));
        assert_eq!(c("pragma foreign_keys = on"), (Write, "PRAGMA".to_string()));
        assert_eq!(c("replace into a values (1)"), (Write, "REPLACE".to_string()));
        assert_eq!(c("insert or replace into a values (1)").0, Write);
        assert_eq!(c("explain query plan select 1").0, Read);
        assert_eq!(c("explain select 1").0, Read);
        assert_eq!(c("attach 'x.db' as x"), (Write, "ATTACH".to_string()));
        for v in ["detach x", "vacuum", "reindex", "analyze"] { assert_eq!(c(v).0, Write, "{v}"); }
        for v in ["begin", "commit", "end", "rollback", "savepoint s", "release s"] { assert_eq!(c(v).0, Read, "{v}"); }
        assert_eq!(c("with a as (select 1) select * from a").0, Read);
        assert_eq!(c("with a as (select 1) delete from t").0, Write);
        // Postgres is unchanged
        assert_eq!(classify("pragma x").0, Write);
    }

    // --- MySQL ---
    const MY: SqlDialect = SqlDialect::Mysql;
    fn my_texts(sql: &str) -> Vec<String> {
        let chars: Vec<char> = sql.chars().collect();
        split_statements_in(MY, sql).iter().map(|s| chars[s.start..s.end].iter().collect()).collect()
    }
    #[test] fn mysql_comments_and_strings() {
        let sql = r#"select 'a\'b;:x', "c;:y", `d;:z` # e;:w
-- f;:v
, 1--1, :real"#;
        assert_eq!(split_statements_in(MY, sql).len(), 1);
        assert_eq!(find_params_in(MY, sql), vec!["real"]);
    }
    #[test] fn mysql_variables_are_not_params() {
        let sql = "set @a := :b; select @@sql_mode, @a";
        assert_eq!(find_params_in(MY, sql), vec!["b"]);
        assert_eq!(split_statements_in(MY, sql).len(), 2);
    }
    #[test] fn mysql_rewrite_repeats_names() {
        let r = rewrite_params_in(MY, "select :a, :b, :a");
        assert_eq!(r.sql, "select ?, ?, ?");
        assert_eq!(r.names, vec!["a", "b"]);
        assert_eq!(r.bind_order, vec!["a", "b", "a"]);
        let pg = rewrite_params_in(SqlDialect::Postgres, "select :a, :b, :a");
        assert_eq!(pg.bind_order, pg.names);
        let lite = rewrite_params_in(SqlDialect::Sqlite, "select :a, :b, :a");
        assert_eq!(lite.bind_order, lite.names);
    }
    #[test] fn mysql_procedure_body_is_one_statement() {
        let t = my_texts("create procedure p() begin declare x int default 0; if x = 0 then select 1; end if; case when x then select 2; else select 3; end case; select case when 1 then 2 end; end; select 4;");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], "create procedure p() begin declare x int default 0; if x = 0 then select 1; end if; case when x then select 2; else select 3; end case; select case when 1 then 2 end; end");
        assert_eq!(t[1], "select 4");
    }
    #[test] fn mysql_delimiter_lines() {
        let t = my_texts("delimiter //\ncreate function f() returns int begin return 1; end//\ndelimiter ;\nselect f();");
        assert_eq!(t, vec!["create function f() returns int begin return 1; end", "select f()"]);
    }
    #[test] fn mysql_begin_is_its_own_statement() {
        assert_eq!(my_texts("begin; update t set a = 1; commit;"), vec!["begin", "update t set a = 1", "commit"]);
    }
    #[test] fn mysql_classification() {
        use StatementKind::{Read, Write};
        for sql in ["show tables", "describe t", "explain select 1"] {
            assert_eq!(classify_in(MY, sql).0, Read, "{sql}");
        }
        for sql in ["explain analyze delete from t", "replace into t values (1)", "call p()", "set @a = 1",
                    "select 1 into @a", "load data infile 'x' into table t", "/*!40101 SET NAMES utf8 */"] {
            assert_eq!(classify_in(MY, sql).0, Write, "{sql}");
        }
    }
    /// Under a custom DELIMITER a group may hold several statements the server runs one
    /// after the other; it is classified as a write, so the read-only guards ask first.
    #[test] fn mysql_a_group_of_several_statements_is_a_write() {
        let s = split_statements_in(MY, "DELIMITER //\nselect 1; delete from t//");
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!((s[0].kind, s[0].verb.as_str()), (StatementKind::Write, "SELECT"));
        assert_eq!(analyze_in(MY, "DELIMITER //\nselect 1; select 2//").statements[0].kind, StatementKind::Write);
        // The statement's own text, classified on its own (the session's row-cap path does).
        assert_eq!(classify_in(MY, "select 1; delete from t").0, StatementKind::Write);
        // A trailing `;` ends nothing that follows; a lone statement still reads.
        assert_eq!(classify_in(MY, "select 1;").0, StatementKind::Read);
        assert_eq!(split_statements_in(MY, "DELIMITER //\nselect 1;//")[0].kind, StatementKind::Read);
        // Postgres and SQLite keep classifying by the first statement.
        assert_eq!(classify_in(SqlDialect::Postgres, "select 1; delete from t").0, StatementKind::Read);
    }
    #[test] fn mysql_dollar_is_an_identifier_char() {
        assert_eq!(find_params_in(MY, "select $a, a$b from t where x = :x"), vec!["x"]);
    }
    // The traps around those rules.
    #[test] fn mysql_custom_delimiter_ends_words_and_any_body() {
        let t = my_texts("DELIMITER $$\ncreate procedure p() begin select 1; end$$\n  Delimiter ;\ncall p();");
        assert_eq!(t, vec!["create procedure p() begin select 1; end", "call p()"]);
        // A custom delimiter ends the statement even inside an unbalanced body; `;` does not.
        assert_eq!(my_texts("delimiter //\nselect 1; select 2//select 3"), vec!["select 1; select 2", "select 3"]);
        // A DELIMITER line ends a statement left open before it.
        assert_eq!(my_texts("select 1\ndelimiter //\nselect 2//"), vec!["select 1", "select 2"]);
        // `delimiter` mid-line, or with nothing after it, is just a word.
        assert_eq!(my_texts("select delimiter from t;\nselect a,\ndelimiter\nfrom t"), vec!["select delimiter from t", "select a,\ndelimiter\nfrom t"]);
        // Delimiters inside strings, comments and identifiers don't count.
        assert_eq!(my_texts("delimiter //\nselect '//', `//` /* // */ # //\n//"), vec!["select '//', `//`"]);
    }
    #[test] fn mysql_nested_blocks_labels_and_variables_in_bodies() {
        let sql = "create definer = 'root'@'%' procedure p() lbl:begin declare continue handler for sqlexception begin set @end = 1, $case = 2; end; while 1 do leave lbl; end while; repeat select 1; until 1 end repeat; loop leave lbl; end loop; select :x; end lbl; select 2";
        let t = my_texts(sql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[1], "select 2");
        assert_eq!(find_params_in(MY, sql), vec!["x"]);
        for head in ["create definer=root@localhost trigger t before insert on a for each row", "create or replace function f() returns int",
                     "create event e on schedule every 1 day do", "create definer = current_user() procedure p()"] {
            assert_eq!(my_texts(&format!("{head} begin select 1; end; select 2")).len(), 2, "{head}");
        }
        // Not a routine: BEGIN there is no body.
        assert_eq!(my_texts("create table t (a int) begin; select 1").len(), 2);
    }
    #[test] fn mysql_versioned_comments_are_statements_classified_by_what_is_outside() {
        let s = split_statements_in(MY, "/*!40101 SET NAMES utf8 */;\nselect 1 /*!, :x */;");
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].kind, s[0].verb.as_str()), (StatementKind::Write, ""));
        assert_eq!((s[1].kind, s[1].verb.as_str()), (StatementKind::Read, "SELECT"));
        assert!(find_params_in(MY, "select 1 /*!, :x */").is_empty());
        assert_eq!(leading_keyword_in(MY, "/*!50001 */ select 1").as_deref(), Some("SELECT"));
    }
    #[test] fn mysql_more_classification() {
        use StatementKind::{Read, Write};
        for sql in ["explain analyze format=tree select 1", "explain format=json delete from t", "desc t", "help 'x'", "use db",
                    "start transaction", "commit", "select * from t for update"] {
            assert_eq!(classify_in(MY, sql).0, Read, "{sql}");
        }
        for sql in ["explain analyze format = tree update t set a = 1", "set names utf8mb4", "lock tables t read", "unlock tables",
                    "flush privileges", "optimize table t", "analyze table t", "rename table a to b", "truncate t", "kill 5",
                    "handler t open", "do sleep(1)", "install plugin x soname 'y'", "grant select on t to u", "revoke all on t from u",
                    "repair table t", "select 1 into outfile '/tmp/x'", "select 1 into dumpfile '/tmp/x'"] {
            assert_eq!(classify_in(MY, sql).0, Write, "{sql}");
        }
        assert_eq!(classify_in(MY, "describe t").1, "DESCRIBE");
    }
    #[test] fn mysql_reset_and_start_classification() {
        use StatementKind::{Read, Write};
        let c = |s: &str| classify_in(MY, s).0;
        assert_eq!(c("reset master"), Write);
        assert_eq!(c("reset replica"), Write);
        assert_eq!(c("start replica"), Write);
        assert_eq!(c("start transaction"), Read);
        assert_eq!(c("start transaction read only"), Read);
    }
    #[test] fn mysql_lexing_details() {
        // `--` without a space is two minus signs; `--` at the very end is a comment.
        assert_eq!(my_texts("select 1--1;select 2 --"), vec!["select 1--1", "select 2"]);
        // `/* */` doesn't nest; `::` is no cast; a label's colon is no parameter.
        assert_eq!(my_texts("/* a /* b */ select 1"), vec!["select 1"]);
        assert_eq!(find_params_in(MY, "select :a::b, x:y, `l`:z"), vec!["a", "b"]);
        // `$name` and `?NNN` are not parameters; `"…"` and `'…'` take doubled quotes too.
        assert!(find_params_in(MY, "select $a, ?1, 'it''s :a', \"x\"\":b\"").is_empty());
        let r = rewrite_params_in(MY, "a = :abc and b = :d");
        assert_eq!(r.sql, "a = ? and b = ?");
        assert_eq!((r.original_pos(4), r.original_pos(5), r.original_pos(10)), (4, 8, 13));
    }
}
