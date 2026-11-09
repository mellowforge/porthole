//! Tokens of the mongosh subset. Offsets are char offsets into the text.

use super::ParseError;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Comma,
    Colon,
    Dot,
    Semi,
    /// A name, including a leading `$`.
    Ident(String),
    /// A string literal, escapes decoded.
    Str(String),
    /// A number as written, sign included: `-5`, `0x10`, `1e3`, `-Infinity`.
    Num(String),
    /// `/pattern/flags`, the pattern as written.
    Regex {
        pattern: String,
        flags: String,
    },
    /// `:name` where a value is expected.
    Param(String),
    /// One or more line breaks between two tokens (for the statement splitter).
    Newline,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: Tok,
    pub start: usize,
    pub end: usize,
}

/// Splits `text` into tokens, skipping whitespace and comments.
pub fn tokenize(text: &str) -> Result<Vec<Token>, ParseError> {
    let mut lexer = Lexer::new(text);
    lexer.run().map_err(|(e, _)| e)?;
    Ok(lexer.tokens)
}

/// The tokens before the first one that doesn't lex, and where that one starts (a comment's or
/// a string's opening character, say). Never fails.
pub fn tokenize_lenient(text: &str) -> (Vec<Token>, Option<usize>) {
    let mut lexer = Lexer::new(text);
    let bad = lexer.run().err().map(|(_, at)| at);
    (lexer.tokens, bad)
}

fn ident_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphabetic()
}

fn ident_char(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphanumeric()
}

fn param_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn param_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

fn digit(c: char) -> bool {
    c.is_ascii_digit()
}

fn line_break(c: char) -> bool {
    c == '\n' || c == '\r'
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    tokens: Vec<Token>,
}

