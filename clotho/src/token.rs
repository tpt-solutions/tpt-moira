//! Lexical tokens produced by the Clotho streaming lexer.

use std::fmt;

/// A byte-offset span within a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }
}

/// A token with its kind and source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    // --- Literals ---
    /// An identifier, e.g. `foo`, `_`, `r#type`.
    Ident(String),
    /// An integer literal value with an optional type suffix (`123u32`).
    Int {
        value: u128,
        suffix: Option<String>,
    },
    /// A floating point literal with its raw text and optional suffix.
    Float {
        text: String,
        suffix: Option<String>,
    },
    /// A string literal payload (without the surrounding quotes).
    Str(String),
    /// A character literal value.
    Char(char),

    // --- Keywords ---
    Fn,
    Let,
    If,
    Else,
    While,
    For,
    Loop,
    Return,
    Break,
    Continue,
    Struct,
    Enum,
    Match,
    Macro,
    True,
    False,
    Mut,
    Pub,
    Use,
    Const,
    Static,
    Async,
    Await,
    Unsafe,
    As,
    In,
    /// Primitive type keywords.
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    Bool,
    CharKw,
    StrKw,
    F32,
    F64,

    // --- Delimiters & punctuation ---
    /// Left parenthesis `(`.
    LParen,
    /// Right parenthesis `)`.
    RParen,
    /// Left brace `{`.
    LBrace,
    /// Right brace `}`.
    RBrace,
    /// Left bracket `[`.
    LBracket,
    /// Right bracket `]`.
    RBracket,
    /// Comma `,`.
    Comma,
    /// Semicolon `;`.
    Semicolon,
    /// Colon `:`.
    Colon,
    /// Double colon `::`.
    ColonColon,
    /// Dot `.`.
    Dot,
    /// Dot-dot `..`.
    DotDot,
    /// Arrow `->`.
    Arrow,
    /// Fat arrow `=>`.
    FatArrow,
    /// At sign `@`.
    At,
    /// Underscore `_`.
    Underscore,
    /// Hash `#`.
    Hash,
    /// Dollar `$`.
    Dollar,
    /// Question `?`.
    Question,

    // --- Operators ---
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    AmpAmp,
    Pipe,
    PipePipe,
    Caret,
    Bang,
    Eq,
    EqEq,
    BangEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    LtLt,
    GtGt,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    LtLtEq,
    GtGtEq,

    /// An unrecognized character; carries the offending byte.
    Unknown(char),
    /// Virtual end-of-file marker.
    Eof,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use TokenKind::*;
        let s = match self {
            Ident(s) => return write!(f, "identifier `{s}`"),
            Int { value, suffix } => {
                return write!(f, "integer `{value}{}`", suffix.as_deref().unwrap_or(""))
            }
            Float { text, suffix } => {
                return write!(f, "float `{text}{}`", suffix.as_deref().unwrap_or(""))
            }
            Str(s) => return write!(f, "string `\"{}\"`", s.escape_default()),
            Char(c) => return write!(f, "char `{}`", c.escape_default()),
            Fn => "fn",
            Let => "let",
            If => "if",
            Else => "else",
            While => "while",
            For => "for",
            Loop => "loop",
            Return => "return",
            Break => "break",
            Continue => "continue",
            Struct => "struct",
            Enum => "enum",
            Match => "match",
            Macro => "macro",
            True => "true",
            False => "false",
            Mut => "mut",
            Pub => "pub",
            Use => "use",
            Const => "const",
            Static => "static",
            Async => "async",
            Await => "await",
            Unsafe => "unsafe",
            As => "as",
            In => "in",
            I8 => "i8",
            I16 => "i16",
            I32 => "i32",
            I64 => "i64",
            I128 => "i128",
            Isize => "isize",
            U8 => "u8",
            U16 => "u16",
            U32 => "u32",
            U64 => "u64",
            U128 => "u128",
            Usize => "usize",
            Bool => "bool",
            CharKw => "char",
            StrKw => "str",
            F32 => "f32",
            F64 => "f64",
            LParen => "(",
            RParen => ")",
            LBrace => "{",
            RBrace => "}",
            LBracket => "[",
            RBracket => "]",
            Comma => ",",
            Semicolon => ";",
            Colon => ":",
            ColonColon => "::",
            Dot => ".",
            DotDot => "..",
            Arrow => "->",
            FatArrow => "=>",
            At => "@",
            Underscore => "_",
            Hash => "#",
            Dollar => "$",
            Question => "?",
            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            Percent => "%",
            Amp => "&",
            AmpAmp => "&&",
            Pipe => "|",
            PipePipe => "||",
            Caret => "^",
            Bang => "!",
            Eq => "=",
            EqEq => "==",
            BangEq => "!=",
            Lt => "<",
            Gt => ">",
            LtEq => "<=",
            GtEq => ">=",
            LtLt => "<<",
            GtGt => ">>",
            PlusEq => "+=",
            MinusEq => "-=",
            StarEq => "*=",
            SlashEq => "/=",
            PercentEq => "%=",
            AmpEq => "&=",
            PipeEq => "|=",
            CaretEq => "^=",
            LtLtEq => "<<=",
            GtGtEq => ">>=",
            Unknown(c) => return write!(f, "unknown character `{c}`"),
            Eof => "end of file",
        };
        f.write_str(s)
    }
}
