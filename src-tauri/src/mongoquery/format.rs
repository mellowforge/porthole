//! Format for MongoDB query tabs: the tokens of each statement reprinted with a fixed layout.
//!
//! Every token is printed as its source slice and comments are kept, so formatting never
//! changes what the text means. Layout: a bracket group prints flat when it fits the rest of
//! the line, else one element per line; a call whose last argument is a document or array
//! keeps `(` on the line and breaks only that argument; a statement that doesn't fit on one
//! line puts each call after its method call on its own line.

use super::ParseError;
use super::lex::{self, Tok, Token};
use super::split::split;

const WIDTH: usize = 80;
const INDENT: usize = 2;

/// `text` laid out at width 80 with a 2-space indent. Tokens and comments are kept as written.
pub fn format(text: &str) -> Result<String, ParseError> {
    if text.trim().is_empty() {
        return Ok(text.to_string());
    }
    let tokens: Vec<Token> = lex::tokenize(text)?
        .into_iter()
        .filter(|t| t.kind != Tok::Newline)
        .collect();
    let chars: Vec<char> = text.chars().collect();
    let statements = statements(text, &tokens);
    let gap_before = |i: usize| {
        let from = if i == 0 { 0 } else { tokens[i - 1].end };
        let to = tokens.get(i).map_or(chars.len(), |t| t.start);
        gap(&chars, from, to, i == 0)
    };

    // Inside a statement, a comment on the line of the token before it trails that token; one
    // that starts its own line (and any after it) leads the token after it.
    let mut trailing = vec![Vec::new(); tokens.len()];
    let mut leading = vec![Vec::new(); tokens.len()];
    for &(from, to) in &statements {
        for i in from + 1..to {
            let mut comments = gap_before(i).comments;
            let own = comments.iter().position(|c| c.own_line).unwrap_or(comments.len());
            leading[i] = comments.split_off(own);
            trailing[i - 1] = comments;
        }
    }
    let mut p = Printer {
        chars: &chars,
        tokens: &tokens,
        trailing,
        leading,
        out: String::new(),
        col: 0,
        line_indent: 0,
        prev: None,
        line_start: true,
        after_block: false,
        need_break: false,
    };
    for &(from, to) in &statements {
        let g = gap_before(from);
        p.gap_comments(&g.comments);
        if !p.out.is_empty() {
            p.newline(0);
            if g.blank_after {
                p.newline(0);
            }
        }
        p.statement(from, to);
    }
    p.gap_comments(&gap_before(tokens.len()).comments);
    let mut out = p.out.trim_end().to_string();
    if text.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Each statement's tokens as a `[from, to)` range of `tokens`. The bounds come from `split`;
/// a `;` between two statements goes with the one before it.
fn statements(text: &str, tokens: &[Token]) -> Vec<(usize, usize)> {
    let spans = split(text);
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut span = 0;
    let mut current = None;
    for (i, t) in tokens.iter().enumerate() {
        while span < spans.len() && spans[span].end <= t.start {
            span += 1;
        }
        let inside = span < spans.len() && spans[span].start <= t.start;
        if (inside && current != Some(span)) || out.is_empty() {
            out.push((i, i));
        }
        if inside {
            current = Some(span);
        }
        out.last_mut().expect("pushed above").1 = i + 1;
    }
    out
}

#[derive(Debug, Clone)]
struct Comment {
    text: String,
    line: bool,
    /// A line break comes before it (or it starts the text).
    own_line: bool,
    /// A blank line comes before it.
    blank_before: bool,
}

/// The comments between two tokens, and whether a blank line comes after the last of them.
struct Gap {
    comments: Vec<Comment>,
    blank_after: bool,
}

/// Reads the comments in `chars[from..to]`, which holds only whitespace and comments.
fn gap(chars: &[char], from: usize, to: usize, leading: bool) -> Gap {
    let mut comments = Vec::new();
    let mut breaks = 0;
    let mut own_line = leading;
    let mut i = from;
    while i < to {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\n' || (c == '\r' && next != Some('\n')) {
            breaks += 1;
            own_line = true;
            i += 1;
        } else if c == '/' && (next == Some('/') || next == Some('*')) {
            let start = i;
            let line = next == Some('/');
            if line {
                while i < to && !matches!(chars[i], '\n' | '\r') {
                    i += 1;
                }
            } else {
                i += 2;
                while i + 1 < to && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(to);
            }
            let text: String = chars[start..i].iter().collect();
            comments.push(Comment {
                text: if line { text.trim_end().to_string() } else { text },
                line,
                own_line,
                blank_before: breaks >= 2,
            });
            breaks = 0;
            own_line = false;
        } else {
            i += 1;
        }
    }
    Gap {
        comments,
        blank_after: breaks >= 2,
    }
}

enum Node {
    Tok(usize),
    Group(Group),
}

/// A `{}`, `[]` or `()` group: its opening token, its comma-separated elements, and its
/// closing token (any closer, as `split` counts depth; none at the end of an unclosed one).
struct Group {
    open: usize,
    elems: Vec<Elem>,
    close: Option<usize>,
    last: usize,
}

struct Elem {
    items: Vec<Node>,
    comma: Option<usize>,
}

impl Node {
    fn first(&self) -> usize {
        match self {
            Node::Tok(i) => *i,
            Node::Group(g) => g.open,
        }
    }

