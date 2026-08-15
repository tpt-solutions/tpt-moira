//! Clotho — Moira's streaming lexer, parser, and AST frontend.
//!
//! Clotho is the first of the three Fates: it spins the thread of syntax. It
//! provides a streaming lexer ([`lexer`]), a recursive-descent parser
//! ([`parser`]), the abstract syntax tree ([`ast`]), and token definitions
//! ([`token`]).
//!
//! # Example
//! ```
//! use clotho::parse_source;
//! let prog = parse_source("fn main() { let x = 1 + 2; }").unwrap();
//! assert_eq!(prog.items.len(), 1);
//! ```

pub mod ast;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod token;

/// Parse a complete program from a source string.
pub fn parse_source(src: &str) -> Result<ast::Program, error::Diag> {
    let mut p = parser::Parser::from_source(src);
    p.parse_program()
}

/// Tokenize a source string into a vector of tokens.
pub fn tokenize_source(src: &str) -> Vec<token::Token> {
    lexer::tokenize(src)
}
