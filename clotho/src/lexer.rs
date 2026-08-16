//! Clotho streaming lexer.
//!
//! The lexer is single-pass and driven token-by-token: [`Lexer::next_token`]
//! consumes exactly one token from the source and advances the cursor. This
//! makes it suitable for a streaming frontend where tokens can be produced and
//! consumed concurrently with I/O.

use crate::token::{Span, Token, TokenKind};

/// A streaming lexer over a UTF-8 source slice.
pub struct Lexer<'src> {
    src: &'src str,
    bytes: &'src [u8],
    /// Byte offset of the next character to read.
    pos: usize,
    /// Current line (1-based) and column (1-based) for diagnostics.
    line: usize,
    col: usize,
}

impl<'src> Lexer<'src> {
    pub fn new(src: &'src str) -> Self {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    /// The original source slice this lexer was created from.
    pub fn source(&self) -> &'src str {
        self.src
    }

    /// Returns true if the lexer has consumed the entire source.
    pub fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// The current 1-based line and column.
    pub fn position(&self) -> (usize, usize) {
        (self.line, self.col)
    }

    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_byte_at(&self, off: usize) -> Option<u8> {
        self.bytes.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.bytes.get(self.pos).copied()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek_byte() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => {
                    self.bump();
                }
                Some(b'/') if self.peek_byte_at(1) == Some(b'/') => {
                    // line comment
                    while let Some(b) = self.peek_byte() {
                        if b == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some(b'/') if self.peek_byte_at(1) == Some(b'*') => {
                    // block comment, supports nesting
                    self.bump();
                    self.bump();
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.peek_byte() {
                            None => break,
                            Some(b'/') if self.peek_byte_at(1) == Some(b'*') => {
                                self.bump();
                                self.bump();
                                depth += 1;
                            }
                            Some(b'*') if self.peek_byte_at(1) == Some(b'/') => {
                                self.bump();
                                self.bump();
                                depth -= 1;
                            }
                            Some(_) => {
                                self.bump();
                            }
                        }
                    }
                }
                _ => break,
            }
        }
    }

    /// Produce the next token, or [`TokenKind::Eof`] at end of input.
    pub fn next_token(&mut self) -> Token {
        self.skip_trivia();
        let start = self.pos;
        let start_line = self.line;
        let start_col = self.col;

        let Some(b) = self.peek_byte() else {
            return Token {
                kind: TokenKind::Eof,
                span: Span::new(start, start),
            };
        };

        let kind = match b {
            b'0'..=b'9' => self.lex_number(),
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'@'
                if b == b'@' || b.is_ascii_alphanumeric() || b == b'_' =>
            {
                self.lex_ident_or_keyword()
            }
            b'"' => self.lex_string(),
            b'\'' => self.lex_char(),
            _ => self.lex_punct(),
        };

        // `Unknown` consumes a single byte so we always make forward progress.
        if matches!(kind, TokenKind::Unknown(_)) {
            self.bump();
        }

        let end = self.pos;
        let _ = (start_line, start_col);
        Token {
            kind,
            span: Span::new(start, end),
        }
    }

    fn lex_ident_or_keyword(&mut self) -> TokenKind {
        // Raw identifier: r#ident
        if self.peek_byte() == Some(b'r') && self.peek_byte_at(1) == Some(b'#') {
            self.bump(); // r
            self.bump(); // #
            let name = self.lex_ident_body();
            return TokenKind::Ident(name);
        }
        let name = self.lex_ident_body();
        keyword(&name).unwrap_or(TokenKind::Ident(name))
    }

    fn lex_ident_body(&mut self) -> String {
        let mut s = String::new();
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                s.push(b as char);
                self.bump();
            } else {
                break;
            }
        }
        s
    }

    fn lex_number(&mut self) -> TokenKind {
        let start = self.pos;
        let base = if self.peek_byte() == Some(b'0') {
            match self.peek_byte_at(1) {
                Some(b'x' | b'X') => {
                    self.bump();
                    self.bump();
                    Some(16)
                }
                Some(b'b' | b'B') => {
                    self.bump();
                    self.bump();
                    Some(2)
                }
                Some(b'o' | b'O') => {
                    self.bump();
                    self.bump();
                    Some(8)
                }
                _ => None,
            }
        } else {
            None
        };

        let mut digits = String::new();
        let mut is_float = false;
        loop {
            match self.peek_byte() {
                Some(b'_') => {
                    self.bump();
                }
                Some(b'.')
                    if base.is_none()
                        && self.peek_byte_at(1).is_some_and(|c| c.is_ascii_digit()) =>
                {
                    // decimal point of a float
                    is_float = true;
                    self.bump();
                    digits.push('.');
                }
                Some(b'e' | b'E') if base.is_none() => {
                    is_float = true;
                    self.bump();
                    digits.push('e');
                    if self.peek_byte() == Some(b'+') || self.peek_byte() == Some(b'-') {
                        digits.push(self.bump().unwrap() as char);
                    }
                }
                Some(b) if (b.is_ascii_digit() || (base == Some(16) && b.is_ascii_hexdigit())) => {
                    digits.push(b as char);
                    self.bump();
                }
                Some(b) if b.is_ascii_alphabetic() => {
                    // type suffix
                    let suffix = self.lex_ident_body();
                    if is_float {
                        return TokenKind::Float {
                            text: digits,
                            suffix: Some(suffix),
                        };
                    }
                    return TokenKind::Int {
                        value: parse_int(&digits, base),
                        suffix: Some(suffix),
                    };
                }
                _ => break,
            }
        }

        let _ = start;
        if is_float {
            TokenKind::Float {
                text: digits,
                suffix: None,
            }
        } else {
            TokenKind::Int {
                value: parse_int(&digits, base),
                suffix: None,
            }
        }
    }

    fn lex_string(&mut self) -> TokenKind {
        self.bump(); // opening quote
        let mut s = String::new();
        loop {
            match self.bump() {
                None => break,
                Some(b'"') => break,
                Some(b'\\') => match self.bump() {
                    Some(b'n') => s.push('\n'),
                    Some(b't') => s.push('\t'),
                    Some(b'r') => s.push('\r'),
                    Some(b'\\') => s.push('\\'),
                    Some(b'\'') => s.push('\''),
                    Some(b'"') => s.push('"'),
                    Some(b'0') => s.push('\0'),
                    Some(b'x') => {
                        let h = self.read_hex_escape(2);
                        s.push(char::from_u32(h).unwrap_or('\u{FFFD}'));
                    }
                    Some(b'u') => {
                        let c = self.read_unicode_escape();
                        s.push(c);
                    }
                    Some(other) => s.push(other as char),
                    None => break,
                },
                Some(other) => s.push(other as char),
            }
        }
        TokenKind::Str(s)
    }

    fn lex_char(&mut self) -> TokenKind {
        self.bump(); // opening quote
        let c = match self.bump() {
            None => '\0',
            Some(b'\\') => match self.bump() {
                Some(b'n') => '\n',
                Some(b't') => '\t',
                Some(b'r') => '\r',
                Some(b'\\') => '\\',
                Some(b'\'') => '\'',
                Some(b'"') => '"',
                Some(b'0') => '\0',
                Some(b'x') => char::from_u32(self.read_hex_escape(2)).unwrap_or('\u{FFFD}'),
                Some(b'u') => self.read_unicode_escape(),
                Some(other) => other as char,
                None => '\0',
            },
            Some(other) => other as char,
        };
        // consume closing quote if present
        if self.peek_byte() == Some(b'\'') {
            self.bump();
        }
        TokenKind::Char(c)
    }

    fn read_hex_escape(&mut self, count: usize) -> u32 {
        let mut v = 0u32;
        for _ in 0..count {
            match self.bump() {
                Some(b) if b.is_ascii_hexdigit() => {
                    v = v * 16 + b.to_ascii_lowercase() as u32
                        - (if b.is_ascii_digit() { b'0' } else { b'a' - 10 }) as u32;
                }
                _ => break,
            }
        }
        v
    }

    fn read_unicode_escape(&mut self) -> char {
        // \u{...}
        if self.peek_byte() == Some(b'{') {
            self.bump();
            let mut v = 0u32;
            while let Some(b) = self.peek_byte() {
                if b == b'}' {
                    self.bump();
                    break;
                }
                if b.is_ascii_hexdigit() {
                    v = v * 16 + b.to_ascii_lowercase() as u32
                        - (if b.is_ascii_digit() { b'0' } else { b'a' - 10 }) as u32;
                    self.bump();
                } else {
                    break;
                }
            }
            char::from_u32(v).unwrap_or('\u{FFFD}')
        } else {
            '\u{FFFD}'
        }
    }

    fn lex_punct(&mut self) -> TokenKind {
        let a = self.peek_byte().unwrap();
        let b = self.peek_byte_at(1);
        let c = self.peek_byte_at(2);

        macro_rules! one {
            ($k:expr) => {{
                self.bump();
                $k
            }};
        }
        macro_rules! two {
            ($k:expr) => {{
                self.bump();
                self.bump();
                $k
            }};
        }
        macro_rules! three {
            ($k:expr) => {{
                self.bump();
                self.bump();
                self.bump();
                $k
            }};
        }

        match a {
            b'(' => one!(TokenKind::LParen),
            b')' => one!(TokenKind::RParen),
            b'{' => one!(TokenKind::LBrace),
            b'}' => one!(TokenKind::RBrace),
            b'[' => one!(TokenKind::LBracket),
            b']' => one!(TokenKind::RBracket),
            b',' => one!(TokenKind::Comma),
            b';' => one!(TokenKind::Semicolon),
            b'@' => one!(TokenKind::At),
            b'#' => one!(TokenKind::Hash),
            b'$' => one!(TokenKind::Dollar),
            b'?' => one!(TokenKind::Question),
            b'.' if b == Some(b'.') => two!(TokenKind::DotDot),
            b'.' => one!(TokenKind::Dot),
            b':' if b == Some(b':') => two!(TokenKind::ColonColon),
            b':' => one!(TokenKind::Colon),
            b'-' if b == Some(b'>') => two!(TokenKind::Arrow),
            b'-' if b == Some(b'=') => two!(TokenKind::MinusEq),
            b'-' => one!(TokenKind::Minus),
            b'+' if b == Some(b'=') => two!(TokenKind::PlusEq),
            b'+' => one!(TokenKind::Plus),
            b'*' if b == Some(b'=') => two!(TokenKind::StarEq),
            b'*' => one!(TokenKind::Star),
            b'/' if b == Some(b'=') => two!(TokenKind::SlashEq),
            b'/' => one!(TokenKind::Slash),
            b'%' if b == Some(b'=') => two!(TokenKind::PercentEq),
            b'%' => one!(TokenKind::Percent),
            b'&' if b == Some(b'&') => two!(TokenKind::AmpAmp),
            b'&' if b == Some(b'=') => two!(TokenKind::AmpEq),
            b'&' => one!(TokenKind::Amp),
            b'|' if b == Some(b'|') => two!(TokenKind::PipePipe),
            b'|' if b == Some(b'=') => two!(TokenKind::PipeEq),
            b'|' => one!(TokenKind::Pipe),
            b'^' if b == Some(b'=') => two!(TokenKind::CaretEq),
            b'^' => one!(TokenKind::Caret),
            b'!' if b == Some(b'=') => two!(TokenKind::BangEq),
            b'!' => one!(TokenKind::Bang),
            b'=' if b == Some(b'=') => two!(TokenKind::EqEq),
            b'=' if b == Some(b'>') => two!(TokenKind::FatArrow),
            b'=' => one!(TokenKind::Eq),
            b'<' if b == Some(b'<') && c == Some(b'=') => three!(TokenKind::LtLtEq),
            b'<' if b == Some(b'<') => two!(TokenKind::LtLt),
            b'<' if b == Some(b'=') => two!(TokenKind::LtEq),
            b'<' => one!(TokenKind::Lt),
            b'>' if b == Some(b'>') && c == Some(b'=') => three!(TokenKind::GtGtEq),
            b'>' if b == Some(b'>') => two!(TokenKind::GtGt),
            b'>' if b == Some(b'=') => two!(TokenKind::GtEq),
            b'>' => one!(TokenKind::Gt),
            _ => TokenKind::Unknown(a as char),
        }
    }
}