    fn last(&self) -> usize {
        match self {
            Node::Tok(i) => *i,
            Node::Group(g) => g.last,
        }
    }
}

fn opener(t: &Tok) -> bool {
    matches!(t, Tok::LBrace | Tok::LBracket | Tok::LParen)
}

fn closer(t: &Tok) -> bool {
    matches!(t, Tok::RBrace | Tok::RBracket | Tok::RParen)
}

/// The nodes of `tokens[*pos..end]`; a closer outside any group is a plain token.
fn nodes(tokens: &[Token], pos: &mut usize, end: usize) -> Vec<Node> {
    let mut out = Vec::new();
    while *pos < end {
        if opener(&tokens[*pos].kind) {
            out.push(Node::Group(group(tokens, pos, end)));
        } else {
            out.push(Node::Tok(*pos));
            *pos += 1;
        }
    }
    out
}

fn group(tokens: &[Token], pos: &mut usize, end: usize) -> Group {
    let open = *pos;
    *pos += 1;
    let mut elems = Vec::new();
    let mut items = Vec::new();
    while *pos < end {
        let kind = &tokens[*pos].kind;
        if opener(kind) {
            items.push(Node::Group(group(tokens, pos, end)));
        } else if closer(kind) {
            if !items.is_empty() {
                elems.push(Elem { items, comma: None });
            }
            let close = *pos;
            *pos += 1;
            return Group {
                open,
                elems,
                close: Some(close),
                last: close,
            };
        } else if *kind == Tok::Comma {
            elems.push(Elem {
                items: std::mem::take(&mut items),
                comma: Some(*pos),
            });
            *pos += 1;
        } else {
            items.push(Node::Tok(*pos));
            *pos += 1;
        }
    }
    if !items.is_empty() {
        elems.push(Elem { items, comma: None });
    }
    Group {
        open,
        elems,
        close: None,
        last: end - 1,
    }
}

fn word(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Ident(_) | Tok::Str(_) | Tok::Num(_) | Tok::Regex { .. } | Tok::Param(_)
    )
}

/// Whether one space goes between two adjacent tokens printed flat.
fn space(a: &Tok, b: &Tok) -> bool {
    use Tok::*;
    match (a, b) {
        (_, Semi | Comma | Colon) => false,
        (LBrace, RBrace) => false,
        (LBrace, _) | (_, RBrace) => true,
        (_, RBracket | RParen) => false,
        (Comma | Colon, _) => true,
        (LBracket | LParen, _) => false,
        // `x . 5` must not become `x.5`, which lexes as `x` and `.5`.
        (Dot, Num(n)) => n.starts_with(|c: char| c.is_ascii_digit() || c == '.'),
        (Dot, _) | (_, Dot) => false,
        (a, b) => word(a) && word(b),
    }
}

struct Printer<'a> {
    chars: &'a [char],
    tokens: &'a [Token],
    /// The comments after each token inside a statement, on its line.
    trailing: Vec<Vec<Comment>>,
    /// The comments on their own lines before each token inside a statement.
    leading: Vec<Vec<Comment>>,
    out: String,
    col: usize,
    /// The indent of the current line.
    line_indent: usize,
    prev: Option<usize>,
    line_start: bool,
    after_block: bool,
    /// A line comment ended the line: the next token starts a new one.
    need_break: bool,
}

