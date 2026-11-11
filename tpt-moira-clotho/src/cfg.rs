//! `#[cfg(..)]` predicate evaluation for the compilation target.
//!
//! Moira compiles for one fixed configuration: 64-bit Windows, x86_64, MSVC,
//! little-endian, with **no Cargo features enabled** and `cfg(test)` off. Items
//! whose `cfg` is false are dropped by the parser, exactly like unmodeled items,
//! so a `#[cfg(feature = "std")] mod x;` never tries to load `x.rs`.
//!
//! The predicate grammar is Rust's: `name`, `name = "value"`, `all(..)`,
//! `any(..)` and `not(..)`. Unknown names and values evaluate to false.

use crate::ast::Attribute;
use crate::token::{Token, TokenKind};

/// Whether every `#[cfg(..)]` attribute in `attrs` is satisfied by the target.
/// Other attributes (`derive`, `allow`, `cfg_attr`, ...) do not affect this.
pub fn attrs_enabled(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .filter(|a| a.path.segments.last().is_some_and(|s| s.node == "cfg"))
        .all(|a| eval_attr_args(&a.args))
}

/// Evaluate the argument tokens of one `cfg(..)` attribute. `args` are the tokens
/// after the attribute path, including the wrapping parentheses.
fn eval_attr_args(args: &[Token]) -> bool {
    let inner = match args.first().map(|t| &t.kind) {
        Some(TokenKind::LParen) if args.len() >= 2 => &args[1..args.len() - 1],
        _ => return false,
    };
    let mut pos = 0;
    let value = eval_pred(inner, &mut pos);
    value.unwrap_or(false)
}

/// Evaluate one predicate starting at `*pos`, advancing past it. `None` when the
/// tokens are not a well-formed predicate (treated as false by the caller).
fn eval_pred(toks: &[Token], pos: &mut usize) -> Option<bool> {
    let name = match toks.get(*pos).map(|t| &t.kind) {
        Some(TokenKind::Ident(n)) => n.clone(),
        _ => return None,
    };
    *pos += 1;
    match toks.get(*pos).map(|t| &t.kind) {
        // `name = "value"`
        Some(TokenKind::Eq) => {
            *pos += 1;
            let value = match toks.get(*pos).map(|t| &t.kind) {
                Some(TokenKind::Str(v)) => v.clone(),
                _ => return None,
            };
            *pos += 1;
            Some(key_value_enabled(&name, &value))
        }
        // `all(..)`, `any(..)`, `not(..)`
        Some(TokenKind::LParen) => {
            *pos += 1;
            let mut items = Vec::new();
            loop {
                match toks.get(*pos).map(|t| &t.kind) {
                    Some(TokenKind::RParen) => {
                        *pos += 1;
                        break;
                    }
                    Some(TokenKind::Comma) => *pos += 1,
                    _ => items.push(eval_pred(toks, pos)?),
                }
            }
            match name.as_str() {
                "all" => Some(items.iter().all(|b| *b)),
                "any" => Some(items.iter().any(|b| *b)),
                "not" if items.len() == 1 => Some(!items[0]),
                _ => None,
            }
        }
        // A bare name such as `windows` or `test`.
        _ => Some(bare_enabled(&name)),
    }
}

/// `name = "value"` predicates for the fixed target.
fn key_value_enabled(name: &str, value: &str) -> bool {
    match name {
        "target_os" => value == "windows",
        "target_family" => value == "windows",
        "target_arch" => value == "x86_64",
        "target_pointer_width" => value == "64",
        "target_endian" => value == "little",
        "target_env" => value == "msvc",
        "target_vendor" => value == "pc",
        "target_has_atomic" => matches!(value, "8" | "16" | "32" | "64" | "ptr"),
        // Cargo features are all disabled.
        "feature" => false,
        _ => false,
    }
}

/// Bare-name predicates for the fixed target.
fn bare_enabled(name: &str) -> bool {
    match name {
        "windows" | "debug_assertions" | "target_thread_local" => true,
        // `test`, `unix`, `doc`, `docsrs`, `miri`, `panic` ... are all off.
        _ => false,
    }
}

/// The file named by a module's `#[path = "file.rs"]` attribute, including one
/// written as `#[cfg_attr(pred, path = "file.rs")]` whose predicate holds.
pub fn module_path(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        match attr.path.segments.last().map(|s| s.node.as_str()) {
            Some("path") => {
                // `#[path = "file"]`: an `=` then the string.
                let eq = attr.args.first().map(|t| &t.kind) == Some(&TokenKind::Eq);
                if let (true, Some(TokenKind::Str(file))) = (eq, attr.args.get(1).map(|t| &t.kind))
                {
                    return Some(file.clone());
                }
            }
            Some("cfg_attr") => {
                if let Some(file) = cfg_attr_path(&attr.args) {
                    return Some(file);
                }
            }
            _ => {}
        }
    }
    None
}

/// For `cfg_attr(pred, path = "file")` (argument tokens including parentheses):
/// the file, when `pred` holds for the target.
fn cfg_attr_path(args: &[Token]) -> Option<String> {
    let inner = match args.first().map(|t| &t.kind) {
        Some(TokenKind::LParen) if args.len() >= 2 => &args[1..args.len() - 1],
        _ => return None,
    };
    // The predicate ends at the first comma that is not nested in parentheses.
    let mut depth = 0i32;
    let mut comma = None;
    for (i, t) in inner.iter().enumerate() {
        match t.kind {
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => depth -= 1,
            TokenKind::Comma if depth == 0 => {
                comma = Some(i);
                break;
            }
            _ => {}
        }
    }
    let comma = comma?;
    let mut pos = 0;
    if !eval_pred(&inner[..comma], &mut pos).unwrap_or(false) {
        return None;
    }
    // The remainder must be `path = "file"`.
    let rest = &inner[comma + 1..];
    let is_path = matches!(rest.first().map(|t| &t.kind), Some(TokenKind::Ident(n)) if n == "path")
        && rest.get(1).map(|t| &t.kind) == Some(&TokenKind::Eq);
    if !is_path {
        return None;
    }
    match rest.get(2).map(|t| &t.kind) {
        Some(TokenKind::Str(file)) => Some(file.clone()),
        _ => None,
    }
}