fn parse_int(digits: &str, base: Option<u32>) -> u128 {
    let base = base.unwrap_or(10);
    let mut v: u128 = 0;
    for ch in digits.chars() {
        if ch == '.' || ch == 'e' {
            continue; // safety; floats handled separately
        }
        let d = ch.to_digit(base);
        if let Some(d) = d {
            v = v.saturating_mul(base as u128).saturating_add(d as u128);
        }
    }
    v
}

/// Map an identifier to its keyword token, if it is a reserved word in the
/// Phase-1 language subset.
fn keyword(name: &str) -> Option<TokenKind> {
    use TokenKind::*;
    Some(match name {
        "fn" => Fn,
        "let" => Let,
        "if" => If,
        "else" => Else,
        "while" => While,
        "for" => For,
        "loop" => Loop,
        "return" => Return,
        "break" => Break,
        "continue" => Continue,
        "struct" => Struct,
        "enum" => Enum,
        "match" => Match,
        "macro" => Macro,
        "true" => True,
        "false" => False,
        "mut" => Mut,
        "pub" => Pub,
        "use" => Use,
        "const" => Const,
        "static" => Static,
        "async" => Async,
        "await" => Await,
        "unsafe" => Unsafe,
        "as" => As,
        "in" => In,
        "i8" => I8,
        "i16" => I16,
        "i32" => I32,
        "i64" => I64,
        "i128" => I128,
        "isize" => Isize,
        "u8" => U8,
        "u16" => U16,
        "u32" => U32,
        "u64" => U64,
        "u128" => U128,
        "usize" => Usize,
        "bool" => Bool,
        "char" => CharKw,
        "str" => StrKw,
        "f32" => F32,
        "f64" => F64,
        _ => return None,
    })
}

/// Convenience helper: tokenize an entire source slice into a vector.
pub fn tokenize(src: &str) -> Vec<Token> {
    let mut lexer = Lexer::new(src);
    let mut out = Vec::new();
    loop {
        let t = lexer.next_token();
        let is_eof = t.kind == TokenKind::Eof;
        out.push(t);
        if is_eof {
            break;
        }
    }
    out
}