impl Printer<'_> {
    fn slice(&self, i: usize) -> String {
        let t = &self.tokens[i];
        self.chars[t.start..t.end].iter().collect()
    }

    fn push(&mut self, s: &str) {
        match s.rfind('\n') {
            Some(at) => self.col = s[at + 1..].chars().count(),
            None => self.col += s.chars().count(),
        }
        self.out.push_str(s);
    }

    fn newline(&mut self, indent: usize) {
        let kept = self.out.trim_end_matches([' ', '\t']).len();
        self.out.truncate(kept);
        self.out.push('\n');
        self.out.push_str(&" ".repeat(indent * INDENT));
        self.col = indent * INDENT;
        self.line_indent = indent;
        self.line_start = true;
        self.after_block = false;
        self.need_break = false;
    }

    /// At the start of a line, gives it `indent` instead.
    fn reindent(&mut self, indent: usize) {
        let kept = self.out.trim_end_matches([' ', '\t']).len();
        self.out.truncate(kept);
        self.out.push_str(&" ".repeat(indent * INDENT));
        self.col = indent * INDENT;
        self.line_indent = indent;
    }

    /// The comments on their own lines before token `i`, at the indent of the line `i` starts
    /// (`indent` when it is mid-line); `i` then starts a new line at that indent.
    fn leading_comments(&mut self, i: usize, indent: usize) {
        let comments = std::mem::take(&mut self.leading[i]);
        if comments.is_empty() {
            return;
        }
        if !self.line_start {
            self.newline(indent);
        }
        let indent = self.line_indent;
        for (k, c) in comments.iter().enumerate() {
            if k > 0 {
                if c.own_line {
                    self.newline(indent);
                } else {
                    self.push(" ");
                }
            }
            self.push(&c.text);
            self.line_start = false;
        }
        self.newline(indent);
    }

    /// Comments between statements: those on the line before stay there, the rest go on
    /// their own lines.
    fn gap_comments(&mut self, comments: &[Comment]) {
        for c in comments {
            if self.out.is_empty() {
            } else if c.own_line {
                self.newline(0);
                if c.blank_before {
                    self.newline(0);
                }
            } else {
                self.push(" ");
            }
            self.push(&c.text);
        }
    }

    /// The width of tokens `from..=to` printed flat, or `None` when a line comment, or a comment
    /// on its own line, sits among them. With `suffix`, a line comment last of all is allowed
    /// (and not counted).
    fn flat_width(&self, from: usize, to: usize, suffix: bool) -> Option<usize> {
        let mut w = 0;
        let mut after_block = false;
        for i in from..=to {
            if i > from && (after_block || space(&self.tokens[i - 1].kind, &self.tokens[i].kind)) {
                w += 1;
            }
            if i > from && !self.leading[i].is_empty() {
                return None;
            }
            w += self.tokens[i].end - self.tokens[i].start;
            after_block = false;
            let comments = &self.trailing[i];
            for (k, c) in comments.iter().enumerate() {
                if c.line {
                    if suffix && i == to && k + 1 == comments.len() {
                        break;
                    }
                    return None;
                }
                if c.text.contains(['\n', '\r']) {
                    return None;
                }
                w += 1 + c.text.chars().count();
                after_block = true;
            }
        }
        Some(w)
    }

    /// The space the next token `i` gets on this line: 0 or 1.
    fn space_before(&self, i: usize) -> usize {
        match self.prev {
            Some(p)
                if !self.line_start
                    && (self.after_block || space(&self.tokens[p].kind, &self.tokens[i].kind)) =>
            {
                1
            }
            _ => 0,
        }
    }

    fn tok(&mut self, i: usize, indent: usize) {
        self.leading_comments(i, indent);
        if self.need_break {
            self.newline(indent);
        } else if self.space_before(i) == 1 {
            self.push(" ");
        }
        let s = self.slice(i);
        self.push(&s);
        self.line_start = false;
        self.after_block = false;
        let comments = std::mem::take(&mut self.trailing[i]);
        for c in &comments {
            if self.need_break {
                self.newline(indent);
            } else {
                self.push(" ");
            }
            self.push(&c.text);
            self.line_start = false;
            if c.line {
                self.need_break = true;
            } else {
                self.after_block = true;
            }
        }
        self.trailing[i] = comments;
        self.prev = Some(i);
    }

    fn toks(&mut self, from: usize, to: usize, indent: usize) {
        for i in from..=to {
            self.tok(i, indent);
        }
    }

    fn statement(&mut self, from: usize, to: usize) {
        self.prev = None;
        let mut pos = from;
        let items = nodes(self.tokens, &mut pos, to);
        if self.flat_width(from, to - 1, true).is_some_and(|w| w <= WIDTH) {
            self.toks(from, to - 1, 0);
            return;
        }
        // The calls after the method call (`db.c.find(…)`, `db.getCollection("c").find(…)`),
        // each on its own line.
        let is = |n: &Node, want: &dyn Fn(&Tok) -> bool| match n {
            Node::Tok(i) => want(&self.tokens[*i].kind),
            Node::Group(_) => false,
        };
        let method = items.iter().enumerate().position(|(k, n)| {
            matches!(n, Node::Group(g) if self.tokens[g.open].kind == Tok::LParen)
                && !(k > 0
                    && is(&items[k - 1], &|t| {
                        matches!(t, Tok::Ident(name) if name == "getCollection")
                    }))
        });
        let links: Vec<usize> = match method {
            Some(m) => (m + 1..items.len())
                .filter(|&k| is(&items[k], &|t| *t == Tok::Dot))
                .collect(),
            None => Vec::new(),
        };
        let Some(&first) = links.first() else {
            self.items(&items, 0, 0, true);
            return;
        };
        self.items(&items[..first], 0, 0, true);
        for (n, &start) in links.iter().enumerate() {
            let end = links.get(n + 1).copied().unwrap_or(items.len());
            self.newline(1);
            self.items(&items[start..end], 1, 0, true);
        }
    }

    /// Prints `items` on the current line at `indent`; `tail` chars must follow on the line.
    /// With `top` (a statement's own items), a line a line comment ends continues one level
    /// deeper.
    fn items(&mut self, items: &[Node], indent: usize, tail: usize, top: bool) {
        let cont = indent + usize::from(top);
        for (k, node) in items.iter().enumerate() {
            match node {
                Node::Tok(i) => self.tok(*i, cont),
                Node::Group(g) => {
                    if self.need_break {
                        self.newline(cont);
                    }
                    self.leading_comments(g.open, cont);
                    let rest = self.rest_width(g, &items[k + 1..], tail);
                    self.group(g, indent, rest);
                }
            }
        }
    }

    /// What must follow group `g` on its line: the items after it up to the opening bracket of
    /// the next group (where the line can break), else up to the end and `tail`.
    fn rest_width(&self, g: &Group, after: &[Node], tail: usize) -> usize {
        let Some(next) = after.first() else {
            return tail;
        };
        let (last, tail) = match after.iter().find_map(|n| match n {
            Node::Group(h) => Some(h.open),
            Node::Tok(_) => None,
        }) {
            Some(open) => (open, 0),
            None => (after[after.len() - 1].last(), tail),
        };
        let gap = space(&self.tokens[g.last].kind, &self.tokens[next.first()].kind);
        self.flat_width(next.first(), last, true).unwrap_or(0) + usize::from(gap) + tail
    }

    fn group(&mut self, g: &Group, indent: usize, rest: usize) {
        let sp = self.space_before(g.open);
        if self
            .flat_width(g.open, g.last, true)
            .is_some_and(|w| self.col + sp + w + rest <= WIDTH)
        {
            self.toks(g.open, g.last, indent);
            return;
        }
        if let Some(last) = self.hug(g, sp) {
            self.toks(g.open, last.open - 1, indent);
            self.broken(last, indent);
            if let Some(close) = g.close {
                self.tok(close, indent);
            }
            return;
        }
        self.broken(g, indent);
    }

    /// The last argument of call group `g` when it is a `{}` or `[]` group and `(`, the
    /// arguments before it and its opening bracket fit flat on the line (a line comment after
    /// that bracket ends the line anyway).
    fn hug<'g>(&self, g: &'g Group, sp: usize) -> Option<&'g Group> {
        if self.tokens[g.open].kind != Tok::LParen {
            return None;
        }
        let Elem { items, comma: None } = g.elems.last()? else {
            return None;
        };
        let [Node::Group(last)] = items.as_slice() else {
            return None;
        };
        if !matches!(self.tokens[last.open].kind, Tok::LBrace | Tok::LBracket) {
            return None;
        }
        let w = self.flat_width(g.open, last.open, true)?;
        (self.col + sp + w <= WIDTH).then_some(last)
    }

    /// `g` with each element on its own line, one level deeper than `indent`.
    fn broken(&mut self, g: &Group, indent: usize) {
        self.tok(g.open, indent);
        for e in &g.elems {
            self.newline(indent + 1);
            self.items(&e.items, indent + 1, usize::from(e.comma.is_some()), false);
            if let Some(comma) = e.comma {
                self.tok(comma, indent + 1);
            }
        }
        if let Some(close) = g.close {
            if self.leading[close].is_empty() {
                self.newline(indent);
            } else {
                // Comments before the closer sit at the elements' indent.
                self.leading_comments(close, indent + 1);
                self.reindent(indent);
            }
            self.tok(close, indent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mongoquery::lex::{Tok, tokenize};

    fn fmt(text: &str) -> String {
        format(text).unwrap()
    }

    #[test]
    fn flat_spacing() {
        assert_eq!(
            fmt(r#"db.users.find({name:"Ann",age:{$gt:30}})"#),
            r#"db.users.find({ name: "Ann", age: { $gt: 30 } })"#
        );
        assert_eq!(fmt("db.users.find( { } , [ ] )"), "db.users.find({}, [])");
    }

    #[test]
    fn a_call_hugs_its_last_document() {
        assert_eq!(
            fmt(r#"db.users.updateMany({ active: false, "address.city": "Oslo" }, { $set: { flagged: true, reviewedBy: "someone@example.com" } })"#),
            r#"db.users.updateMany({ active: false, "address.city": "Oslo" }, {
  $set: { flagged: true, reviewedBy: "someone@example.com" }
})"#
        );
    }

    #[test]
    fn a_pipeline_breaks_one_stage_per_line() {
        assert_eq!(
            fmt(r#"db.orders.aggregate([{ $match: { status: "A" } }, { $group: { _id: "$cust", total: { $sum: "$amount" } } }, { $sort: { total: -1 } }])"#),
            r#"db.orders.aggregate([
  { $match: { status: "A" } },
  { $group: { _id: "$cust", total: { $sum: "$amount" } } },
  { $sort: { total: -1 } }
])"#
        );
    }

    #[test]
    fn a_long_chain_puts_each_call_on_its_own_line() {
        assert_eq!(
            fmt("db.users.find({ active: true }, { name: 1, email: 1 }).sort({ name: 1 }).skip(20).limit(10)"),
            "db.users.find({ active: true }, { name: 1, email: 1 })\n  .sort({ name: 1 })\n  .skip(20)\n  .limit(10)"
        );
    }

    #[test]
    fn comments_are_kept() {
        assert_eq!(
            fmt("// top\ndb.users.find({ a: 1, // why\n b: 2 })"),
            "// top\ndb.users.find({\n  a: 1, // why\n  b: 2\n})"
        );
        assert_eq!(fmt("db.users.find({ a: /* x */ 1 })"), "db.users.find({ a: /* x */ 1 })");
    }

    #[test]
    fn statements_go_on_their_own_lines() {
        assert_eq!(fmt("db.a.find();db.b.find()"), "db.a.find();\ndb.b.find()");
        assert_eq!(fmt("db.a.find()\n\n\n\ndb.b.find()\n"), "db.a.find()\n\ndb.b.find()\n");
    }

    #[test]
    fn literals_keep_their_spelling() {
        // 105 chars, so the document breaks.
        assert_eq!(
            fmt(r#"db.users.find({ _id: ObjectId("650000000000000000000001"), at: new Date("2024-01-01"), n: 5.0, m: :min })"#),
            r#"db.users.find({
  _id: ObjectId("650000000000000000000001"),
  at: new Date("2024-01-01"),
  n: 5.0,
  m: :min
})"#
        );
    }

    #[test]
    fn a_group_needs_room_only_up_to_the_next_bracket() {
        // `getCollection("users")` stays whole: the line can break at `updateOne(`.
        assert_eq!(
            fmt(r#"db.getCollection("users").updateOne({ _id: ObjectId("65a1b2c3d4e5f60718293a4b") }, { $set: { name: "Ann B" } })"#),
            r#"db.getCollection("users").updateOne(
  { _id: ObjectId("65a1b2c3d4e5f60718293a4b") },
  { $set: { name: "Ann B" } }
)"#
        );
    }

    #[test]
    fn a_line_comment_ends_its_line() {
        // On its own line it stays there, and the call still hugs the document.
        for text in [
            "db.users.find({\n  // the filter\n  a: 1\n})",
            "db.users.find({\n  a: 1,\n  // age: { $gt: 30 },\n  b: 2\n})",
            "db.users.find({\n  a: 1,\n  /* old */\n  b: 2\n})",
            "db.users.find({ a: 1 })\n  // then sort\n  .sort({ a: 1 })",
            "db.users.find({\n  a: 1\n  // the end\n})",
        ] {
            assert_eq!(fmt(text), text);
        }
        assert_eq!(fmt("db.users.find({ a: 1, // why\n /* x */ b: 2 })"), "db.users.find({\n  a: 1, // why\n  /* x */\n  b: 2\n})");
        // Outside brackets, the statement goes on one level deeper.
        assert_eq!(fmt("db.a // c\n.find()"), "db.a // c\n  .find()");
        assert_eq!(fmt("db.a.find() /* c */ ;"), "db.a.find() /* c */ ;");
    }

    #[test]
    fn whitespace_only_text_is_returned_unchanged() {
        assert_eq!(fmt(""), "");
        assert_eq!(fmt("  \n\t\n"), "  \n\t\n");
    }

    #[test]
    fn text_that_does_not_lex_is_an_error_at_the_lexers_position() {
        let text = r#"db.users.find({ name: "x"#;
        let err = format(text).unwrap_err();
        assert_eq!(err, tokenize(text).unwrap_err());
        assert_eq!(err.at, 22);
    }

    #[test]
    fn unclosed_brackets_format_without_panicking() {
        assert_eq!(fmt("db.dropDatabase("), "db.dropDatabase(");
        assert_eq!(fmt("db.users.find( ).explain("), "db.users.find().explain(");
    }

    /// The pairs above, manual check steps 105–116 (docs/manual-check.md), and statements from the query tests.
    const CORPUS: &[&str] = &[
        r#"db.users.find({name:"Ann",age:{$gt:30}})"#,
        "db.users.find( { } , [ ] )",
        r#"db.users.updateMany({ active: false, "address.city": "Oslo" }, { $set: { flagged: true, reviewedBy: "someone@example.com" } })"#,
        r#"db.orders.aggregate([{ $match: { status: "A" } }, { $group: { _id: "$cust", total: { $sum: "$amount" } } }, { $sort: { total: -1 } }])"#,
        "db.users.find({ active: true }, { name: 1, email: 1 }).sort({ name: 1 }).skip(20).limit(10)",
        "// top\ndb.users.find({ a: 1, // why\n b: 2 })",
        "db.users.find({ a: /* x */ 1 })",
        "db.a.find();db.b.find()",
        "db.a.find()\n\n\n\ndb.b.find()\n",
        r#"db.users.find({ _id: ObjectId("650000000000000000000001"), at: new Date("2024-01-01"), n: 5.0, m: :min })"#,
        // Manual check steps 105–116 (step 112's `updateOne(…)` written out as Show SQL gives it).
        "db.users.find({}, { name: 1, age: 1 }).sort({ name: 1 }).limit(3)",
        "db.big.find()",
        "db.users.find({ age: { $gt: :min } }).sort({ name: 1 })",
        r#"db.users.insertOne({ name: "Gus" })"#,
        r#"db.notes.insertMany([{ t: "a" }, { t: "b" }])"#,
        "db.users.updateMany({ active: false }, { $set: { flagged: true } })",
        r#"db.users.deleteMany({ name: "Gus" })"#,
        r#"db.users.countDocuments({ name: "Gus" })"#,
        "db.users.find()",
        r#"db.users.insertOne({ name: "Hal" })"#,
        r#"db.getCollection("users").updateOne({ _id: ObjectId("65a1b2c3d4e5f60718293a4b") }, { $set: { name: "Ann B" } })"#,
        r#"db.big.find({ $where: "sleep(100) || true" })"#,
        r#"db.big.updateMany({ $where: "sleep(100) || true" }, { $set: { s: 1 } })"#,
        "db.big.countDocuments({ s: 1 })",
        r#"db.big.distinct("_id")"#,
        r#"db.users.find({ name: "Ann B" })"#,
        "db.users.find({ age: })",
        "db.users.find({ name: \"Ann B\" })\ndb.users.find({ age: })",
        // From the query tests.
        "db.a.bulkWrite([])",
        "db.a.bulkWrite([{ insertOne: { document: {} } }], { ordered: 0 })",
        "db.a.bulkWrite([{ insertOne: { document: {} } }], { w: 1 })",
        "db.a.bulkWrite({ insertOne: {} })",
        "db.a.createIndex()",
        "db.a.createIndex({ a: 1 }, 5)",
        "db.a.createIndex({ a: 1 }, { unique: true, name: 5 })",
        "db.a.createIndex({ a: 1 }, {}, {})",
        "db.a.createIndex({ a: 1, b: true })",
        "db.a.createIndex({})",
        "db.a.drop(1)",
        "db.a.dropIndex()",
        "db.a.dropIndex(5)",
        "db.a.dropIndex(\"*\")",
        "db.a.dropIndex(\"a_1\", \"b_1\")",
        "db.a.findOneAndDelete()",
        "db.a.findOneAndDelete({}, 5)",
        "db.a.findOneAndDelete({}, { projection: 1 })",
        "db.a.findOneAndDelete({}, { sort: [1] })",
        "db.a.findOneAndDelete({}, { upsert: true })",
        "db.a.findOneAndReplace({}, 5)",
        "db.a.findOneAndReplace({}, { a: 1 }, { returnNewDocument: \"yes\" })",
        "db.a.findOneAndReplace({}, { a: 1 }, { sort: {}, w: 1 })",
        "db.a.findOneAndReplace({}, { a: 1, $set: { a: 1 } })",
        "db.a.findOneAndUpdate({}, [])",
        "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, { returnDocument: \"later\" })",
        "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, { returnDocument: true })",
        "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, { returnOriginal: false })",
        "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, { upsert: 1 })",
        "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, {}, {})",
        "db.a.findOneAndUpdate({}, { b: 1 })",
        "db.a.getIndexes({})",
        "db.a.renameCollection()",
        "db.a.renameCollection(\"b\", 1)",
        "db.a.renameCollection(\"b\", true, {})",
        "db.big.find({ $where: \"sleep(1500) || true\" }).limit(1)\ndb.users.find()",
        "db.big.find({ $where: \"sleep(2000) || true\" }).limit(1)",
        "db.big.find({ n: { $gt: 5 } }).sort({ n: -1 }).explain(\"executionStats\")",
        "db.big.find({SLOW})",
        "db.createCollection()",
        "db.createCollection(\"logs\", { capped: true, size: 4096 })",
        "db.createCollection(\"p9_logs\")",
        "db.createCollection(\"p9_ran\")\ndb.p9_ran.getIndexes()\ndb.p9_ran.drop()",
        "db.createCollection(\"p9_ro\")",
        "db.createCollection(\"x\")",
        "db.createCollection(\"x\").find()",
        "db.createCollection(\"x\", 5)",
        "db.createCollection(\"x\", {}, {})",
        "db.createCollection.find()",
        "db.dropDatabase(",
        "db.dropDatabase()",
        "db.dropDatabase.drop()",
        "db.getCollection(\"x\")",
        "db.getCollectionNames()",
        "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 1 } } }, { deleteMany: { filter: {} } }])",
        "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 2 } } }])",
        "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 2 } } }], { ordered: false })",
        "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 2 } } }])",
        "db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 3 } } }, { insertOne: { document: { _id: 1 } } }, { updateOne: { filter: { _id: 2 }, update: { $set: { a: 1 } } } }, { updateOne: { filter: { _id: 3 }, update: { $inc: { _id: 1 } } } }, { insertOne: { document: { _id: 2 } } }], { ordered: false })",
        "db.p9_bulk.bulkWrite([{ insertOne: { document: { n: 7 } } }])",
        "db.p9_bulk2.bulkWrite([{ insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 1 } } }, { deleteMany: { filter: {} } }])",
        "db.p9_bulk2.bulkWrite([{ updateOne: { filter: { _id: 1 }, update: { $set: { a: 1 } } } }, { deleteOne: { filter: { _id: 9 } } }, { insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 3 } } }])",
        "db.p9_logs.createIndex({ at: -1 })",
        "db.p9_logs.dropIndex(\"at_-1\")",
        "db.p9_logs.getIndexes()",
        "db.p9_logs.renameCollection(\"p9_logs2\")",
        "db.p9_logs2.drop()",
        "db.p9_missing.getIndexes()",
        "db.p9_new.createIndex({ a: 1 })",
        "db.p9_new.createIndex({ b: 1, c: -1 }, { name: \"bc\", unique: true })",
        "db.p9_new.dropIndex({ a: 1 })",
        "db.p9_new.getIndexes()",
        "db.p9_new.renameCollection(\"p9_logs2\")",
        "db.p9_new.renameCollection(\"p9_logs2\", true)",
        "db.runCommand({})",
        "db.users.aggregate([]).explain()",
        "db.users.aggregate([{ $match: { active: true } }, { $group: { _id: \"$active\", n: { $sum: 1 } } }]).explain()",
        "db.users.aggregate([{ $match: { active: true } }]).explain(\"executionStats\")",
        "db.users.aggregate([{ $merge: { into: \"x\" } }, { $match: {} }]).explain()",
        "db.users.aggregate([{ $out: \"x\" }]).explain(\"executionStats\")",
        "db.users.bulkWrite([{ deleteOne: { filter: { a: 1 } } }])",
        "db.users.bulkWrite([{ insertOne: { document: { a: 1 } } }])",
        "db.users.bulkWrite([{ insertOne: { document: {} } }], { ordered: true })",
        "db.users.countDocuments().explain()",
        "db.users.countDocuments({ name: \"Fay\" })",
        "db.users.createCollection()",
        "db.users.createIndex({ a: 1, b: -1 }, { name: \"ab\", sparse: true })",
        "db.users.createIndex({ email: 1 })",
        "db.users.createIndex({ email: 1 }, { unique: true })",
        "db.users.createIndex({ name: 1 })",
        "db.users.createIndex({ t: \"text\" })",
        "db.users.deleteMany({ a: x.explain() })",
        "db.users.deleteMany({}).explain()",
        "db.users.drop()",
        "db.users.dropIndex(\"email_1\")",
        "db.users.dropIndex({ email: 1 })",
        "db.users.explain().find()",
        "db.users.find()",
        "db.users.find().explain(",
        "db.users.find().explain().limit(1)",
        "db.users.find().explain(\"full\")",
        "db.users.find().explain(\"queryPlanner\")",
        "db.users.find().explain(\"queryPlanner\", 1)",
        "db.users.find().explain(true)",
        "db.users.find().limit(5).explain(\"allPlansExecution\")",
        "db.users.find({ a: 1 }).sort({ a: 1 }).explain(\"executionStats\")",
        "db.users.find({ email: \"ann@example.com\" }).explain()",
        "db.users.findOne().explain()",
        "db.users.findOneAndDelete({ a: 1 })",
        "db.users.findOneAndDelete({ a: 1 }, { sort: { a: 1 }, projection: { _id: 0 } })",
        "db.users.findOneAndDelete({ name: \"Ann\" })",
        "db.users.findOneAndDelete({ name: \"Fay\" })",
        "db.users.findOneAndReplace({ a: 1 }, { a: 2 })",
        "db.users.findOneAndReplace({ name: \"Cid\" }, { name: \"Cid\", email: \"cid@x\", replaced: true }, { returnNewDocument: true })",
        "db.users.findOneAndUpdate({ a: 1 }, { $set: { b: 2 } })",
        "db.users.findOneAndUpdate({ name: \"Ann\" }, { $set: { email: \"bob@example.com\" } })",
        "db.users.findOneAndUpdate({ name: \"Bob\" }, { $set: { age: 31 } }, { returnDocument: \"after\", projection: { name: 1, age: 1 } })",
        "db.users.findOneAndUpdate({ name: \"Bob\" }, { $set: { age: 32 } })",
        "db.users.findOneAndUpdate({ name: \"Nobody\" }, { $set: { age: 31 } }, { returnDocument: \"after\", projection: { name: 1, age: 1 } })",
        "db.users.findOneAndUpdate({ name: \"Zed\" }, { $set: { email: \"zed@x\" } }, { upsert: true })",
        "db.users.findOneAndUpdate({}, [{ $set: { b: 1 } }], { projection: { b: 1 } })",
        "db.users.findOneAndUpdate({}, { $set: { b: 1 } }, { returnDocument: \"before\" })",
        "db.users.findOneAndUpdate({}, { $set: { b: 1 } }, { returnNewDocument: false })",
        "db.users.findOneAndUpdate({}, { $set: { b: 1 } }, { returnNewDocument: true })",
        "db.users.getIndexes()",
        "db.users.renameCollection(\"people\")",
        "db.users.renameCollection(\"people\", true)",
        "db.users.watch()",
        "db.users\n  .find({ a: [1, 2] })\n  .explain()",
        // More layouts: comments between and inside statements, a long chain inside a call.
        "/* a */ /* b */\n\n// c\ndb.a.find() // d\n\n\n// e\n\ndb.b.find(); /* f */\n// g\n",
        "db.users.find({\n  // the filter\n  a: 1\n})",
        "db.a // c\n  .find()",
        "db.a.find() // c\n;",
        "db.a.find({ a: [1, 2,], b: { c: 1, }, })",
        "db.users.find({ a: /a/i, b: 0x10, c: 'x', d: -Infinity, e: 1e3, f: .5 })",
        r#"db.users.aggregate([{ $match: { status: "A", createdAt: { $gte: new Date("2024-01-01"), $lt: new Date("2025-01-01") } } }, { $lookup: { from: "orders", localField: "_id", foreignField: "userId", as: "orders" } }]).explain("executionStats")"#,
        r#"db.users.bulkWrite([{ insertOne: { document: { name: "Ann", tags: ["a", "b", "c"], address: { city: "Oslo", zip: "0150" } } } }, { updateOne: { filter: { name: "Bob" }, update: { $set: { age: 31 } }, upsert: true } }], { ordered: false })"#,
        "db.a.find(x // c\n)",
        "db.a.find({ a: // c\n 1 })",
        "db.a.find({ a: 1 } // c\n, { b: 1 })",
        "db.a.find({ a: 1, // x\n // y\n /* z */ b: 2 })",
        "db.a.find({ a: /* x\n y */ 1 })",
        ";;db.a.find();;\n;",
        "x . 5",
        "// only a comment\n",
        "db.users.find({\n  a: 1,\n  // age: { $gt: 30 },\n  b: 2\n})",
        "db.users.find({ a: 1 })\n  // then sort\n  .sort({ a: 1 })",
        "db.a.find(\n  // c\n  { a: 1 })",
        "db.a.find({\n  // only\n})",
        "db.a.find()\n// c\n;",
        "[\n  /* a */ /* b */ 1]",
        "db.a.find({a:1})\r\n\r\n\r\n// c\r\ndb.b.find()\r\n",
        // Non-ASCII text: in strings, a key, a comment, an identifier.
        r#"db.users.find({ name: "Ünïcødé ✓", "😀": 1, city: "Tromsø" }) // ünïcødé ✓ 😀"#,
        "db.users.find({ café: \"crème brûlée 😀😀😀\", /* ✓ */ naïve: true, tags: [\"ä\", \"ö\", \"ü\", \"日本語のテキストはここにあります、とても長い文字列です\"] })",
        "/* ünïcødé ✓ */ db.users.updateMany({ \"😀\": { $exists: true } }, { $set: { \"✓\": \"Ünïcødé ✓ Ünïcødé ✓ Ünïcødé ✓ Ünïcødé ✓\" } })",
        // Strings that hold comment openers, brackets and escaped quotes.
        r#"db.a.find({ url: "http://example.com/a//b", c: "/* not a comment */", d: "{ ] ) [ (", e: "say \"hi\" // x", f: 'it\'s /* y' })"#,
        r#"db.a.insertOne({ s: "}", t: "]", u: ")", v: ",", w: ";", x: ":", y: "\\", z: "\u00e9\n\t" })"#,
        // Regexes with a slash in a class, and an escaped slash.
        r#"db.a.find({ p: /[a/b]+/i, q: /x\/y/, r: /[\]/]/g, s: /^\/\/ comment-ish$/ })"#,
        // A string longer than the width.
        r#"db.a.insertOne({ note: "a very long string that runs well past the eighty columns of the line width, and then some more text" })"#,
        // A long numeric array, and a long nested pipeline (four levels).
        "db.a.insertOne({ n: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, -1.5, 2e10] })",
        r#"db.orders.aggregate([{ $match: { $and: [{ status: { $in: ["A", "B", "C"] } }, { $or: [{ qty: { $gt: 10, $lt: 100 } }, { "items.sku": { $elemMatch: { code: /^X[/-]/, n: { $gte: 1 } } } }] }] } }, { $lookup: { from: "inventory", let: { sku: "$sku" }, pipeline: [{ $match: { $expr: { $and: [{ $eq: ["$sku", "$$sku"] }, { $gt: ["$instock", { $multiply: [2, "$min"] }] }] } } }, { $project: { _id: 0, instock: 1 } }], as: "stock" } }, { $unwind: { path: "$stock", preserveNullAndEmptyArrays: true } }, { $group: { _id: { cust: "$cust", day: { $dateToString: { format: "%Y-%m-%d", date: "$at" } } }, total: { $sum: { $multiply: ["$price", "$qty"] } } } }, { $sort: { total: -1 } }])"#,
    ];

    fn kinds_and_slices(text: &str) -> (Vec<Tok>, Vec<String>) {
        let chars: Vec<char> = text.chars().collect();
        tokenize(text)
            .unwrap()
            .into_iter()
            .filter(|t| t.kind != Tok::Newline)
            .map(|t| (t.kind, chars[t.start..t.end].iter().collect()))
            .unzip()
    }

    #[test]
    fn format_keeps_every_token_and_is_idempotent() {
        for text in CORPUS {
            let once = fmt(text);
            assert_eq!(kinds_and_slices(&once), kinds_and_slices(text), "tokens of {text:?} → {once:?}");
            assert_eq!(fmt(&once), once, "format twice: {text:?}");
            assert_eq!(super::super::split(&once).len(), super::super::split(text).len(), "statements of {text:?} → {once:?}");
            assert!(once.lines().all(|l| l == l.trim_end()), "trailing whitespace in {once:?}");
            assert_eq!(once.ends_with('\n'), text.ends_with('\n'), "final line break of {text:?}");
        }
    }

    #[test]
    fn comments_are_kept_in_order() {
        // The comments are in the gaps between the lexer's tokens, which hold no string or regex.
        let comments = |s: &str| -> Vec<String> {
            let chars: Vec<char> = s.chars().collect();
            let tokens: Vec<Token> = tokenize(s).unwrap().into_iter().filter(|t| t.kind != Tok::Newline).collect();
            let mut bounds = vec![0];
            for t in &tokens {
                bounds.extend([t.start, t.end]);
            }
            bounds.push(chars.len());
            let mut out = Vec::new();
            for gap in bounds.chunks(2) {
                let gap: String = chars[gap[0]..gap[1]].iter().collect();
                let mut rest = gap.as_str();
                while let Some(i) = rest.find("/*").into_iter().chain(rest.find("//")).min() {
                    rest = &rest[i..];
                    let end = if rest.starts_with("/*") { rest.find("*/").unwrap() + 2 } else { rest.find('\n').unwrap_or(rest.len()) };
                    out.push(rest[..end].trim_end().to_string());
                    rest = &rest[end..];
                }
            }
            out
        };
        for text in CORPUS {
            assert_eq!(comments(&fmt(text)), comments(text), "comments of {text:?}");
        }
        // The scanner finds the comments, and none in a string or a regex.
        assert_eq!(comments(r#"db.a.find({ a: "// x /* y */" }) // z"#), ["// z"]);
        assert_eq!(comments("db.a.find({ a: /\\/\\// }) /* z */"), ["/* z */"]);
        let total: usize = CORPUS.iter().map(|t| comments(t).len()).sum();
        assert!(total >= 20, "{total}");
    }
}
