//! `macro_rules!` token matcher and template expander (Phase-3 foundation).
//!
//! This is the first, self-contained slice of the full `macro_rules!` support
//! described in `TODO.md`. It implements:
//!
//! * parsing `macro_rules!` definitions out of a token stream
//!   ([`extract_macro_rules`]);
//! * a token-tree matcher supporting literal tokens, metavariables
//!   (`$x:frag`), and repetitions `$(...)*` / `+` / `?` with optional
//!   separators;
//! * template expansion that substitutes metavariables and repeats captured
//!   fragments, including repetitions nested inside `{ ... }` blocks;
//! * integration with the concurrent frontend pipeline so `name!(...)` calls
//!   are rewritten *before* parsing.
//!
//! The matcher and template are parsed from the **flat** token stream (where
//! `(`/`)` remain leaf tokens), so a `(` in the matcher is a literal group
//! delimiter to be matched against a call-site group — not a token-tree
//! boundary. Fragment capture (`expr`, `ty`, `pat`, `stmt`, `path`, …) is
//! heuristic: it consumes a balanced tree, stopping at a top-level separator
//! (`;`, `,`, `=>`). This is enough for the common macro shapes and is
//! documented as an intentional approximation of `rustc`'s full fragment
//! grammars.

use crate::error::{Diag, ParseResult};
use crate::token::{Span, Token, TokenKind};
use std::collections::HashMap;

/// A delimiter kind for a literal group matcher (`(`/`[`/`{`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delim {
    Paren,
    Brace,
    Bracket,
}

fn open_kind(d: Delim) -> TokenKind {
    match d {
        Delim::Paren => TokenKind::LParen,
        Delim::Brace => TokenKind::LBrace,
        Delim::Bracket => TokenKind::LBracket,
    }
}

fn close_kind(d: Delim) -> TokenKind {
    match d {
        Delim::Paren => TokenKind::RParen,
        Delim::Brace => TokenKind::RBrace,
        Delim::Bracket => TokenKind::RBracket,
    }
}

fn delim_of_open(k: &TokenKind) -> Option<Delim> {
    match k {
        TokenKind::LParen => Some(Delim::Paren),
        TokenKind::LBrace => Some(Delim::Brace),
        TokenKind::LBracket => Some(Delim::Bracket),
        _ => None,
    }
}

fn delim_of_close(k: &TokenKind) -> Option<Delim> {
    match k {
        TokenKind::RParen => Some(Delim::Paren),
        TokenKind::RBrace => Some(Delim::Brace),
        TokenKind::RBracket => Some(Delim::Bracket),
        _ => None,
    }
}

/// A `macro_rules!` fragment specifier (`$x:frag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frag {
    Tt,
    Ident,
    Expr,
    Ty,
    Pat,
    Stmt,
    Block,
    Item,
    Literal,
    Path,
}

/// The repetition kind of a `$(...)*` / `+` / `?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepKind {
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

/// A parsed matcher element.
#[derive(Debug, Clone)]
enum Matcher {
    /// A literal token that must match exactly.
    Tok(TokenKind),
    /// A metavariable `$name:frag`.
    Var(String, Frag),
    /// A literal delimited group `(` … `)` (and `[` / `{`).
    Group(Delim, Vec<Matcher>),
    /// A repetition `$(...)*` / `+` / `?` (with optional separator).
    Rep(Vec<Matcher>, Option<TokenKind>, RepKind),
}

/// A parsed template element.
#[derive(Debug, Clone)]
enum TmplEl {
    /// A literal token to emit verbatim.
    TT(Token),
    /// A metavariable to substitute (`$name`).
    Var(String),
    /// A repetition `$(...)*` / `+` / `?` over the captured fragments.
    Rep(Vec<TmplEl>, Option<TokenKind>, RepKind),
}

/// A captured binding for a metavariable.
#[derive(Clone)]
enum Binding {
    /// Captured once (a top-level `$x`).
    Single(Vec<Token>),
    /// Captured once per repetition iteration (`$x` inside a `$(...)*`).
    Repeat(Vec<Vec<Token>>),
}

type Bindings = HashMap<String, Binding>;

/// A single `macro_rules!` arm.
#[derive(Debug, Clone)]
struct Arm {
    matcher: Vec<Matcher>,
    template: Vec<TmplEl>,
}

