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
pub mod closure_return;
pub mod derive;
pub mod error;
pub mod let_conditions;
pub mod lexer;
pub mod macrorules;
pub mod macros;
pub mod parser;
pub mod pipeline;
pub mod resolve;
pub mod std_imports;
pub mod token;

use std::path::{Path, PathBuf};

/// Parse a complete program from a source string.
pub fn parse_source(src: &str) -> Result<ast::Program, error::Diag> {
    let mut p = parser::Parser::from_source(src);
    Ok(closure_return::inline_returned_closures(
        let_conditions::desugar_let_conditions(std_imports::resolve_std_paths(p.parse_program()?)),
    ))
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
    compile_file_mapped(path, next_base, &mut Vec::new())
}

/// A source file loaded into a program, and the span offset its text starts at.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub base: usize,
    pub path: PathBuf,
    pub src: String,
}

/// Like [`compile_file_in`], and also records each file loaded (its span base, path and
/// text) in `files`, so a span offset of the program can be found again with
/// [`locate_span`].
pub fn compile_file_mapped(
    path: &Path,
    next_base: &mut usize,
    files: &mut Vec<SourceFile>,
) -> Result<ast::Program, error::Diag> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        error::Diag::error(format!("cannot read `{}`: {}", path.display(), e), None)
    })?;
    let base = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let root_base = reserve_span_range(next_base, src.len());
    files.push(SourceFile {
        base: root_base,
        path: path.to_path_buf(),
        src: src.clone(),
    });
    let prog = pipeline::compile_at(&src, root_base).map_err(|d| in_file(d, path))?;
    flatten_modules(prog, &base, next_base, files)
}

/// The `path:line:col` of a span offset, when it falls in one of `files`. Offsets of
/// derived or generated code belong to no file and give `None`.
pub fn locate_span(files: &[SourceFile], offset: usize) -> Option<String> {
    // The file whose text starts closest below the offset holds it.
    let mut found: Option<&SourceFile> = None;
    let mut k = 0;
    while k < files.len() {
        let f = &files[k];
        if f.base <= offset && offset <= f.base + f.src.len() {
            let closer = match found {
                Some(g) => f.base > g.base,
                None => true,
            };
            if closer {
                found = Some(f);
            }
        }
        k += 1;
    }
    let file = found?;
    let rel = offset - file.base;
    let bytes = file.src.as_bytes();
    let mut line = 1;
    let mut line_start = 0;
    let mut i = 0;
    while i < rel && i < bytes.len() {
        if bytes[i] == b'\n' {
            line += 1;
            line_start = i + 1;
        }
        i += 1;
    }
    Some(format!(
        "{}:{}:{}",
        file.path.display(),
        line,
        rel - line_start + 1
    ))
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
    files: &mut Vec<SourceFile>,
) -> Result<ast::Program, error::Diag> {
    let items = flatten_items(prog.items, base, next_base, files)?;
    Ok(ast::Program { items })
}

fn flatten_items(
    items: Vec<ast::Item>,
    base: &Path,
    next_base: &mut usize,
    files: &mut Vec<SourceFile>,
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
                    ast::ModuleKind::Inline(sub) => {
                        flatten_items(sub.clone(), base, next_base, files)?
                    }
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
                        let sub_span_base = reserve_span_range(next_base, sub_src.len());
                        files.push(SourceFile {
                            base: sub_span_base,
                            path: sub_path.clone(),
                            src: sub_src.clone(),
                        });
                        let sub_prog = pipeline::compile_at(&sub_src, sub_span_base)
                            .map_err(|d| in_file(d, &sub_path))?;
                        flatten_items(sub_prog.items, &sub_base, next_base, files)?
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
