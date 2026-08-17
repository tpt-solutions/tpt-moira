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
pub mod derive;
pub mod error;
pub mod lexer;
pub mod macrorules;
pub mod macros;
pub mod parser;
pub mod pipeline;
pub mod token;

use std::path::{Path, PathBuf};

/// Parse a complete program from a source string.
pub fn parse_source(src: &str) -> Result<ast::Program, error::Diag> {
    let mut p = parser::Parser::from_source(src);
    p.parse_program()
}

/// Parse a complete program through the concurrent macro-expansion pipeline.
pub fn compile_source(src: &str) -> Result<ast::Program, error::Diag> {
    pipeline::compile(src)
}

/// Tokenize a source string into a vector of tokens.
pub fn tokenize_source(src: &str) -> Vec<token::Token> {
    lexer::tokenize(src)
}

/// Parse a source file, resolving `mod` declarations into their sibling
/// `.rs` files (or `<name>/mod.rs`), and return a single flattened program
/// whose item list contains every reachable definition.
///
/// This is the entry point the Moira driver uses for real source files; it is
/// what lets the compiler parse its own crate layout (a prerequisite for
/// self-hosting). `compile_source` remains for path-less snippets (stdin).
pub fn compile_file(path: &Path) -> Result<ast::Program, error::Diag> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        error::Diag::error(format!("cannot read `{}`: {}", path.display(), e), None)
    })?;
    let base = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let prog = pipeline::compile(&src)?;
    flatten_modules(prog, &base)
}

/// Recursively inline module items into a flat item list, loading the source
/// of `mod name;` declarations relative to `base`.
fn flatten_modules(prog: ast::Program, base: &Path) -> Result<ast::Program, error::Diag> {
    let items = flatten_items(prog.items, base)?;
    Ok(ast::Program { items })
}

fn flatten_items(items: Vec<ast::Item>, base: &Path) -> Result<Vec<ast::Item>, error::Diag> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item.kind {
            ast::ItemKind::Module(m) => match m.kind {
                ast::ModuleKind::Inline(sub) => {
                    out.extend(flatten_items(sub, base)?);
                }
                ast::ModuleKind::External => {
                    let candidate = base.join(format!("{}.rs", m.name.node));
                    let (sub_path, sub_base) = if candidate.is_file() {
                        (
                            candidate.clone(),
                            candidate.parent().unwrap_or(base).to_path_buf(),
                        )
                    } else {
                        let mod_rs = base.join(&m.name.node).join("mod.rs");
                        (mod_rs, base.join(&m.name.node))
                    };
                    let sub_src = std::fs::read_to_string(&sub_path).map_err(|e| {
                        error::Diag::error(
                            format!(
                                "cannot resolve module `{}` (tried `{}`): {}",
                                m.name.node,
                                sub_path.display(),
                                e
                            ),
                            None,
                        )
                    })?;
                    let sub_prog = pipeline::compile(&sub_src)?;
                    out.extend(flatten_items(sub_prog.items, &sub_base)?);
                }
            },
            other => out.push(ast::Item {
                visibility: item.visibility,
                kind: other,
                attributes: item.attributes.clone(),
            }),
        }
    }
    Ok(out)
}