impl Lexer {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            pos: 0,
            tokens: Vec::new(),
        }
    }

    fn at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn take_while(&mut self, keep: impl Fn(char) -> bool) -> String {
        let start = self.pos;
        while self.at(0).is_some_and(&keep) {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    /// Whether `word` starts `ahead` chars from here and isn't part of a longer name.
    fn word_at(&self, ahead: usize, word: &str) -> bool {
        let n = word.chars().count();
        word.chars()
            .enumerate()
            .all(|(i, c)| self.at(ahead + i) == Some(c))
            && !self.at(ahead + n).is_some_and(ident_char)
    }

    /// Lexes to the end. A failure carries the offset where the failing token (or comment)
    /// starts.
    fn run(&mut self) -> Result<(), (ParseError, usize)> {
        loop {
            let newline = self.skip_blank().map_err(|e| {
                let at = e.at;
                (e, at)
            })?;
            let Some(c) = self.at(0) else {
                return Ok(());
            };
            if let Some(at) = newline
                && !self.tokens.is_empty()
            {
                self.tokens.push(Token {
                    kind: Tok::Newline,
                    start: at,
                    end: at + 1,
                });
            }
            let start = self.pos;
            let kind = self.token(c, start).map_err(|e| (e, start))?;
            self.tokens.push(Token {
                kind,
                start,
                end: self.pos,
            });
        }
    }

    /// Skips whitespace and comments. Returns the offset of the first line break skipped.
    fn skip_blank(&mut self) -> Result<Option<usize>, ParseError> {
        let mut newline = None;
        while let Some(c) = self.at(0) {
            if line_break(c) {
                newline.get_or_insert(self.pos);
                self.pos += 1;
            } else if c.is_whitespace() {
                self.pos += 1;
            } else if c == '/' && self.at(1) == Some('/') {
                while self.at(0).is_some_and(|c| !line_break(c)) {
                    self.pos += 1;
                }
            } else if c == '/' && self.at(1) == Some('*') {
                let start = self.pos;
                self.pos += 2;
                loop {
                    match self.at(0) {
                        None => return Err(ParseError::new("Unclosed comment.", start)),
                        Some('*') if self.at(1) == Some('/') => {
                            self.pos += 2;
                            break;
                        }
                        Some(c) => {
                            if line_break(c) {
                                newline.get_or_insert(self.pos);
                            }
                            self.pos += 1;
                        }
                    }
                }
            } else {
                break;
            }
        }
        Ok(newline)
    }

    /// After `:`, `,`, `(`, `[` or at the start, where a `:name` is a parameter.
    fn value_position(&self) -> bool {
        self.tokens
            .iter()
            .rev()
            .find(|t| t.kind != Tok::Newline)
            .is_none_or(|t| {
                matches!(
                    t.kind,
                    Tok::Colon | Tok::Comma | Tok::LParen | Tok::LBracket
                )
            })
    }

    fn token(&mut self, c: char, start: usize) -> Result<Tok, ParseError> {
        let punct = match c {
            '{' => Some(Tok::LBrace),
            '}' => Some(Tok::RBrace),
            '[' => Some(Tok::LBracket),
            ']' => Some(Tok::RBracket),
            '(' => Some(Tok::LParen),
            ')' => Some(Tok::RParen),
            ',' => Some(Tok::Comma),
            ';' => Some(Tok::Semi),
            _ => None,
        };
        if let Some(t) = punct {
            self.pos += 1;
            return Ok(t);
        }
        match c {
            ':' => {
                let param = self.value_position() && self.at(1).is_some_and(param_start);
                self.pos += 1;
                Ok(if param {
                    Tok::Param(self.take_while(param_char))
                } else {
                    Tok::Colon
                })
            }
            '.' if !self.at(1).is_some_and(digit) => {
                self.pos += 1;
                Ok(Tok::Dot)
            }
            '"' | '\'' => self.string(c, start),
            '/' => self.regex(start),
            '-' | '.' | '0'..='9' => self.number(start),
            c if ident_start(c) => Ok(Tok::Ident(self.take_while(ident_char))),
            c => Err(ParseError::new(
                format!("Unexpected character \"{c}\"."),
                start,
            )),
        }
    }

    fn number(&mut self, start: usize) -> Result<Tok, ParseError> {
        let invalid = || ParseError::new("Invalid number.", start);
        if self.at(0) == Some('-') {
            let digits_next = self.at(1).is_some_and(digit)
                || (self.at(1) == Some('.') && self.at(2).is_some_and(digit));
            if !digits_next {
                if self.word_at(1, "Infinity") {
                    self.pos += 1 + "Infinity".len();
                    return Ok(Tok::Num("-Infinity".into()));
                }
                return Err(ParseError::new("Unexpected character \"-\".", start));
            }
            self.pos += 1;
        }
        if self.at(0) == Some('0') && matches!(self.at(1), Some('x' | 'X')) {
            self.pos += 2;
            if self.take_while(|c| c.is_ascii_hexdigit()).is_empty() {
                return Err(invalid());
            }
        } else {
            let whole = self.take_while(digit);
            if whole.len() > 1 && whole.starts_with('0') {
                return Err(ParseError::new("Numbers can't start with 0.", start));
            }
            if self.at(0) == Some('.') && self.at(1).is_some_and(digit) {
                self.pos += 1;
                self.take_while(digit);
            }
            if matches!(self.at(0), Some('e' | 'E')) {
                let sign = usize::from(matches!(self.at(1), Some('+' | '-')));
                if self.at(1 + sign).is_some_and(digit) {
                    self.pos += 1 + sign;
                    self.take_while(digit);
                }
            }
        }
        // `1abc`, `1e`, `0x1g`: letters straight after a number.
        if self.at(0).is_some_and(ident_char) {
            return Err(invalid());
        }
        Ok(Tok::Num(self.chars[start..self.pos].iter().collect()))
    }

    fn string(&mut self, quote: char, start: usize) -> Result<Tok, ParseError> {
        let unclosed = || ParseError::new("Unclosed string.", start);
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.at(0) {
                None => return Err(unclosed()),
                Some(c) if c == quote => {
                    self.pos += 1;
                    return Ok(Tok::Str(out));
                }
                Some(c) if line_break(c) => return Err(unclosed()),
                Some('\\') => {
                    if self.at(1).is_none() {
                        return Err(unclosed());
                    }
                    self.escape(&mut out)?;
                }
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// One JS escape, from the backslash (which is followed by a char).
    fn escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        let at = self.pos;
        let bad = || ParseError::new("Invalid escape.", at);
        let c = self.chars[self.pos + 1];
        self.pos += 2;
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !self.at(0).is_some_and(digit) => out.push('\0'),
            // A legacy octal escape.
            '0'..='9' => return Err(bad()),
            // A line continuation adds nothing.
            '\r' => {
                if self.at(0) == Some('\n') {
                    self.pos += 1;
                }
            }
            '\n' | '\u{2028}' | '\u{2029}' => {}
            'x' => {
                let n = self.hex(2).ok_or_else(bad)?;
                out.push(char::from_u32(n).ok_or_else(bad)?);
            }
            'u' => out.push(self.unicode().ok_or_else(bad)?),
            c => out.push(c),
        }
        Ok(())
    }

    /// Exactly `n` hex digits.
    fn hex(&mut self, n: usize) -> Option<u32> {
        let text: String = (0..n).map(|i| self.at(i)).collect::<Option<_>>()?;
        if !text.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        self.pos += n;
        u32::from_str_radix(&text, 16).ok()
    }

    /// The rest of `\u{H…}` or `\uXXXX`, a surrogate pair written as two escapes.
    fn unicode(&mut self) -> Option<char> {
        if self.at(0) == Some('{') {
            self.pos += 1;
            let text = self.take_while(|c| c.is_ascii_hexdigit());
            if self.at(0) != Some('}') || text.is_empty() || text.len() > 6 {
                return None;
            }
            self.pos += 1;
            return char::from_u32(u32::from_str_radix(&text, 16).ok()?);
        }
        let high = self.hex(4)?;
        if !(0xD800..0xDC00).contains(&high) {
            return char::from_u32(high);
        }
        if self.at(0) != Some('\\') || self.at(1) != Some('u') {
            return None;
        }
        self.pos += 2;
        let low = self.hex(4)?;
        if !(0xDC00..0xE000).contains(&low) {
            return None;
        }
        char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00))
    }

    fn regex(&mut self, start: usize) -> Result<Tok, ParseError> {
        let unclosed = || ParseError::new("Unclosed regex.", start);
        self.pos += 1;
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            let c = match self.at(0) {
                Some(c) if !line_break(c) => c,
                _ => return Err(unclosed()),
            };
            match c {
                '\\' => {
                    let next = self
                        .at(1)
                        .filter(|c| !line_break(*c))
                        .ok_or_else(unclosed)?;
                    pattern.push(c);
                    pattern.push(next);
                    self.pos += 2;
                    continue;
                }
                '/' if !in_class => {
                    self.pos += 1;
                    break;
                }
                '[' => in_class = true,
                ']' => in_class = false,
                _ => {}
            }
            pattern.push(c);
            self.pos += 1;
        }
        let flags = self.take_while(ident_char);
        Ok(Tok::Regex { pattern, flags })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<Tok> {
        tokenize(text)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    fn s(v: &str) -> Tok {
        Tok::Str(v.into())
    }

    #[test]
    fn strings_decode_escapes() {
        assert_eq!(
            kinds(r#""a\"b\\c\n\t\r\b\f\v\0\/é\'" 'it\'s' "😀\u{1F600}\x41""#),
            vec![
                s("a\"b\\c\n\t\r\u{8}\u{c}\u{b}\0/é'"),
                s("it's"),
                s("😀😀A")
            ]
        );
        assert_eq!(kinds("\"a\\\nb\""), vec![s("ab")]);
        let err = tokenize(r#"{ a: "open"#).unwrap_err();
        assert_eq!(
            err,
            crate::mongoquery::ParseError::new("Unclosed string.", 5)
        );
        assert_eq!(
            tokenize("\"a\nb\"").unwrap_err().message,
            "Unclosed string."
        );
        assert_eq!(
            tokenize(r#""\ud800""#).unwrap_err().message,
            "Invalid escape."
        );
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(
            kinds("1 // one\n/* two\nlines */ 2 /* three */ 3"),
            vec![
                Tok::Num("1".into()),
                Tok::Newline,
                Tok::Num("2".into()),
                Tok::Num("3".into())
            ]
        );
        assert_eq!(
            tokenize("1 /* open").unwrap_err().message,
            "Unclosed comment."
        );
    }

    #[test]
    fn newlines_only_between_tokens() {
        assert_eq!(
            kinds("\n\n a \n\n b \n"),
            vec![Tok::Ident("a".into()), Tok::Newline, Tok::Ident("b".into())]
        );
    }

    #[test]
    fn regex_after_colon() {
        assert_eq!(
            kinds(r"{ a: /^a\/b[/]c/mi }"),
            vec![
                Tok::LBrace,
                Tok::Ident("a".into()),
                Tok::Colon,
                Tok::Regex {
                    pattern: r"^a\/b[/]c".into(),
                    flags: "mi".into()
                },
                Tok::RBrace
            ]
        );
        assert_eq!(tokenize("/open").unwrap_err().message, "Unclosed regex.");
    }

    #[test]
    fn param_after_colon_comma_paren_bracket() {
        assert_eq!(
            kinds("{ a: :x, b: [:y, :z], $c::w } f(:v) :s"),
            vec![
                Tok::LBrace,
                Tok::Ident("a".into()),
                Tok::Colon,
                Tok::Param("x".into()),
                Tok::Comma,
                Tok::Ident("b".into()),
                Tok::Colon,
                Tok::LBracket,
                Tok::Param("y".into()),
                Tok::Comma,
                Tok::Param("z".into()),
                Tok::RBracket,
                Tok::Comma,
                Tok::Ident("$c".into()),
                Tok::Colon,
                Tok::Param("w".into()),
                Tok::RBrace,
                Tok::Ident("f".into()),
                Tok::LParen,
                Tok::Param("v".into()),
                Tok::RParen,
                Tok::Colon,
                Tok::Ident("s".into()),
            ]
        );
        // A key's colon is punctuation, even with no space before the value.
        assert_eq!(
            kinds("{a:b}"),
            vec![
                Tok::LBrace,
                Tok::Ident("a".into()),
                Tok::Colon,
                Tok::Ident("b".into()),
                Tok::RBrace
            ]
        );
        // At the start of the text (a parameter's own value, `parse_value`).
        assert_eq!(kinds(":x"), vec![Tok::Param("x".into())]);
    }

    #[test]
    fn param_inside_a_string_is_text() {
        assert_eq!(
            kinds(r#"{ a: ":x" }"#),
            vec![
                Tok::LBrace,
                Tok::Ident("a".into()),
                Tok::Colon,
                s(":x"),
                Tok::RBrace
            ]
        );
    }

    #[test]
    fn numbers_keep_their_text() {
        assert_eq!(
            kinds("5 -5 5.0 -0.5 .5 1e3 1E-7 0x1F -0x10 -Infinity -0 9007199254740993"),
            [
                "5",
                "-5",
                "5.0",
                "-0.5",
                ".5",
                "1e3",
                "1E-7",
                "0x1F",
                "-0x10",
                "-Infinity",
                "-0",
                "9007199254740993"
            ]
            .map(|n| Tok::Num(n.into()))
            .to_vec()
        );
        assert_eq!(tokenize("1abc").unwrap_err().message, "Invalid number.");
        assert_eq!(tokenize("0x").unwrap_err().message, "Invalid number.");
        assert_eq!(
            tokenize("010").unwrap_err().message,
            "Numbers can't start with 0."
        );
        assert_eq!(
            tokenize("- 5").unwrap_err().message,
            r#"Unexpected character "-"."#
        );
    }

    #[test]
    fn punctuation_and_identifiers() {
        assert_eq!(
            kinds("db.c[\"x\"].find();$set _id ünï"),
            vec![
                Tok::Ident("db".into()),
                Tok::Dot,
                Tok::Ident("c".into()),
                Tok::LBracket,
                s("x"),
                Tok::RBracket,
                Tok::Dot,
                Tok::Ident("find".into()),
                Tok::LParen,
                Tok::RParen,
                Tok::Semi,
                Tok::Ident("$set".into()),
                Tok::Ident("_id".into()),
                Tok::Ident("ünï".into()),
            ]
        );
        assert_eq!(
            tokenize("a @").unwrap_err(),
            crate::mongoquery::ParseError::new(r#"Unexpected character "@"."#, 2)
        );
    }

    #[test]
    fn lenient_tokens_stop_where_lexing_fails() {
        let (tokens, bad) = tokenize_lenient("a \"x\\u12\" b");
        assert_eq!(
            tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
            vec![Tok::Ident("a".into())]
        );
        // The string's start, not the escape's.
        assert_eq!(bad, Some(2));
        assert_eq!(tokenize_lenient("a\n/* open").1, Some(2));
        let (tokens, bad) = tokenize_lenient("a b");
        assert_eq!((tokens.len(), bad), (2, None));
    }

    #[test]
    fn offsets_are_chars() {
        let tokens = tokenize("{ n: \"Zoë 😀\", a: }").unwrap();
        let last = tokens.last().unwrap();
        assert_eq!(last.kind, Tok::RBrace);
        assert_eq!((last.start, last.end), (17, 18));
        let name = &tokens[3];
        assert_eq!((name.start, name.end), (5, 12));
    }
}