/// A parsed `macro_rules!` definition (a set of arms).
#[derive(Debug, Clone)]
pub struct MacroRulesDef {
    arms: Vec<Arm>,
}

impl MacroRulesDef {
    /// Try each arm against the call-site body (including its delimiters),
    /// returning the rendered token stream on the first successful match.
    fn expand_to_tokens(&self, body: &[Token]) -> Option<Vec<Token>> {
        for arm in &self.arms {
            let mut binds = HashMap::new();
            let mut pos = 0;
            let mut rep_acc: Option<HashMap<String, Vec<Vec<Token>>>> = None;
            let matched = match_m(&arm.matcher, body, &mut pos, &mut binds, &mut rep_acc);
            if matched && pos == body.len() {
                return Some(render(&arm.template, &binds));
            }
        }
        None
    }
}

/// A table of `macro_rules!` definitions keyed by name.
#[derive(Debug, Clone, Default)]
pub struct MacroRulesTable {
    pub defs: HashMap<String, MacroRulesDef>,
}

impl MacroRulesTable {
    pub fn new() -> Self {
        MacroRulesTable {
            defs: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Extraction of `macro_rules!` definitions
// ---------------------------------------------------------------------------

/// Pull `macro_rules!` definitions out of a token stream, returning the table
/// and the remaining tokens (with the definitions removed).
pub fn extract_macro_rules(tokens: &[Token]) -> ParseResult<(MacroRulesTable, Vec<Token>)> {
    let mut table = MacroRulesTable::new();
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        // `macro_rules ! name { ... }`
        let is_def = tokens.len() >= i + 4
            && matches!(
                tokens[i].kind,
                TokenKind::Ident(ref n) if n == "macro_rules"
            )
            && matches!(tokens[i + 1].kind, TokenKind::Bang)
            && matches!(tokens[i + 2].kind, TokenKind::Ident(_))
            && matches!(tokens[i + 3].kind, TokenKind::LBrace);
        if is_def {
            let name = if let TokenKind::Ident(n) = &tokens[i + 2].kind {
                n.clone()
            } else {
                unreachable!()
            };
            // Collect the body between the outer `{` and its matching `}`.
            let mut depth = 0i32;
            let mut j = i + 3;
            loop {
                match tokens[j].kind {
                    TokenKind::LBrace => depth += 1,
                    TokenKind::RBrace => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
                if j >= tokens.len() {
                    return Err(Diag::error(
                        "macro_rules!: unterminated definition body",
                        None,
                    ));
                }
            }
            let body = tokens[i + 4..j].to_vec();
            let def = parse_def(&name, &body)?;
            table.defs.insert(name, def);
            i = j + 1;
            continue;
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    Ok((table, out))
}

fn parse_def(name: &str, body: &[Token]) -> ParseResult<MacroRulesDef> {
    let arms_tts = split_top_level(body, TokenKind::Semicolon);
    let mut arms = Vec::new();
    for arm in arms_tts {
        if arm.is_empty() {
            continue;
        }
        let (matcher_tts, template_tts) = match split_at_top_level(&arm, TokenKind::FatArrow) {
            Some((l, r)) => (l, r),
            None => {
                return Err(Diag::error(
                    format!("macro_rules! `{name}`: arm is missing `=>`"),
                    None,
                ))
            }
        };
        let matcher = parse_matcher(&matcher_tts)?;
        let template = parse_tmpl(&template_tts);
        arms.push(Arm { matcher, template });
    }
    Ok(MacroRulesDef { arms })
}

/// Split a token list at top-level occurrences of `kind` (not inside any
/// `(...)`/`[...]`/`{...}` group), returning the pieces.
fn split_top_level(tts: &[Token], kind: TokenKind) -> Vec<Vec<Token>> {
    let mut res = Vec::new();
    let mut cur = Vec::new();
    let mut depth = 0i32;
    for t in tts {
        match t.kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {}
        }
        if depth == 0 && t.kind == kind {
            res.push(std::mem::take(&mut cur));
        } else {
            cur.push(t.clone());
        }
    }
    res.push(cur);
    res
}

/// Return the first top-level `kind` occurrence, splitting into `(before, after)`.
fn split_at_top_level(tts: &[Token], kind: TokenKind) -> Option<(Vec<Token>, Vec<Token>)> {
    let mut depth = 0i32;
    for idx in 0..tts.len() {
        match tts[idx].kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {}
        }
        if depth == 0 && tts[idx].kind == kind {
            return Some((tts[..idx].to_vec(), tts[idx + 1..].to_vec()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Matcher parsing
// ---------------------------------------------------------------------------

fn parse_frag(s: &str) -> Option<Frag> {
    match s {
        "tt" => Some(Frag::Tt),
        "ident" => Some(Frag::Ident),
        "expr" => Some(Frag::Expr),
        "ty" => Some(Frag::Ty),
        "pat" => Some(Frag::Pat),
        "stmt" => Some(Frag::Stmt),
        "block" => Some(Frag::Block),
        "item" => Some(Frag::Item),
        "literal" => Some(Frag::Literal),
        "path" => Some(Frag::Path),
        _ => None,
    }
}

fn parse_matcher(tts: &[Token]) -> ParseResult<Vec<Matcher>> {
    let mut stack: Vec<Vec<Matcher>> = vec![Vec::new()];
    let mut i = 0;
    while i < tts.len() {
        match &tts[i].kind {
            TokenKind::Dollar => {
                // Repetition open: `$(`
                let next_open = matches!(
                    tts.get(i + 1),
                    Some(Token {
                        kind: TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket,
                        ..
                    })
                );
                if next_open {
                    let (inner, sep, kind, next) = parse_rep(tts, i)?;
                    stack.last_mut().unwrap().push(Matcher::Rep(inner, sep, kind));
                    i = next;
                    continue;
                }
                // Metavariable `$name` (optionally `:frag`).
                if let Some(Token {
                    kind: TokenKind::Ident(name),
                    ..
                }) = tts.get(i + 1)
                {
                    let mut frag = Frag::Tt;
                    let mut j = i + 2;
                    if let Some(Token {
                        kind: TokenKind::Colon,
                        ..
                    }) = tts.get(j)
                    {
                        if let Some(Token {
                            kind: TokenKind::Ident(f),
                            ..
                        }) = tts.get(j + 1)
                        {
                            frag = parse_frag(f).ok_or_else(|| {
                                Diag::error(
                                    format!("macro_rules!: unknown fragment specifier `{f}`"),
                                    None,
                                )
                            })?;
                            j += 2;
                        }
                    }
                    stack
                        .last_mut()
                        .unwrap()
                        .push(Matcher::Var(name.clone(), frag));
                    i = j;
                    continue;
                }
                // Bare `$` — match literally.
                stack
                    .last_mut()
                    .unwrap()
                    .push(Matcher::Tok(TokenKind::Dollar));
                i += 1;
                continue;
            }
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => {
                stack.push(Vec::new());
                i += 1;
                continue;
            }
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                let inner = stack.pop().ok_or_else(|| {
                    Diag::error("macro_rules!: unbalanced `)` in matcher", None)
                })?;
                let delim = delim_of_close(&tts[i].kind).ok_or_else(|| {
                    Diag::error("macro_rules!: unbalanced delimiter in matcher", None)
                })?;
                if let Some(parent) = stack.last_mut() {
                    parent.push(Matcher::Group(delim, inner));
                } else {
                    return Err(Diag::error(
                        "macro_rules!: unbalanced `)` in matcher",
                        None,
                    ));
                }
                i += 1;
                continue;
            }
            other => {
                stack.last_mut().unwrap().push(Matcher::Tok(other.clone()));
                i += 1;
                continue;
            }
        }
    }
    if stack.len() != 1 {
        return Err(Diag::error(
            "macro_rules!: unbalanced delimiter in matcher",
            None,
        ));
    }
    Ok(stack.pop().unwrap())
}

/// Parse a `$( ... )*` / `+` / `?` group starting at `i` (the `$`). Returns
/// `(inner, separator, kind, next_index)`.
fn parse_rep(
    tts: &[Token],
    i: usize,
) -> ParseResult<(Vec<Matcher>, Option<TokenKind>, RepKind, usize)> {
    // i = Dollar, i+1 = open delimiter.
    let mut depth = 0i32;
    let mut j = i + 1;
    loop {
        match tts[j].kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        j += 1;
        if j >= tts.len() {
            return Err(Diag::error(
                "macro_rules!: unterminated `$(` repetition",
                None,
            ));
        }
    }
    // j points at the matching close delimiter.
    let inner_tts = &tts[i + 2..j];
    let inner = parse_matcher(inner_tts)?;
    let mut next = j + 1;
    let mut sep = None;
    let kind = match tts.get(next) {
        Some(Token {
            kind: TokenKind::Star,
            ..
        }) => RepKind::ZeroOrMore,
        Some(Token {
            kind: TokenKind::Plus,
            ..
        }) => RepKind::OneOrMore,
        Some(Token {
            kind: TokenKind::Question,
            ..
        }) => RepKind::ZeroOrOne,
        Some(Token { kind: tok, .. }) => {
            // Separator token, then the repetition operator.
            sep = Some(tok.clone());
            next += 1;
            match tts.get(next) {
                Some(Token {
                    kind: TokenKind::Star,
                    ..
                }) => RepKind::ZeroOrMore,
                Some(Token {
                    kind: TokenKind::Plus,
                    ..
                }) => RepKind::OneOrMore,
                Some(Token {
                    kind: TokenKind::Question,
                    ..
                }) => RepKind::ZeroOrOne,
                _ => {
                    return Err(Diag::error(
                        "macro_rules!: expected `*`, `+`, or `?` after repetition",
                        None,
                    ))
                }
            }
        }
        None => {
            return Err(Diag::error(
                "macro_rules!: expected repetition operator after `)`",
                None,
            ))
        }
    };
    next += 1;
    Ok((inner, sep, kind, next))
}

// ---------------------------------------------------------------------------
// Template parsing
// ---------------------------------------------------------------------------

fn parse_tmpl(tts: &[Token]) -> Vec<TmplEl> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tts.len() {
        match &tts[i].kind {
            TokenKind::Dollar => {
                let next_open = matches!(
                    tts.get(i + 1),
                    Some(Token {
                        kind: TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket,
                        ..
                    })
                );
                if next_open {
                    let (body, sep, kind, next) = parse_rep_tmpl(tts, i);
                    out.push(TmplEl::Rep(body, sep, kind));
                    i = next;
                    continue;
                }
                if let Some(Token {
                    kind: TokenKind::Ident(name),
                    ..
                }) = tts.get(i + 1)
                {
                    out.push(TmplEl::Var(name.clone()));
                    i += 2;
                    continue;
                }
                out.push(TmplEl::TT(tts[i].clone()));
                i += 1;
                continue;
            }
            tok => {
                out.push(TmplEl::TT(Token {
                    kind: tok.clone(),
                    span: tts[i].span,
                }));
                i += 1;
            }
        }
    }
    out
}

fn parse_rep_tmpl(tts: &[Token], i: usize) -> (Vec<TmplEl>, Option<TokenKind>, RepKind, usize) {
    let mut depth = 0i32;
    let mut j = i + 1;
    loop {
        match tts[j].kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        j += 1;
        if j >= tts.len() {
            return (Vec::new(), None, RepKind::ZeroOrMore, tts.len());
        }
    }
    let inner = parse_tmpl(&tts[i + 2..j]);
    let mut next = j + 1;
    let mut sep = None;
    let kind = match tts.get(next) {
        Some(Token {
            kind: TokenKind::Star,
            ..
        }) => RepKind::ZeroOrMore,
        Some(Token {
            kind: TokenKind::Plus,
            ..
        }) => RepKind::OneOrMore,
        Some(Token {
            kind: TokenKind::Question,
            ..
        }) => RepKind::ZeroOrOne,
        Some(Token { kind: tok, .. }) => {
            sep = Some(tok.clone());
            next += 1;
            match tts.get(next) {
                Some(Token {
                    kind: TokenKind::Star,
                    ..
                }) => RepKind::ZeroOrMore,
                Some(Token {
                    kind: TokenKind::Plus,
                    ..
                }) => RepKind::OneOrMore,
                Some(Token {
                    kind: TokenKind::Question,
                    ..
                }) => RepKind::ZeroOrOne,
                _ => RepKind::ZeroOrMore,
            }
        }
        None => RepKind::ZeroOrMore,
    };
    next += 1;
    (inner, sep, kind, next)
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

fn is_literal(k: &TokenKind) -> bool {
    matches!(
        k,
        TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Str(_)
            | TokenKind::Char(_)
            | TokenKind::True
            | TokenKind::False
    )
}

fn frag_stoppers(frag: Frag) -> &'static [TokenKind] {
    match frag {
        Frag::Expr => &[TokenKind::Comma, TokenKind::Semicolon, TokenKind::FatArrow],
        Frag::Ty => &[
            TokenKind::Comma,
            TokenKind::Semicolon,
            TokenKind::FatArrow,
            TokenKind::Arrow,
        ],
        Frag::Pat => &[
            TokenKind::Comma,
            TokenKind::Semicolon,
            TokenKind::FatArrow,
            TokenKind::Eq,
            TokenKind::Arrow,
        ],
        Frag::Stmt => &[TokenKind::Semicolon, TokenKind::FatArrow],
        Frag::Item => &[],
        Frag::Path => &[
            TokenKind::Comma,
            TokenKind::Semicolon,
            TokenKind::FatArrow,
            TokenKind::LParen,
            TokenKind::LBrace,
            TokenKind::LBracket,
            TokenKind::Colon,
            TokenKind::Arrow,
            TokenKind::Eq,
            TokenKind::RParen,
            TokenKind::RBrace,
            TokenKind::RBracket,
        ],
        _ => &[],
    }
}

/// Capture `frag` from `input` starting at `*pos`, returning the number of
/// tokens consumed (advancing `*pos` on success).
fn capture_frag(frag: Frag, input: &[Token], pos: &mut usize) -> Option<usize> {
    if *pos >= input.len() {
        return None;
    }
    let k = &input[*pos].kind;
    match frag {
        Frag::Tt => {
            // A `tt` captures a single token, or a whole delimited group as one
            // tree when the cursor sits on an opening delimiter.
            if delim_of_open(k).is_some() {
                Some(balanced_len(input, *pos))
            } else {
                Some(1)
            }
        }
        Frag::Ident => {
            if matches!(k, TokenKind::Ident(_)) {
                Some(1)
            } else {
                None
            }
        }
        Frag::Block => {
            if matches!(k, TokenKind::LBrace) {
                Some(balanced_len(input, *pos))
            } else {
                None
            }
        }
        Frag::Literal => {
            if is_literal(k) {
                Some(1)
            } else {
                None
            }
        }
        _ => {
            if delim_of_open(k).is_some() {
                Some(balanced_len(input, *pos))
            } else {
                let stoppers = frag_stoppers(frag);
                let mut depth = 0i32;
                let mut j = *pos;
                while j < input.len() {
                    match input[j].kind {
                        TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
                        TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                        }
                        _ => {}
                    }
                    if depth == 0 && stoppers.contains(&input[j].kind) {
                        break;
                    }
                    j += 1;
                }
                if j == *pos {
                    None
                } else {
                    Some(j - *pos)
                }
            }
        }
    }
}

/// Length of the balanced delimited group starting at `pos` (which must be an
/// opening delimiter).
fn balanced_len(input: &[Token], pos: usize) -> usize {
    let mut depth = 0i32;
    let mut j = pos;
    loop {
        match input[j].kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                depth -= 1;
                if depth == 0 {
                    j += 1;
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    j - pos
}

/// Match a sequence of matcher elements against `input`, advancing `pos`.
fn match_m(
    ms: &[Matcher],
    input: &[Token],
    pos: &mut usize,
    binds: &mut Bindings,
    rep_acc: &mut Option<HashMap<String, Vec<Vec<Token>>>>,
) -> bool {
    let mut i = 0;
    while i < ms.len() {
        let snap = binds.clone();
        let sub_snap = rep_acc.clone();
        let ok = match &ms[i] {
            Matcher::Tok(t) => {
                if *pos < input.len() && &input[*pos].kind == t {
                    *pos += 1;
                    true
                } else {
                    false
                }
            }
            Matcher::Var(name, frag) => match capture_frag(*frag, input, pos) {
                Some(n) => {
                    let toks = input[*pos..*pos + n].to_vec();
                    *pos += n;
                    if let Some(ra) = rep_acc {
                        ra.entry(name.clone()).or_default().push(toks);
                    } else {
                        binds.insert(name.clone(), Binding::Single(toks));
                    }
                    true
                }
                None => false,
            },
            Matcher::Group(d, inner) => {
                if *pos < input.len() && input[*pos].kind == open_kind(*d) {
                    *pos += 1;
                    let mut p2 = *pos;
                    let ok = match_m(inner, input, &mut p2, binds, rep_acc)
                        && p2 < input.len()
                        && input[p2].kind == close_kind(*d);
                    if ok {
                        *pos = p2 + 1;
                        true
                    } else {
                        *pos -= 1;
                        false
                    }
                } else {
                    false
                }
            }
            Matcher::Rep(inner, sep, kind) => {
                match_rep(inner, sep.clone(), *kind, input, pos, binds, rep_acc)
            }
        };
        if !ok {
            *binds = snap;
            *rep_acc = sub_snap;
            return false;
        }
        i += 1;
    }
    true
}

/// Match a repetition, accumulating per-iteration captures for any metavariable
/// declared inside the repetition.
fn match_rep(
    inner: &[Matcher],
    sep: Option<TokenKind>,
    kind: RepKind,
    input: &[Token],
    pos: &mut usize,
    binds: &mut Bindings,
    rep_acc: &mut Option<HashMap<String, Vec<Vec<Token>>>>,
) -> bool {
    let min = match kind {
        RepKind::ZeroOrOne | RepKind::ZeroOrMore => 0,
        RepKind::OneOrMore => 1,
    };
    let start = *pos;
    let snap = binds.clone();
    let outer_snap = rep_acc.clone();
    let mut local: HashMap<String, Vec<Vec<Token>>> = HashMap::new();
    let mut count = 0usize;
    loop {
        if count > 0 {
            if let Some(s) = sep.as_ref() {
                if *pos < input.len() && input[*pos].kind == *s {
                    *pos += 1;
                } else {
                    break;
                }
            }
        }
        let mut sub_opt: Option<HashMap<String, Vec<Vec<Token>>>> = Some(HashMap::new());
        let save = *pos;
        if match_m(inner, input, pos, binds, &mut sub_opt) {
            count += 1;
            if let Some(map) = sub_opt.take() {
                for (k, v) in map {
                    if let Some(one) = v.into_iter().next() {
                        local.entry(k).or_default().push(one);
                    }
                }
            }
        } else {
            *pos = save;
            break;
        }
    }
    if count >= min {
        *binds = snap;
        for (k, vec) in local {
            if let Some(ra) = rep_acc {
                ra.entry(k).or_default().extend(vec);
            } else {
                binds.insert(k, Binding::Repeat(vec));
            }
        }
        true
    } else {
        *binds = snap;
        *rep_acc = outer_snap;
        *pos = start;
        false
    }
}

// ---------------------------------------------------------------------------
// Template expansion
// ---------------------------------------------------------------------------

fn render(tmpl: &[TmplEl], binds: &Bindings) -> Vec<Token> {
    let mut out = Vec::new();
    render_el(tmpl, binds, &mut None, &mut out);
    out
}

fn render_el(els: &[TmplEl], binds: &Bindings, index: &mut Option<usize>, out: &mut Vec<Token>) {
    for el in els {
        match el {
            TmplEl::TT(t) => out.push(t.clone()),
            TmplEl::Var(name) => emit_var(name, binds, index, out),
            TmplEl::Rep(body, sep, _kind) => {
                let n = rep_count(body, binds);
                for k in 0..n {
                    let mut idx = Some(k);
                    render_el(body, binds, &mut idx, out);
                    if k + 1 < n {
                        if let Some(s) = sep {
                            out.push(Token {
                                kind: s.clone(),
                                span: Span::default(),
                            });
                        }
                    }
                }
            }
        }
    }
}

fn emit_var(name: &str, binds: &Bindings, index: &Option<usize>, out: &mut Vec<Token>) {
    if let Some(b) = binds.get(name) {
        match b {
            Binding::Single(toks) => out.extend(toks.iter().cloned()),
            Binding::Repeat(vec) => {
                let v = if let Some(idx) = *index {
                    vec.get(idx)
                } else {
                    vec.first()
                };
                if let Some(toks) = v {
                    out.extend(toks.iter().cloned());
                }
            }
        }
    }
}

fn rep_count(els: &[TmplEl], binds: &Bindings) -> usize {
    for el in els {
        match el {
            TmplEl::Var(name) => {
                if let Some(Binding::Repeat(v)) = binds.get(name) {
                    return v.len();
                }
            }
            TmplEl::Rep(sub, _, _) => {
                let n = rep_count(sub, binds);
                if n > 0 {
                    return n;
                }
            }
            _ => {}
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Pipeline integration
// ---------------------------------------------------------------------------

/// Expand every `name!(...)` call-site in `tokens` using the `macro_rules!`
/// table, returning the rewritten token stream (ready for the parser).
pub fn expand_macro_rules_calls(tokens: &[Token], table: &MacroRulesTable) -> Vec<Token> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let is_call = tokens.len() >= i + 3
            && matches!(tokens[i].kind, TokenKind::Ident(_))
            && matches!(tokens[i + 1].kind, TokenKind::Bang)
            && delim_of_open(&tokens[i + 2].kind).is_some();
        if is_call {
            if let TokenKind::Ident(name) = &tokens[i].kind {
                if let Some(def) = table.defs.get(name) {
                    let mut depth = 0i32;
                    let mut j = i + 2;
                    loop {
                        match tokens[j].kind {
                            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
                            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                                depth -= 1;
                                if depth == 0 {
                                    j += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    // Body includes the call-site delimiters.
                    let body = tokens[i + 2..j].to_vec();
                    if let Some(produced) = def.expand_to_tokens(&body) {
                        // Recursively expand any macro calls emitted in output.
                        let produced2 = expand_macro_rules_calls(&produced, table);
                        out.extend(produced2);
                        i = j;
                        continue;
                    }
                }
            }
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;

    fn compile_macro(src: &str) -> ast::Program {
        crate::pipeline::compile(src).expect("compile with macro_rules!")
    }

    fn main_body(prog: &ast::Program) -> &ast::Block {
        for item in &prog.items {
            if let ast::ItemKind::Function(f) = &item.kind {
                if f.name.node == "main" {
                    return &f.body;
                }
            }
        }
        panic!("no main function found");
    }

    #[test]
    fn extracts_macro_rules_def() {
        let src = "macro_rules! m { ($x:expr) => { $x } } fn main(){}";
        let toks = crate::lexer::tokenize(src);
        let (table, rest) = extract_macro_rules(&toks).unwrap();
        assert!(table.defs.contains_key("m"));
        assert!(!rest.iter().any(|t| matches!(
            t.kind,
            TokenKind::Ident(ref n) if n == "macro_rules"
        )));
    }

    #[test]
    fn ident_fragment_substitution() {
        let src = "macro_rules! id { ($x:ident) => { $x } }
                   fn main() { let y = id!(foo); }";
        let prog = compile_macro(src);
        let body = main_body(&prog);
        let init = &body.stmts[0];
        assert!(format!("{:?}", init).contains("foo"));
    }

    #[test]
    fn expr_repetition_array() {
        let src = "macro_rules! arr { ($($x:expr),*) => { [ $($x),* ] } }
                   fn main() { let a = arr!(1, 2, 3); }";
        let prog = compile_macro(src);
        let body = main_body(&prog);
        let init = &body.stmts[0];
        let s = format!("{:?}", init);
        assert!(s.contains("Array"), "{s}");
        assert!(s.contains("Int(1"), "{s}");
        assert!(s.contains("Int(3"), "{s}");
    }

    #[test]
    fn stmt_repetition_in_block() {
        let src = "macro_rules! seq { ($($s:stmt);*) => { $($s);* } }
                   fn main() { seq!(let x = 1; let y = 2;); }";
        let prog = compile_macro(src);
        let body = main_body(&prog);
        let init = &body.stmts[0];
        let s = format!("{:?}", init);
        assert!(s.contains("x"), "expected `x` binding: {s}");
        assert!(s.contains("y"), "expected `y` binding: {s}");
    }

    #[test]
    fn nested_macro_call_in_arg() {
        let src = "macro_rules! inner { ($x:expr) => { $x } }
                   macro_rules! outer { ($x:expr) => { inner!($x) } }
                   fn main() { let a = outer!(42); }";
        let prog = compile_macro(src);
        let body = main_body(&prog);
        let init = &body.stmts[0];
        assert!(format!("{:?}", init).contains("Int(42"));
    }
}
