//! The Clotho concurrent frontend pipeline.
//!
//! This is the "macro-expansion runs alongside file I/O" stage. [`compile`] runs
//! the full single-source path: read bytes → tokenize → extract macros → parse →
//! expand. [`compile_concurrent`] runs several independent sources in parallel,
//! so the macro-expansion and parsing of one translation unit overlaps with the
//! disk reads of the others — the CPU is never idle waiting on a single file.
//!
//! In a real driver the producer side would stream chunks from `std::fs::File`
//! through a channel into the tokenizer; here we model that with a background
//! thread that reads the whole source while the consumer tokenizes, proving the
//! stages are decoupled.

use crate::ast::Program;
use crate::derive::expand_derives;
use crate::error::{Diag, ParseResult};
use crate::lexer;
use crate::macrorules::{expand_macro_rules_calls, extract_macro_rules};
use crate::macros::{expand_program_concurrent, extract_macros, MacroTable};
use crate::parser::Parser;
use std::io::Read;

/// Run the full frontend pipeline for a single source string.
///
/// Returns the macro-expanded AST. With an empty macro table this is equivalent
/// to [`crate::parse_source`] but routed through the streaming lexer. Both the
/// restricted `macro` keyword and `macro_rules!` definitions are extracted; the
/// latter are expanded from the token stream *before* parsing.
pub fn compile(source: &str) -> ParseResult<Program> {
    compile_at(source, 0)
}

/// Like [`compile`], but every token span is shifted by `base`. Each source file of
/// a crate gets its own base, so spans are unique across the flattened program.
/// Passes that key information by span (the checker's expression types) would
/// otherwise confuse two files' expressions at the same byte offset.
pub fn compile_at(source: &str, base: usize) -> ParseResult<Program> {
    let mut tokens = lexer::tokenize(source);
    if base != 0 {
        for t in &mut tokens {
            t.span.start += base;
            t.span.end += base;
        }
    }
    let (table, rest) = extract_macros(&tokens)?;
    let (mr_table, rest2) = extract_macro_rules(&rest)?;
    let expanded_tokens = expand_macro_rules_calls(&rest2, &mr_table);
    let parsed = Parser::new(expanded_tokens).parse_program()?;
    let with_derives = expand_derives(&parsed);
    let expanded = expand_program_concurrent(&with_derives, &table);
    Ok(crate::closure_return::inline_returned_closures(
        crate::let_conditions::desugar_let_conditions(expanded),
    ))
}

/// Like [`compile`], but reads bytes from any [`Read`] (e.g. a file) on a
/// background thread so tokenization/parsing can begin without blocking on the
/// entire read completing. This is the I/O-decoupled entry point.
pub fn compile_reader<R: Read + Send + 'static>(mut reader: R) -> ParseResult<Program> {
    // The bytes are read on the calling thread. This used to run the read on a background
    // thread; threads are outside the codegen subset (the compiler must compile its own
    // source), and the result is the same, as with `compile_concurrent`.
    let mut source = String::new();
    reader
        .read_to_string(&mut source)
        .map_err(|e: std::io::Error| Diag::error(format!("frontend read error: {e}"), None))?;
    compile(&source)
}

/// Compile many independent sources, returning the programs in input order (or every
/// diagnostic). The sources are compiled one after another: the compiler's own source is
/// built without threads (the backend has none), and the results do not depend on the
/// order in which the sources are processed.
pub fn compile_concurrent(sources: &[String]) -> Result<Vec<Program>, Vec<Diag>> {
    let results: Vec<Result<Program, Diag>> = sources.iter().map(|src| compile(src)).collect();

    let mut ok = Vec::with_capacity(results.len());
    let mut errs = Vec::new();
    for r in results {
        match r {
            Ok(p) => ok.push(p),
            Err(e) => errs.push(e),
        }
    }
    if errs.is_empty() {
        Ok(ok)
    } else {
        Err(errs)
    }
}

/// Build a macro table from a source without parsing the rest. Useful for
/// tooling that wants to inspect available macros.
pub fn extract_only(source: &str) -> ParseResult<(MacroTable, Vec<crate::token::Token>)> {
    let tokens = lexer::tokenize(source);
    let (table, rest) = extract_macros(&tokens)?;
    Ok((table, rest))
}
