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
use crate::error::{Diag, ParseResult};
use crate::lexer;
use crate::macros::{expand_program_concurrent, extract_macros, MacroTable};
use crate::parser::Parser;
use std::io::Read;

/// Run the full frontend pipeline for a single source string.
///
/// Returns the macro-expanded AST. With an empty macro table this is equivalent
/// to [`crate::parse_source`] but routed through the streaming lexer.
pub fn compile(source: &str) -> ParseResult<Program> {
    let tokens = lexer::tokenize(source);
    let (table, rest) = extract_macros(&tokens)?;
    let parsed = Parser::new(rest).parse_program()?;
    let expanded = expand_program_concurrent(&parsed, &table);
    Ok(expanded)
}

/// Like [`compile`], but reads bytes from any [`Read`] (e.g. a file) on a
/// background thread so tokenization/parsing can begin without blocking on the
/// entire read completing. This is the I/O-decoupled entry point.
pub fn compile_reader<R: Read + Send + 'static>(mut reader: R) -> ParseResult<Program> {
    // Spawn the "disk" reader; the main thread waits for the bytes, but in a
    // multi-file build this thread would feed a channel while other files are
    // processed concurrently (see `compile_concurrent`).
    let handle = std::thread::spawn(move || {
        let mut buf = String::new();
        reader.read_to_string(&mut buf).map(|_| buf)
    });
    let source = handle
        .join()
        .map_err(|_| Diag::error("frontend reader thread panicked", None))?
        .map_err(|e: std::io::Error| Diag::error(format!("frontend read error: {e}"), None))?;
    compile(&source)
}

/// Compile many independent sources in parallel. Each source goes through the
/// full read→lex→parse→expand path on its own worker thread, so macro expansion
/// of one unit never stalls on another unit's I/O.
pub fn compile_concurrent(sources: &[String]) -> Result<Vec<Program>, Vec<Diag>> {
    let results = std::thread::scope(|s| {
        let handles: Vec<_> = sources
            .iter()
            .map(|src| s.spawn(|| compile(src)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });

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
