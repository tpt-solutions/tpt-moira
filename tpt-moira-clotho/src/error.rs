//! Clotho error types.

use crate::token::Span;
use std::fmt;

/// A diagnostic produced by the frontend (lexer or parser).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    pub level: Level,
    pub message: String,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.span {
            Some(span) => write!(
                f,
                "{:?} at bytes {}-{}: {}",
                self.level, span.start, span.end, self.message
            ),
            None => write!(f, "{:?}: {}", self.level, self.message),
        }
    }
}

impl Diag {
    pub fn error(message: impl Into<String>, span: Option<Span>) -> Self {
        Diag {
            level: Level::Error,
            message: message.into(),
            span,
        }
    }
}

/// A parse result wrapper.
pub type ParseResult<T> = Result<T, Diag>;
