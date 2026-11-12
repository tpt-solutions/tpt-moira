//! Clotho — Moira's streaming lexer, parser, and AST frontend.
//!
//! Clotho is the first of the three Fates: it spins the thread of syntax. It
//! provides a streaming lexer ([`lexer`]), a recursive-descent parser
//! ([`parser`]), the abstract syntax tree ([`ast`]), and token definitions
//! ([`token`]).
//!
//! # Example
//! ```
//! use tpt_moira_clotho::parse_source;
//! let prog = parse_source("fn main() { let x = 1 + 2; }").unwrap();
//! assert_eq!(prog.items.len(), 1);
//! ```

pub mod alias;
pub mod ast;
pub mod cfg;
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
    compile_file_in(path, &mut 0)
}

/// Like [`compile_file`], but span offsets are drawn from `next_base`. A driver that
/// loads several programs (the root crate and its extern crates) passes one counter
/// through all of them, so no two files in the run share a span offset.
pub fn compile_file_in(path: &Path, next_base: &mut usize) -> Result<ast::Program, error::Diag> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        error::Diag::error(format!("cannot read `{}`: {}", path.display(), e), None)
    })?;
    let base = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let prog = pipeline::compile_at(&src, reserve_span_range(next_base, src.len()))
        .map_err(|d| in_file(d, path))?;
    flatten_modules(prog, &base, next_base)
}

/// Reserve the offsets `[*next, *next + len]` for one file and return the base.
/// Each file compiled for a program reserves a disjoint range, so spans are unique
/// across all files of the flattened program.
fn reserve_span_range(next: &mut usize, len: usize) -> usize {
    let base = *next;
    *next += len + 1;
    base
}

/// Name the file a diagnostic came from: front-end errors carry only byte offsets,
/// which are meaningless without the file (a `mod` file is a different text).
fn in_file(mut d: error::Diag, path: &Path) -> error::Diag {
    d.message = format!("{} (in `{}`)", d.message, path.display());
    d
}

/// Recursively inline module items into a flat item list, loading the source
/// of `mod name;` declarations relative to `base`.
fn flatten_modules(
    prog: ast::Program,
    base: &Path,
    next_base: &mut usize,
) -> Result<ast::Program, error::Diag> {
    let items = flatten_items(prog.items, base, next_base)?;
    Ok(ast::Program { items })
}

fn flatten_items(
    items: Vec<ast::Item>,
    base: &Path,
    next_base: &mut usize,
) -> Result<Vec<ast::Item>, error::Diag> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item.kind {
            ast::ItemKind::Module(ref m) => {
                // A `#[cfg(test)] mod tests { .. }` gate applies to every item
                // inside: propagate the module's cfg attributes onto the
                // flattened children so the checker/codegen can skip them.
                let gated = item.is_cfg_test();
                let inherited = if gated {
                    item.attributes.clone()
                } else {
                    Vec::new()
                };
                let sub = match &m.kind {
                    ast::ModuleKind::Inline(sub) => flatten_items(sub.clone(), base, next_base)?,
                    ast::ModuleKind::External => {
                        let candidate = base.join(format!("{}.rs", m.name.node));
                        let (sub_path, sub_base) =
                            if let Some(file) = cfg::module_path(&item.attributes) {
                                // `#[path = "file.rs"]` names the module's file explicitly.
                                let sub_path = base.join(&file);
                                let sub_base = sub_path.parent().unwrap_or(base).to_path_buf();
                                (sub_path, sub_base)
                            } else if candidate.is_file() {
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
                        let sub_prog =
                            pipeline::compile_at(&sub_src, reserve_span_range(next_base, sub_src.len()))
                                .map_err(|d| in_file(d, &sub_path))?;
                        flatten_items(sub_prog.items, &sub_base, next_base)?
                    }
                };
                for mut child in sub {
                    child.attributes.extend(inherited.iter().cloned());
                    out.push(child);
                }
            }
            other => out.push(ast::Item {
                visibility: item.visibility,
                kind: other,
                attributes: item.attributes.clone(),
            }),
        }
    }
    Ok(out)
}
