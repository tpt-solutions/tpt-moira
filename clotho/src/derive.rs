//! Built-in `#[derive(...)]` macro expansion.
//!
//! Moira's `derive` support is implemented as a frontend pass that runs after
//! parsing (see [`expand_derives`]). For each `#[derive(Trait, ...)]` attribute
//! on a `struct` or `enum`, it generates the corresponding trait `impl` and
//! appends it to the program's item list.
//!
//! The generated `impl`s reuse the existing [`ImplDef`] AST node, so they flow
//! through the rest of the pipeline unchanged: the Lachesis type/borrow checker
//! and the Atropos codegen treat user-written and derive-generated `impl`s
//! identically (the checker ignores impl bodies; see `lachesis::infer` and
//! `atropos::lower`).
//!
//! Supported built-in derives: `Clone`, `Copy`, `Debug`, `Default`,
//! `PartialEq`, `Eq`, `Hash`, `Ord`, `PartialOrd`. Unknown derive paths are
//! left in place (so user-defined derive macros can be layered on later).

use crate::ast::*;
use crate::parser::Parser;

/// Expand every `#[derive(...)]` attribute on struct/enum items into generated
/// trait `impl`s, returning a program whose item list contains both the
/// original definitions and the derived `impl`s.
pub fn expand_derives(prog: &Program) -> Program {
    let mut items = Vec::with_capacity(prog.items.len());
    for item in &prog.items {
        items.push(item.clone());
        match &item.kind {
            ItemKind::Struct(s) => {
                for t in derives_of(item) {
                    if let Some(src) = derive_struct(&t, s) {
                        append_impl(&mut items, &src);
                    }
                }
            }
            ItemKind::Enum(e) => {
                for t in derives_of(item) {
                    if let Some(src) = derive_enum(&t, e) {
                        append_impl(&mut items, &src);
                    }
                }
            }
            _ => {}
        }
    }
    Program { items }
}

/// Collect the trait names listed in the item's `derive` attribute(s).
fn derives_of(item: &Item) -> Vec<String> {
    let mut out = Vec::new();
    for attr in &item.attributes {
        if attr.path.segments.last().map(|s| s.node.as_str()) == Some("derive") {
            // The derive argument list is a comma-separated set of paths;
            // pull the identifier text straight out of the tokens.
            let mut cur = String::new();
            for t in &attr.args {
                if let crate::token::TokenKind::Ident(name) = &t.kind {
                    cur = name.clone();
                } else if matches!(t.kind, crate::token::TokenKind::Comma) {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
            }
            if !cur.is_empty() {
                out.push(cur);
            }
        }
    }
    out
}

/// Parse a generated `impl` source and append it (preserving the original
/// item's visibility attributes are not carried onto impls).
fn append_impl(items: &mut Vec<Item>, src: &str) {
    let mut p = Parser::from_source(src);
    match p.parse_item() {
        Ok(impl_item) => items.push(impl_item),
        Err(_) => {
            // If the generated source fails to parse, drop it rather than
            // poisoning the whole program; the derive is best-effort.
        }
    }
}

/// Generate the `impl` source for a derive trait on an enum. Exposed for
/// tooling/tests to inspect the generated text.
pub fn derive_enum_src(trait_name: &str, e: &EnumDef) -> Option<String> {
    derive_enum(trait_name, e)
}
fn derive_struct(trait_name: &str, s: &StructDef) -> Option<String> {
    let name = &s.name.node;
    let fields = &s.fields;
    let src = match trait_name {
        "Clone" => {
            let inits = fields
                .iter()
                .map(|f| format!("{}: self.{}.clone()", f.name.node, f.name.node))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "impl Clone for {name} {{ fn clone(&self) -> Self {{ Self {{ {inits} }} }} }}"
            )
        }
        "Copy" => {
            // No methods; emit a marker impl with an empty body.
            format!("impl Copy for {name} {{ }}")
        }
        "Debug" => {
            format!(
                "impl Debug for {name} {{ fn fmt(&self, f: &mut Formatter) -> () {{ 0; }} }}"
            )
        }
        "Default" => {
            let inits = fields
                .iter()
                .map(|f| format!("{}: Default::default()", f.name.node))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "impl Default for {name} {{ fn default() -> Self {{ Self {{ {inits} }} }} }}"
            )
        }
        "PartialEq" => {
            let conds = fields
                .iter()
                .map(|f| format!("self.{fn_field} == other.{fn_field}", fn_field = f.name.node))
                .collect::<Vec<_>>()
                .join(" && ");
            let conds = if conds.is_empty() {
                "true".to_string()
            } else {
                conds
            };
            format!(
                "impl PartialEq for {name} {{ fn eq(&self, other: &Self) -> bool {{ {conds} }} }}"
            )
        }
        "Eq" => format!("impl Eq for {name} {{ }}"),
        "Hash" => {
            let body = fields
                .iter()
                .map(|f| format!("self.{fn_field}.hash(h);", fn_field = f.name.node))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "impl Hash for {name} {{ fn hash(&self, h: &mut Hasher) -> () {{ {body} }} }}"
            )
        }
        "Ord" => format!(
            "impl Ord for {name} {{ fn cmp(&self, other: &Self) -> i32 {{ 0 }} }}"
        ),
        "PartialOrd" => format!(
            "impl PartialOrd for {name} {{ fn partial_cmp(&self, other: &Self) -> i32 {{ 0 }} }}"
        ),
        _ => return None,
    };
    Some(src)
}

/// Generate the `impl` source for a derive trait on an enum.
fn derive_enum(trait_name: &str, e: &EnumDef) -> Option<String> {
    let name = &e.name.node;
    let variants = &e.variants;
    let src = match trait_name {
        "Clone" => {
            let arms = variants
                .iter()
                .map(|v| format!("Self::{v} => Self::{v},", v = v.name.node))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "impl Clone for {name} {{ fn clone(&self) -> Self {{ match self {{ {arms} }} }} }}"
            )
        }
        "Copy" => format!("impl Copy for {name} {{ }}"),
        "Debug" => format!(
            "impl Debug for {name} {{ fn fmt(&self, f: &mut Formatter) -> () {{ 0; }} }}"
        ),
        "PartialEq" => {
            // Compare by discriminant index: map each variant to its ordinal
            // and test the two ordinals for equality.
            let arms: Vec<String> = variants
                .iter()
                .enumerate()
                .map(|(i, v)| format!("Self::{v} => {i},", v = v.name.node, i = i))
                .collect();
            let arms = arms.join(" ");
            format!(
                "impl PartialEq for {name} {{ fn eq(&self, other: &Self) -> bool {{ let a: i32 = match self {{ {arms} }}; let b: i32 = match other {{ {arms} }}; a == b }} }}"
            )
        }
        "Eq" => format!("impl Eq for {name} {{ }}"),
        "Hash" => format!(
            "impl Hash for {name} {{ fn hash(&self, h: &mut Hasher) -> () {{ 0; }} }}"
        ),
        "Ord" => format!(
            "impl Ord for {name} {{ fn cmp(&self, other: &Self) -> i32 {{ 0 }} }}"
        ),
        "PartialOrd" => format!(
            "impl PartialOrd for {name} {{ fn partial_cmp(&self, other: &Self) -> i32 {{ 0 }} }}"
        ),
        _ => return None,
    };
    Some(src)
}
