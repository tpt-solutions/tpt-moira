//! Clotho recursive-descent parser for the Phase-1 Rust subset.
// MARKER_KILO_EDITS_ACTIVE
//!
//! The parser is a Pratt-style precedence parser driven by a cursor over the
//! token stream produced by [`crate::lexer::Lexer`]. It is single-pass with a
//! bounded lookahead of two tokens.

use crate::ast::*;
use crate::error::{Diag, ParseResult};
use crate::token::{Span, Token, TokenKind};

/// The name an item introduces, for the nested-item clash check.
fn item_name(item: &Item) -> Option<(&str, Span)> {
    match &item.kind {
        ItemKind::Function(f) => Some((f.name.node.as_str(), f.name.span)),
        ItemKind::Struct(s) => Some((s.name.node.as_str(), s.name.span)),
        ItemKind::Enum(e) => Some((e.name.node.as_str(), e.name.span)),
        ItemKind::Const(c) => Some((c.name.node.as_str(), c.name.span)),
        ItemKind::Static(s) => Some((s.name.node.as_str(), s.name.span)),
        ItemKind::TypeAlias(t) => Some((t.name.node.as_str(), t.name.span)),
        _ => None,
    }
}

/// Keep only the nested (hoisted) items whose name is unique in the program.
///
/// Two nested items with one name (a local `fn imp` in each of two functions) cannot
/// both be hoisted without renaming. Both are dropped rather than one binding the
/// other's calls: a call to a dropped item then fails to resolve, which the checker
/// reports, instead of silently calling the wrong function.
fn unique_hoisted(items: &[Item], hoisted: Vec<Item>) -> Vec<Item> {
    let mut count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for item in items.iter().chain(hoisted.iter()) {
        if let Some((name, _)) = item_name(item) {
            *count.entry(name.to_string()).or_insert(0) += 1;
        }
    }
    hoisted
        .into_iter()
        .filter(|item| match item_name(item) {
            Some((name, _)) => count.get(name) == Some(&1),
            None => true,
        })
        .collect()
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Items declared inside function bodies, moved to the top level by
    /// `parse_program` (see `stmt_for_item`).
    hoisted: Vec<Item>,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            hoisted: Vec::new(),
        }
    }

    pub fn from_source(src: &str) -> Self {
        Parser::new(crate::lexer::tokenize(src))
    }

    /// Split a macro-invocation token stream on top-level commas and parse each
    /// segment as an expression. Used to give the builtin `println!`-family
    /// macros real (type-checked, lowerable) arguments.
    pub fn parse_macro_args(tokens: Vec<Token>) -> Vec<Expr> {
        let mut args = Vec::new();
        let mut cur: Vec<Token> = Vec::new();
        let mut depth = 0i32;
        for t in tokens {
            match &t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                    depth += 1;
                    cur.push(t);
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth -= 1;
                    cur.push(t);
                }
                TokenKind::Comma if depth == 0 => {
                    if !cur.is_empty() {
                        if let Ok(e) = Parser::segment(std::mem::take(&mut cur)).parse_expr() {
                            args.push(e);
                        }
                    }
                }
                _ => cur.push(t),
            }
        }
        if !cur.is_empty() {
            if let Ok(e) = Parser::segment(cur).parse_expr() {
                args.push(e);
            }
        }
        args
    }

    /// Split the arguments of `matches!(expr, pattern)` into the scrutinee
    /// expression and the pattern. `None` when either part does not parse.
    pub fn parse_matches_args(tokens: Vec<Token>) -> Option<(Expr, Pat)> {
        let mut cur: Vec<Token> = Vec::new();
        let mut rest: Vec<Token> = Vec::new();
        let mut depth = 0i32;
        let mut split = false;
        for t in tokens {
            if split {
                rest.push(t);
                continue;
            }
            match &t.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                    depth += 1;
                    cur.push(t);
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth -= 1;
                    cur.push(t);
                }
                TokenKind::Comma if depth == 0 => split = true,
                _ => cur.push(t),
            }
        }
        let scrutinee = Parser::segment(cur).parse_expr().ok()?;
        let pat = Parser::segment(rest).parse_pat().ok()?;
        Some((scrutinee, pat))
    }

    /// Build a parser over one comma-separated macro segment. Segments split off
    /// the full token stream do not end in the tokenizer's `Eof`, and the cursor
    /// (`peek`/`bump`) relies on a terminating `Eof` to stop: without it, a
    /// segment such as `s.1` (tuple index) makes the postfix loop spin forever,
    /// growing memory until the process is killed.
    pub fn segment(mut tokens: Vec<Token>) -> Self {
        if !matches!(tokens.last().map(|t| &t.kind), Some(TokenKind::Eof)) {
            let end = tokens.last().map(|t| t.span.end).unwrap_or(0);
            tokens.push(Token {
                kind: TokenKind::Eof,
                span: Span::new(end, end),
            });
        }
        Parser::new(tokens)
    }

    // ---- low-level token cursor ----

    fn peek(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .unwrap_or_else(|| self.tokens.last().unwrap())
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn bump(&mut self) -> Token {
        // At end-of-input, re-return the final (Eof) token instead of
        // panicking — postfix parsing (`expr.0` at EOF) can reach here after
        // the cursor already moved past the last token.
        let t = self
            .tokens
            .get(self.pos)
            .cloned()
            .unwrap_or_else(|| self.tokens.last().unwrap().clone());
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Eof)
    }

    /// Whether the token *after* the current one is `:`. Used to distinguish a
    /// named field/param (`x: T`) from a bare (unnamed tuple) type in an enum
    /// variant or tuple struct.
    fn next_is_colon(&self) -> bool {
        matches!(
            self.tokens.get(self.pos + 1).map(|t| &t.kind),
            Some(TokenKind::Colon)
        )
    }

    fn check(&self, k: &TokenKind) -> bool {
        self.peek_kind() == k
    }

    fn eat(&mut self, k: &TokenKind) -> bool {
        if self.check(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, k: TokenKind) -> ParseResult<Token> {
        if self.check(&k) {
            Ok(self.bump())
        } else {
            Err(Diag::error(
                format!("expected {}, found {}", k, self.peek_kind()),
                Some(self.peek().span),
            ))
        }
    }

    fn expect_ident(&mut self) -> ParseResult<Spanned<String>> {
        let t = self.bump();
        match &t.kind {
            TokenKind::Ident(name) => Ok(Spanned::new(name.clone(), t.span)),
            // Primitive type keywords (`str`, `char`, ...) name modules in paths
            // (`core::str`, `core::char`).
            k if is_primitive_keyword(k) => Ok(Spanned::new(primitive_name(k), t.span)),
            _ => Err(Diag::error(
                format!("expected identifier, found {}", t.kind),
                Some(t.span),
            )),
        }
    }

    // ---- top level ----

    pub fn parse_program(&mut self) -> ParseResult<Program> {
        let mut items = Vec::new();
        while !self.at_eof() {
            if let Some(item) = self.parse_item_opt()? {
                items.push(item);
            }
        }
        let hoisted = std::mem::take(&mut self.hoisted);
        let more = unique_hoisted(&items, hoisted);
        items.extend(more);
        Ok(Program { items })
    }

    /// The statement for an item written inside a block. `use` declarations stay
    /// as statements (imports are not resolved); every other item moves to the top
    /// level, where it can be checked and lowered. A nested item's scope is not
    /// modeled, so its name must be unique among the program's items.
    fn stmt_for_item(&mut self, item: Item) -> Stmt {
        if matches!(item.kind, ItemKind::Use(_)) {
            return Stmt {
                kind: StmtKind::Item(item),
            };
        }
        self.hoisted.push(item);
        let at = self.peek().span.start;
        Stmt {
            kind: StmtKind::Semi(Expr {
                kind: ExprKind::Tuple(Vec::new()),
                span: Span::new(at, at),
            }),
        }
    }

    fn parse_visibility(&mut self) -> Visibility {
        if self.eat(&TokenKind::Pub) {
            // Optional visibility restriction: `pub(crate)`, `pub(super)`,
            // `pub(in path)`. The Phase-1 subset does not model fine-grained
            // visibility, so we simply consume the restriction.
            if self.eat(&TokenKind::LParen) {
                while !self.eat(&TokenKind::RParen) {
                    if self.at_eof() {
                        break;
                    }
                    self.bump();
                }
            }
            Visibility::Pub
        } else {
            Visibility::Private
        }
    }

    /// Whether the next tokens begin an item the Phase-1 subset drops rather than
    /// models (trait/union definitions, `macro_rules!` definitions, `extern`
    /// blocks). A leading `unsafe` is consumed in the lookahead so that
    /// `unsafe trait` is also skipped (while `unsafe fn`/`unsafe impl` are kept).
    fn is_skippable_item(&self) -> bool {
        let mut i = self.pos;
        if matches!(self.tokens.get(i).map(|t| &t.kind), Some(TokenKind::Unsafe)) {
            i += 1;
        }
        matches!(
            self.tokens.get(i).map(|t| &t.kind),
            Some(TokenKind::Ident(n))
                if n == "trait" || n == "macro_rules" || n == "extern" || n == "union"
        )
    }

    /// Whether the next tokens are an item-position macro invocation: a path
    /// (`a::b::name`) followed by `!`. These are not expanded at item level.
    fn is_item_macro_call(&self) -> bool {
        let mut i = self.pos;
        loop {
            match self.tokens.get(i).map(|t| &t.kind) {
                Some(TokenKind::Ident(_)) => {}
                _ => return false,
            }
            match self.tokens.get(i + 1).map(|t| &t.kind) {
                Some(TokenKind::Bang) => return true,
                Some(TokenKind::ColonColon) => i += 2,
                _ => return false,
            }
        }
    }

    /// Consume a complete item we don't model (balanced braces, or up to the next
    /// `;` at brace-depth 0, or EOF).
    fn skip_rest_of_item(&mut self) -> ParseResult<()> {
        // `depth` counts every open delimiter, so a `;` inside `[T; N]` or `(..)`
        // does not end the item.
        let mut depth = 0i32;
        loop {
            match self.peek_kind() {
                TokenKind::Eof => break,
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => {
                    depth += 1;
                    self.bump();
                }
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                    if depth == 0 {
                        // Closes an enclosing scope; leave it for the caller.
                        break;
                    }
                    depth -= 1;
                    let closes_item = matches!(self.peek_kind(), TokenKind::RBrace) && depth == 0;
                    self.bump();
                    if closes_item {
                        break;
                    }
                }
                TokenKind::Semicolon if depth == 0 => {
                    self.bump();
                    break;
                }
                _ => {
                    self.bump();
                }
            }
        }
        Ok(())
    }

    pub(crate) fn parse_item(&mut self) -> ParseResult<Item> {
        loop {
            if let Some(item) = self.parse_item_opt()? {
                return Ok(item);
            }
        }
    }

    /// Parse one item, or consume and drop an unmodeled one and return `None`.
    /// Loops over items use this so their own end-of-block check sees the
    /// position after the dropped item (e.g. end of file).
    pub(crate) fn parse_item_opt(&mut self) -> ParseResult<Option<Item>> {
        // A stray `;` is left behind by an item-position macro call (`m!();`).
        if self.eat(&TokenKind::Semicolon) {
            return Ok(None);
        }
        let attributes = self.parse_attributes()?;
        // Attributes with no item after them (e.g. on a `macro_rules!` definition
        // that was extracted before parsing) attach to nothing and are dropped.
        if self.at_eof() || self.check(&TokenKind::RBrace) {
            return Ok(None);
        }
        let visibility = self.parse_visibility();
        // An item whose `#[cfg(..)]` is false for this target is not part of the
        // program at all (its `mod` file need not exist).
        if !crate::cfg::attrs_enabled(&attributes) {
            self.skip_rest_of_item()?;
            return Ok(None);
        }
        // Items the Phase-1 subset does not yet model (e.g. `trait`,
        // `macro_rules!`, `extern`, item-position macro calls) are dropped
        // silently so the program still parses. The self-host probe tolerates
        // this; full modeling is a tracked Phase-3/4 follow-up.
        if self.is_skippable_item() || self.is_item_macro_call() {
            self.skip_rest_of_item()?;
            return Ok(None);
        }
        let kind = self.parse_item_kind(attributes.clone())?;
        Ok(Some(Item {
            visibility,
            kind,
            attributes,
        }))
    }

    fn parse_item_kind(&mut self, attributes: Vec<Attribute>) -> ParseResult<ItemKind> {
        // `unsafe fn` / `unsafe impl` keep their unsafe-ness; `unsafe trait` is
        // dropped by the caller's `is_skippable_item` check.
        let is_unsafe = self.eat(&TokenKind::Unsafe);
        // `impl` is a keyword token; detect it before the name-based checks below.
        if let TokenKind::Impl = self.peek_kind() {
            return Ok(ItemKind::Impl(self.parse_impl(is_unsafe)?));
        }
        // `mod` / `type` still lex as identifiers, so detect them by name.
        if let TokenKind::Ident(name) = self.peek_kind() {
            if name == "mod" {
                return Ok(ItemKind::Module(self.parse_module()?));
            }
            if name == "type" {
                return Ok(ItemKind::TypeAlias(self.parse_type_alias()?));
            }
        }
        // `const fn` is a function; the `const` qualifier is not modeled.
        if matches!(self.peek_kind(), TokenKind::Const)
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Fn)
            )
        {
            self.bump();
            return Ok(ItemKind::Function(self.parse_function(is_unsafe)?));
        }
        match self.peek_kind() {
            TokenKind::Fn | TokenKind::Async => {
                Ok(ItemKind::Function(self.parse_function(is_unsafe)?))
            }
            TokenKind::Struct => Ok(ItemKind::Struct(self.parse_struct(attributes)?)),
            TokenKind::Enum => Ok(ItemKind::Enum(self.parse_enum(attributes)?)),
            TokenKind::Const => Ok(ItemKind::Const(self.parse_const()?)),
            TokenKind::Static => Ok(ItemKind::Static(self.parse_static()?)),
            TokenKind::Use => Ok(ItemKind::Use(self.parse_use()?)),
            other => Err(Diag::error(
                format!("expected item, found {}", other),
                Some(self.peek().span),
            )),
        }
    }

    fn parse_type_alias(&mut self) -> ParseResult<TypeAliasDef> {
        // Consume the `type` identifier.
        self.bump();
        let name = self.expect_ident()?;
        let generics = self.parse_generics()?;
        self.expect(TokenKind::Eq)?;
        let ty = self.parse_type()?;
        self.eat(&TokenKind::Semicolon);
        Ok(TypeAliasDef { name, generics, ty })
    }

    fn parse_function(&mut self, is_unsafe: bool) -> ParseResult<Function> {
        let is_async = self.eat(&TokenKind::Async);
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        let generics = self.parse_generics()?;
        self.expect(TokenKind::LParen)?;
        let params = self.parse_delimited(TokenKind::RParen, |s| s.parse_param())?;
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let generics = self.parse_where_clause(generics)?;
        let body = self.parse_block()?;
        Ok(Function {
            name,
            generics,
            is_async,
            is_unsafe,
            params,
            ret,
            body,
        })
    }

    /// Parse a single function parameter. Also handles the method receiver
    /// `self`, `&self`, and `&mut self`, which carry no `: Type` annotation
    /// (they desugar to `self: Self` / `self: &Self`).
    fn parse_param(&mut self) -> ParseResult<Param> {
        if self.peek_is_receiver() {
            let is_ref = self.eat(&TokenKind::Amp);
            let is_mut = if is_ref {
                self.eat(&TokenKind::Mut)
            } else {
                // By-value mutable receiver: `mut self`.
                self.eat(&TokenKind::Mut)
            };
            let name = self.expect_ident()?; // `self`
                                             // A typed receiver `self: Pin<&mut Self>` keeps its written type.
            if self.eat(&TokenKind::Colon) {
                let ty = self.parse_type()?;
                return Ok(Param {
                    pat: Pat::Ident(name),
                    ty,
                });
            }
            let span = name.span;
            let self_ty = Type {
                kind: TypeKind::Path(
                    Path {
                        segments: vec![Spanned::new("Self".to_string(), span)],
                    },
                    Vec::new(),
                ),
                span,
            };
            let ty = if is_ref {
                let k = if is_mut {
                    TypeKind::RefMut(Box::new(self_ty.clone()))
                } else {
                    TypeKind::Ref(Box::new(self_ty.clone()))
                };
                Type { kind: k, span }
            } else {
                self_ty
            };
            return Ok(Param {
                pat: Pat::Ident(name),
                ty,
            });
        }
        let pat = self.parse_pat()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        Ok(Param { pat, ty })
    }

    /// Whether the next tokens form a method receiver (`self`, `&self`,
    /// `&mut self`).
    fn peek_is_receiver(&self) -> bool {
        match self.peek_kind() {
            TokenKind::Ident(name) => name.as_str() == "self",
            TokenKind::Mut => matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Ident(n)) if n.as_str() == "self"
            ),
            TokenKind::Amp => match self.tokens.get(self.pos + 1).map(|t| &t.kind) {
                Some(TokenKind::Ident(n)) => n.as_str() == "self",
                Some(TokenKind::Mut) => true,
                _ => false,
            },
            _ => false,
        }
    }

    /// Consume an optional visibility qualifier (`pub`, `pub(crate)`,
    /// `pub(in path)`, …). The Phase-1 subset does not model fine-grained
    /// visibility, so the qualifier (and any restriction group) is simply
    /// dropped. This is shared by item-level and field-level visibility.
    fn skip_vis(&mut self) {
        if self.eat(&TokenKind::Pub) {
            if self.eat(&TokenKind::LParen) {
                let mut depth = 0i32;
                loop {
                    match self.peek_kind() {
                        TokenKind::Eof => break,
                        TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                            depth += 1;
                            self.bump();
                        }
                        TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                            if depth == 0 {
                                self.bump();
                                break;
                            }
                            depth -= 1;
                            self.bump();
                        }
                        _ => {
                            self.bump();
                        }
                    }
                }
            }
        }
    }

    fn parse_struct(&mut self, attributes: Vec<Attribute>) -> ParseResult<StructDef> {
        self.expect(TokenKind::Struct)?;
        let name = self.expect_ident()?;
        let generics = self.parse_generics()?;
        let generics = self.parse_where_clause(generics)?;
        // A struct may be a brace struct `S { .. }`, a tuple struct `S(..);`, or
        // a unit struct `S;`. Tuple/unit fields carry no names (represented with
        // an empty name), which the type-checker/codegen treat as the
        // positional form for the Phase-1 subset.
        let fields = if self.eat(&TokenKind::LParen) {
            let mut fs = Vec::new();
            if !self.check(&TokenKind::RParen) {
                loop {
                    let attrs = self.parse_attributes()?;
                    let enabled = crate::cfg::attrs_enabled(&attrs);
                    self.skip_vis();
                    let ty = self.parse_type()?;
                    if enabled {
                        fs.push(Field {
                            name: Spanned::new(String::new(), ty.span),
                            ty,
                        });
                    }
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RParen)?;
            fs
        } else if self.eat(&TokenKind::LBrace) {
            let mut fs = Vec::new();
            while !self.eat(&TokenKind::RBrace) {
                // A field may carry attributes; a false `#[cfg(..)]` drops it.
                let attrs = self.parse_attributes()?;
                let enabled = crate::cfg::attrs_enabled(&attrs);
                self.skip_vis();
                let fname = self.expect_ident()?;
                self.expect(TokenKind::Colon)?;
                let ty = self.parse_type()?;
                if enabled {
                    fs.push(Field { name: fname, ty });
                }
                if !self.eat(&TokenKind::Comma) {
                    self.expect(TokenKind::RBrace)?;
                    break;
                }
            }
            fs
        } else {
            // Unit struct `S;`.
            Vec::new()
        };
        self.eat(&TokenKind::Semicolon);
        Ok(StructDef {
            name,
            generics,
            fields,
            attributes,
        })
    }

    fn parse_enum(&mut self, attributes: Vec<Attribute>) -> ParseResult<EnumDef> {
        self.expect(TokenKind::Enum)?;
        let name = self.expect_ident()?;
        let generics = self.parse_generics()?;
        let generics = self.parse_where_clause(generics)?;
        self.expect(TokenKind::LBrace)?;
        let mut variants = Vec::new();
        while !self.eat(&TokenKind::RBrace) {
            // A variant may carry attributes (`#[default]`, `#[repr(..)]`,
            // …). The Phase-1 subset ignores them, so they are parsed and
            // dropped here to keep variant-heavy code parseable.
            let _ = self.parse_attributes()?;
            self.skip_vis();
            let vname = self.expect_ident()?;
            let fields = if self.eat(&TokenKind::LParen) {
                let mut fs = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    loop {
                        self.skip_vis();
                        // An unnamed tuple-field (`Variant(T)`) parses a bare type
                        // where a `name: Type` field would go. A named field
                        // (`Variant { x: T }`) keeps its field name. A field is
                        // "named" only when an identifier is immediately followed
                        // by `:`.
                        let (fname, ty) = if matches!(self.peek_kind(), TokenKind::Ident(_))
                            && self.next_is_colon()
                        {
                            let fname = self.expect_ident()?;
                            self.expect(TokenKind::Colon)?;
                            (fname, self.parse_type()?)
                        } else {
                            let ty = self.parse_type()?;
                            (Spanned::new(String::new(), ty.span), ty)
                        };
                        fs.push(Field { name: fname, ty });
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RParen)?;
                Some(fs)
            } else if self.eat(&TokenKind::LBrace) {
                // Struct variant `Variant { field: Type, … }`.
                let mut fs = Vec::new();
                if !self.check(&TokenKind::RBrace) {
                    loop {
                        let attrs = self.parse_attributes()?;
                        let enabled = crate::cfg::attrs_enabled(&attrs);
                        self.skip_vis();
                        let fname = self.expect_ident()?;
                        self.expect(TokenKind::Colon)?;
                        let ty = self.parse_type()?;
                        if enabled {
                            fs.push(Field { name: fname, ty });
                        }
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                        if self.check(&TokenKind::RBrace) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RBrace)?;
                Some(fs)
            } else {
                None
            };
            // An explicit discriminant (`Error = 1`) is parsed and not modeled.
            if self.eat(&TokenKind::Eq) {
                self.parse_expr()?;
            }
            variants.push(Variant {
                name: vname,
                fields,
            });
            if !self.eat(&TokenKind::Comma) {
                self.expect(TokenKind::RBrace)?;
                break;
            }
        }
        Ok(EnumDef {
            name,
            generics,
            variants,
            attributes,
        })
    }

    fn parse_const(&mut self) -> ParseResult<ConstDef> {
        self.expect(TokenKind::Const)?;
        let name = self.expect_ident()?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        self.eat(&TokenKind::Semicolon);
        Ok(ConstDef { name, ty, value })
    }

    fn parse_static(&mut self) -> ParseResult<StaticDef> {
        self.expect(TokenKind::Static)?;
        // `static mut` is accepted; mutability is not modeled.
        self.eat(&TokenKind::Mut);
        let name = self.expect_ident()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        self.eat(&TokenKind::Semicolon);
        Ok(StaticDef { name, ty, value })
    }

    fn parse_use(&mut self) -> ParseResult<UseTree> {
        self.expect(TokenKind::Use)?;
        let tree = self.parse_use_tree()?;
        self.eat(&TokenKind::Semicolon);
        Ok(tree)
    }

    fn parse_impl(&mut self, is_unsafe: bool) -> ParseResult<ImplDef> {
        // Consume the `impl` identifier.
        self.bump();
        let generics = self.parse_generics()?;
        // The first type is either the trait (for a trait impl) or the self
        // type (for an inherent impl `impl T { ... }`, which has no `for`).
        let first_ty = self.parse_type()?;
        let (trait_name, for_ty) = if self.eat(&TokenKind::For) {
            let name = Self::type_name(&first_ty);
            let self_ty = self.parse_type()?;
            (name, self_ty)
        } else {
            // Inherent impl: no trait; the parsed type *is* the self type.
            (Spanned::new(String::new(), first_ty.span), first_ty)
        };
        let generics = self.parse_where_clause(generics)?;
        self.expect(TokenKind::LBrace)?;
        let mut items = Vec::new();
        while !self.eat(&TokenKind::RBrace) {
            if let Some(item) = self.parse_item_opt()? {
                items.push(item);
            }
        }
        Ok(ImplDef {
            trait_name,
            for_ty,
            generics,
            is_unsafe,
            items,
        })
    }

    /// Extract the last-segment name of a type — used to name the trait an
    /// `impl` block is for.
    fn type_name(ty: &Type) -> Spanned<String> {
        if let TypeKind::Path(p, _) = &ty.kind {
            p.segments
                .last()
                .cloned()
                .unwrap_or_else(|| Spanned::new(String::new(), ty.span))
        } else {
            Spanned::new(String::new(), ty.span)
        }
    }

    fn parse_module(&mut self) -> ParseResult<ModuleDef> {
        // Consume the `mod` identifier.
        self.bump();
        let name = self.expect_ident()?;
        let kind = if self.eat(&TokenKind::Semicolon) {
            ModuleKind::External
        } else {
            self.expect(TokenKind::LBrace)?;
            let mut items = Vec::new();
            while !self.eat(&TokenKind::RBrace) {
                if let Some(item) = self.parse_item_opt()? {
                    items.push(item);
                }
            }
            ModuleKind::Inline(items)
        };
        Ok(ModuleDef { name, kind })
    }

    /// Parse an optional generic parameter list `<T: Bound, U>` (empty if the
    /// next token is not `<`). The list is the Phase-3 "trait bounds" surface
    /// for item-level generic declarations.
    fn parse_generics(&mut self) -> ParseResult<Generics> {
        if !self.eat_lt() {
            return Ok(Generics::default());
        }
        let mut params = Vec::new();
        if !self.check(&TokenKind::Gt) {
            loop {
                // Lifetimes are dropped at the lexer level, so a generic list
                // such as `<'a, T>` or `<'a>` leaves a dangling `,` (or nothing)
                // behind. Skip a leading `,` and stop at the closing `>` so the
                // remaining real parameters still parse.
                if matches!(self.peek_kind(), TokenKind::Comma) {
                    self.bump();
                    if matches!(self.peek_kind(), TokenKind::Gt | TokenKind::GtGt) {
                        break;
                    }
                    continue;
                }
                if matches!(self.peek_kind(), TokenKind::Gt | TokenKind::GtGt) {
                    break;
                }
                // A const generic `const N: usize` is a parameter whose type is dropped.
                if self.eat(&TokenKind::Const) {
                    let cname = self.expect_ident()?;
                    self.expect(TokenKind::Colon)?;
                    self.parse_type()?;
                    if self.eat(&TokenKind::Eq) {
                        if self.check(&TokenKind::LBrace) {
                            self.parse_block()?;
                        } else {
                            self.bump();
                        }
                    }
                    params.push(TypeParam {
                        name: cname,
                        bounds: Vec::new(),
                    });
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                    continue;
                }
                let name = self.expect_ident()?;
                let mut bounds = Vec::new();
                if self.eat(&TokenKind::Colon) {
                    // A dropped lifetime bound leaves a leading `+` (`T: 'a + Trait`).
                    self.eat(&TokenKind::Plus);
                    loop {
                        // A dropped lifetime bound (`T: Clone + 'static`) leaves a
                        // trailing `+` before `>`, so stop when the bound list ends.
                        if matches!(self.peek_kind(), TokenKind::Gt | TokenKind::GtGt) {
                            break;
                        }
                        if let Some(b) = self.parse_bound()? {
                            bounds.push(b);
                        }
                        if !self.eat(&TokenKind::Plus) {
                            break;
                        }
                    }
                }
                // A default (`E = Error`) is parsed and not modeled.
                if self.eat(&TokenKind::Eq) {
                    self.parse_type()?;
                }
                params.push(TypeParam { name, bounds });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_gt()?;
        Ok(Generics {
            params,
            where_clause: Vec::new(),
        })
    }

    /// If the next token is the `where` keyword, parse a where-clause and merge
    /// it into `generics`; otherwise return `generics` unchanged.
    fn parse_where_clause(&mut self, mut generics: Generics) -> ParseResult<Generics> {
        if let TokenKind::Ident(name) = self.peek_kind() {
            if name != "where" {
                return Ok(generics);
            }
        } else {
            return Ok(generics);
        }
        self.bump(); // consume `where`
        loop {
            // The where-clause ends where the function/impl body (or another
            // token) begins; stop at `{` rather than trying to parse it as a type.
            if matches!(self.peek_kind(), TokenKind::LBrace) {
                break;
            }
            self.skip_hrtb();
            let ty = self.parse_type()?;
            self.expect(TokenKind::Colon)?;
            let mut bounds = Vec::new();
            // A dropped leading lifetime (`T: 'a + Eq`) leaves a `+` first.
            self.eat(&TokenKind::Plus);
            loop {
                // A dropped lifetime bound leaves a trailing `+` before the
                // enclosing delimiter; stop the bound list there.
                if self.at_bound_list_end() {
                    break;
                }
                if let Some(b) = self.parse_bound()? {
                    bounds.push(b);
                }
                if !self.eat(&TokenKind::Plus) {
                    break;
                }
            }
            generics.where_clause.push(WherePredicate { ty, bounds });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(generics)
    }

    /// Skip a `>` closing a generic argument list, transparently splitting the
    /// `>>` shift-right token into two `>` so nested generics like
    /// `Vec<Box<T>>` parse correctly.
    fn expect_gt(&mut self) -> ParseResult<()> {
        match self.peek_kind() {
            TokenKind::Gt => {
                self.bump();
                Ok(())
            }
            TokenKind::GtGt => {
                let tok = self.peek().clone();
                // Split `>>` into `>` `>`: rewrite the current token and insert
                // a second `>` after it.
                self.tokens.insert(
                    self.pos + 1,
                    Token {
                        kind: TokenKind::Gt,
                        span: tok.span,
                    },
                );
                self.tokens[self.pos] = Token {
                    kind: TokenKind::Gt,
                    span: tok.span,
                };
                self.bump();
                Ok(())
            }
            other => Err(Diag::error(
                format!("expected `>`, found {}", other),
                Some(self.peek().span),
            )),
        }
    }

    /// Parse a `<...>` generic argument list of types (after the caller has
    /// consumed the leading `<`). Used for type-level generics like `Vec<T>`.
    fn parse_generic_args(&mut self) -> ParseResult<Vec<Type>> {
        let mut args = Vec::new();
        loop {
            // Lifetime arguments (`'a`, `'_`) are dropped by the lexer, which leaves
            // a stray `,` (or nothing) behind; skip it and stop at the closing `>`.
            if self.eat(&TokenKind::Comma) {
                if matches!(self.peek_kind(), TokenKind::Gt | TokenKind::GtGt) {
                    break;
                }
                continue;
            }
            if matches!(self.peek_kind(), TokenKind::Gt | TokenKind::GtGt) {
                break;
            }
            // Associated-type binding `Item = T`: the name is dropped, the type kept.
            if matches!(self.peek_kind(), TokenKind::Ident(_))
                && self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&TokenKind::Eq)
            {
                self.bump();
                self.bump();
            }
            // Literal const-generic argument (`::<true>`, `::<3>`): a value, not
            // modeled.
            if matches!(
                self.peek_kind(),
                TokenKind::Int { .. }
                    | TokenKind::True
                    | TokenKind::False
                    | TokenKind::Str(_)
                    | TokenKind::Char(_)
            ) {
                self.bump();
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                continue;
            }
            // Const-generic argument `{ expr }`: a value, not a type; not modeled.
            if self.check(&TokenKind::LBrace) {
                self.parse_block()?;
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                continue;
            }
            args.push(self.parse_type()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_gt()?;
        Ok(args)
    }

    fn parse_use_tree(&mut self) -> ParseResult<UseTree> {
        // A leading brace group (`use { a::b, c };`) has an empty prefix.
        if self.eat(&TokenKind::LBrace) {
            let nested = self.parse_delimited(TokenKind::RBrace, |s| s.parse_use_tree())?;
            return Ok(UseTree {
                prefix: Vec::new(),
                kind: UseKind::Nested(nested),
                alias: None,
            });
        }
        let mut prefix = Vec::new();
        prefix.push(self.expect_ident()?);
        while self.eat(&TokenKind::ColonColon) {
            if self.eat(&TokenKind::Star) {
                return Ok(UseTree {
                    prefix,
                    kind: UseKind::Glob,
                    alias: None,
                });
            }
            // nested braces
            if self.eat(&TokenKind::LBrace) {
                let nested = self.parse_delimited(TokenKind::RBrace, |s| s.parse_use_tree())?;
                return Ok(UseTree {
                    prefix,
                    kind: UseKind::Nested(nested),
                    alias: None,
                });
            }
            prefix.push(self.expect_ident()?);
        }
        // `use path as alias` — an optional trailing rename, e.g.
        // `use foo::Bar as Baz;`. Aliases are recorded but otherwise ignored by
        // the Phase-1 subset (which drops `use` declarations).
        if self.eat(&TokenKind::As) {
            let alias = self.expect_ident()?;
            return Ok(UseTree {
                prefix,
                kind: UseKind::Leaf,
                alias: Some(alias),
            });
        }
        Ok(UseTree {
            prefix,
            kind: UseKind::Leaf,
            alias: None,
        })
    }

    // ---- statements & blocks ----

    fn parse_block(&mut self) -> ParseResult<Block> {
        self.expect(TokenKind::LBrace)?;
        let mut stmts = Vec::new();
        let mut tail = None;
        while !self.check(&TokenKind::RBrace) && !self.at_eof() {
            let s = self.parse_stmt()?;
            // A trailing, semicolon-less expression statement becomes the block's
            // value (tail expression), matching Rust semantics.
            if let StmtKind::Expr(e) = &s.kind {
                if self.check(&TokenKind::RBrace) || self.at_eof() {
                    tail = Some(Box::new(e.clone()));
                    break;
                }
            }
            stmts.push(s);
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Block { stmts, tail })
    }

    /// Parse a sequence of leading attributes `#[path(args)]` (and inner
    /// `#![path(args)]`). Each attribute records its name `path` and the raw
    /// token stream inside the brackets (`args`). Returns the collected
    /// attributes (possibly empty).
    fn parse_attributes(&mut self) -> ParseResult<Vec<Attribute>> {
        let mut attrs = Vec::new();
        while self.peek_kind() == &TokenKind::Hash {
            self.bump(); // consume `#`
                         // Inner attribute `#![...]`: consume the `!` if present. It is stored
                         // identically to an outer attribute for now.
            if self.peek_kind() == &TokenKind::Bang {
                self.bump();
            }
            self.expect(TokenKind::LBracket)?;
            // Parse the attribute path: `ident` (`::` `ident`)*.
            let mut segments = Vec::new();
            loop {
                if let TokenKind::Ident(name) = self.peek_kind().clone() {
                    let span = self.peek().span;
                    segments.push(Spanned::new(name, span));
                } else {
                    return Err(Diag::error(
                        "expected attribute name",
                        Some(self.peek().span),
                    ));
                }
                self.bump();
                if self.eat(&TokenKind::ColonColon) {
                    continue;
                }
                break;
            }
            let path = Path { segments };
            // Capture the raw token stream until the matching `]`.
            let mut args = Vec::new();
            let mut depth = 0i32;
            loop {
                match self.peek_kind() {
                    TokenKind::Eof => {
                        return Err(Diag::error(
                            "unterminated attribute",
                            Some(self.peek().span),
                        ))
                    }
                    TokenKind::LBracket | TokenKind::LParen | TokenKind::LBrace => {
                        depth += 1;
                        args.push(self.bump());
                    }
                    TokenKind::RBracket | TokenKind::RParen | TokenKind::RBrace => {
                        if depth == 0 {
                            if matches!(self.peek_kind(), TokenKind::RBracket) {
                                self.bump();
                                break;
                            }
                            return Err(Diag::error(
                                "unexpected delimiter in attribute",
                                Some(self.peek().span),
                            ));
                        }
                        depth -= 1;
                        args.push(self.bump());
                    }
                    _ => args.push(self.bump()),
                }
            }
            attrs.push(Attribute { path, args });
        }
        Ok(attrs)
    }

    fn starts_expr(&self) -> bool {
        use TokenKind::*;
        matches!(
            self.peek_kind(),
            Ident(_)
                | Int { .. }
                | Float { .. }
                | Str(_)
                | Char(_)
                | True
                | False
                | LParen
                | LBrace
                | LBracket
                | If
                | While
                | Loop
                | For
                | Match
                | Return
                | Break
                | Continue
                | Amp
                | Star
                | Bang
                | Minus
                | Underscore
        )
    }

    /// A block-like expression at statement level (`for`, `while`, `loop`, `if`, `match`,
    /// or a `{..}` block). It ends at its closing brace, so no postfix operator (a call or
    /// an index) follows it: a `(` after it begins the next expression.
    fn parse_block_like(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        let e = match self.peek().kind.clone() {
            TokenKind::While => self.parse_while()?,
            TokenKind::Loop => self.parse_loop()?,
            TokenKind::For => self.parse_for()?,
            TokenKind::If => self.parse_if()?,
            TokenKind::Match => self.parse_match()?,
            _ => {
                let block = self.parse_block()?;
                let end = self.peek().span.start;
                Expr {
                    kind: ExprKind::Block(block),
                    span: Span::new(start, end),
                }
            }
        };
        // A method call or `?` on the block (`match x {..}.len()`) still applies.
        if self.check(&TokenKind::Dot) || self.check(&TokenKind::Question) {
            return self.parse_postfix(e);
        }
        Ok(e)
    }

    fn parse_stmt(&mut self) -> ParseResult<Stmt> {
        if self.peek_kind() == &TokenKind::Hash {
            let save = self.pos;
            let attrs = self.parse_attributes()?;
            if !self.starts_item_here() {
                let enabled = crate::cfg::attrs_enabled(&attrs);
                let stmt = self.parse_stmt()?;
                if enabled {
                    return Ok(stmt);
                }
                // A false `#[cfg(..)]` removes the statement entirely.
                let at = self.peek().span.start;
                return Ok(Stmt {
                    kind: StmtKind::Semi(Expr {
                        kind: ExprKind::Tuple(Vec::new()),
                        span: Span::new(at, at),
                    }),
                });
            }
            // An item keeps its attributes: rewind and parse the item with them, so
            // a false `#[cfg(..)]` on a nested item drops just that item.
            self.pos = save;
            return Ok(match self.parse_item_opt()? {
                Some(item) => self.stmt_for_item(item),
                None => Stmt {
                    kind: StmtKind::Semi(Expr {
                        kind: ExprKind::Tuple(Vec::new()),
                        span: Span::new(self.peek().span.start, self.peek().span.start),
                    }),
                },
            });
        }
        // A label `'label:` is lexed as a dropped lifetime followed by a
        // standalone `:`; skip that `:` so a labeled loop/block parses.
        if self.peek_kind() == &TokenKind::Colon
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Loop)
                    | Some(TokenKind::While)
                    | Some(TokenKind::For)
                    | Some(TokenKind::LBrace)
            )
        {
            self.bump();
        }
        match self.peek_kind() {
            TokenKind::Let => {
                let local = self.parse_local(true)?;
                Ok(Stmt {
                    kind: StmtKind::Local(local),
                })
            }
            TokenKind::Fn
            | TokenKind::Struct
            | TokenKind::Enum
            | TokenKind::Const
            | TokenKind::Static
            | TokenKind::Use
            | TokenKind::Impl => {
                let item = self.parse_item()?;
                Ok(self.stmt_for_item(item))
            }
            TokenKind::Return | TokenKind::Break | TokenKind::Continue => {
                let e = self.parse_expr()?;
                self.eat(&TokenKind::Semicolon);
                Ok(Stmt {
                    kind: StmtKind::Semi(e),
                })
            }
            // A loop statement ends at its closing brace: the next `&`, `*`, `-` or
            // `|` begins a new statement rather than continuing the expression.
            TokenKind::While | TokenKind::Loop | TokenKind::For => {
                let e = self.parse_block_like()?;
                self.eat(&TokenKind::Semicolon);
                Ok(Stmt {
                    kind: StmtKind::Semi(e),
                })
            }
            // Likewise an `if`, `match` or block statement.
            TokenKind::If | TokenKind::Match | TokenKind::LBrace => {
                let e = self.parse_block_like()?;
                if self.eat(&TokenKind::Semicolon) {
                    Ok(Stmt {
                        kind: StmtKind::Semi(e),
                    })
                } else {
                    Ok(Stmt {
                        kind: StmtKind::Expr(e),
                    })
                }
            }
            _ => {
                let e = self.parse_expr()?;
                if self.eat(&TokenKind::Semicolon) {
                    Ok(Stmt {
                        kind: StmtKind::Semi(e),
                    })
                } else {
                    Ok(Stmt {
                        kind: StmtKind::Expr(e),
                    })
                }
            }
        }
    }

    /// Whether the next tokens begin an item (as opposed to a statement).
    fn starts_item_here(&self) -> bool {
        match self.peek_kind() {
            TokenKind::Fn
            | TokenKind::Struct
            | TokenKind::Enum
            | TokenKind::Const
            | TokenKind::Static
            | TokenKind::Use
            | TokenKind::Impl
            | TokenKind::Pub
            | TokenKind::Async
            | TokenKind::Unsafe => true,
            TokenKind::Ident(n) => matches!(n.as_str(), "mod" | "type" | "union" | "extern"),
            _ => false,
        }
    }

    fn parse_local(&mut self, allow_struct_init: bool) -> ParseResult<Local> {
        self.expect(TokenKind::Let)?;
        let mutable = self.eat(&TokenKind::Mut);
        let pat = self.parse_pat()?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let init = if self.eat(&TokenKind::Eq) {
            // In expression position (`if let`/`while let`) the RHS must not
            // swallow the following block as a struct literal, so struct literals
            // are disabled; in statement position they are allowed (`let x = Foo { .. }`).
            let e = if allow_struct_init {
                self.parse_expr()?
            } else {
                self.parse_expr_bp(0, false)?
            };
            // `let P = E else { .. }` — the else branch diverges (returns /
            // breaks / panics). The Phase-1 subset does not model the diverging
            // control flow, so the block is parsed and dropped to keep `let`-else
            // code parseable.
            if self.eat(&TokenKind::Else) {
                self.parse_block()?;
            }
            Some(e)
        } else {
            None
        };
        self.eat(&TokenKind::Semicolon);
        Ok(Local {
            pat,
            ty,
            init,
            mutable,
        })
    }

    fn parse_pat_single(&mut self) -> ParseResult<Pat> {
        // Leading range / rest pattern: `..`, `..=b`, `..b`.
        if matches!(self.peek_kind(), TokenKind::DotDot) {
            let inclusive = self.consume_range_op();
            let rhs = if self.is_range_end(self.peek_kind()) {
                None
            } else {
                Some(Box::new(self.parse_pat_single()?))
            };
            return Ok(Pat::Range(None, rhs, inclusive));
        }
        let pat = match self.peek_kind() {
            TokenKind::Mut => {
                // Mutable binding pattern `mut x` (in fn params / `for` loops).
                self.bump();
                let name = self.expect_ident()?;
                Ok(Pat::Ident(name))
            }
            TokenKind::Underscore => {
                self.bump();
                Ok(Pat::Wild)
            }
            TokenKind::Amp => {
                // Reference pattern `&pat` / `&mut pat` (binds by reference).
                self.bump();
                self.eat(&TokenKind::Mut);
                // `parse_pat_single` (no top-level OR-loop) so a closure parameter
                // like `|&u|` does not swallow the closing `|` as an OR-pattern.
                let inner = self.parse_pat_single()?;
                Ok(Pat::Ref(Box::new(inner)))
            }
            TokenKind::LParen => {
                // Tuple pattern `(a, b, c)` (and the single-element `(a,)` form,
                // which keeps a trailing comma).
                self.bump();
                let mut pats = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    loop {
                        pats.push(self.parse_pat()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                        if self.check(&TokenKind::RParen) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RParen)?;
                Ok(Pat::Tuple(pats))
            }
            TokenKind::Int { value, suffix } => {
                let lit = Lit::Int(*value, suffix.clone());
                self.bump();
                Ok(Pat::Lit(lit))
            }
            TokenKind::Float { text, suffix } => {
                let lit = Lit::Float(text.clone(), suffix.clone());
                self.bump();
                Ok(Pat::Lit(lit))
            }
            TokenKind::Str(s) => {
                let lit = Lit::Str(s.clone());
                self.bump();
                Ok(Pat::Lit(lit))
            }
            TokenKind::Char(c) => {
                let lit = Lit::Char(*c);
                self.bump();
                Ok(Pat::Lit(lit))
            }
            TokenKind::True => {
                self.bump();
                Ok(Pat::Lit(Lit::Bool(true)))
            }
            TokenKind::False => {
                self.bump();
                Ok(Pat::Lit(Lit::Bool(false)))
            }
            TokenKind::Ident(_) => {
                let name = self.expect_ident()?;
                // `ref x` / `ref mut x` binding-by-reference pattern. The `ref`
                // keyword is lexed as an identifier; model it as `&x` so the rest
                // of the pattern (and `bind_pattern`) handle it uniformly.
                if name.node == "ref" {
                    let inner = self.parse_pat_single()?;
                    return Ok(Pat::Ref(Box::new(inner)));
                }
                let mut segments = vec![name.clone()];
                if self.eat(&TokenKind::ColonColon) {
                    // A qualified variant pattern: `Foo::Bar`, `ns::Baz`.
                    segments.push(self.expect_ident()?);
                    while self.eat(&TokenKind::ColonColon) {
                        segments.push(self.expect_ident()?);
                    }
                }
                let path = Path { segments };
                // Binding pattern `name @ subpat` (e.g. `x @ 1..=9`): bind `name`
                // to the matched value while also matching `subpat`.
                if self.eat(&TokenKind::At) {
                    let sub = self.parse_pat()?;
                    return Ok(Pat::Bind(name.node.clone(), Box::new(sub)));
                }
                // Consume an optional tuple/struct payload so variant patterns
                // with fields parse, whether qualified (`Foo::Bar(x)`) or not
                // (`Some(a)`, `Ok(v)`, `Err(e)`, `Point { x, y }`).
                if self.peek_kind() == &TokenKind::LParen {
                    // Tuple-variant pattern `Some(x)` / `Ok(v)` / `Err(e)` /
                    // `Foo::Bar(a, b)`: parse the payload as a list of patterns so
                    // the bound names survive into `Pat::Variant` (previously the
                    // payload was discarded, so `x` was never bound).
                    self.bump();
                    let args = self.parse_delimited(TokenKind::RParen, |s| s.parse_pat())?;
                    Ok(Pat::Variant(path, args))
                } else if self.peek_kind() == &TokenKind::LBrace {
                    // Struct or struct-variant pattern `Foo { x, y: pat, .. }`: each
                    // field binds (shorthand) or matches its sub-pattern; `..` is the
                    // rest of the fields.
                    self.bump();
                    let mut fields: Vec<(String, Pat)> = Vec::new();
                    loop {
                        if self.eat(&TokenKind::RBrace) {
                            break;
                        }
                        if self.eat(&TokenKind::DotDot) {
                            self.eat(&TokenKind::Comma);
                            continue;
                        }
                        // `ref` / `ref mut` / `mut` before a shorthand field name: the
                        // binding mode is not modeled, only the name.
                        if matches!(self.peek_kind(), TokenKind::Ident(n) if n == "ref") {
                            self.bump();
                        }
                        self.eat(&TokenKind::Mut);
                        let fname = self.expect_ident()?;
                        let sub = if self.eat(&TokenKind::Colon) {
                            self.parse_pat()?
                        } else {
                            Pat::Ident(fname.clone())
                        };
                        fields.push((fname.node, sub));
                        if !self.eat(&TokenKind::Comma) {
                            self.expect(TokenKind::RBrace)?;
                            break;
                        }
                    }
                    let name = path
                        .segments
                        .last()
                        .map(|s| s.node.clone())
                        .unwrap_or_default();
                    Ok(Pat::Struct(name, fields))
                } else if path.segments.len() > 1 {
                    // A qualified unit variant, e.g. `Enum::Variant` (no payload): a path
                    // test, not a binding of its first segment.
                    Ok(Pat::Path(path))
                } else {
                    // A plain binding pattern, e.g. `x` (no path, no payload).
                    Ok(Pat::Ident(name))
                }
            }
            _ => Err(Diag::error(
                format!("expected pattern, found {}", self.peek_kind()),
                Some(self.peek().span),
            )),
        }?;
        // Range pattern with a left operand: `a..b`, `a..=b`, `a..`.
        if matches!(self.peek_kind(), TokenKind::DotDot) {
            let inclusive = self.consume_range_op();
            let rhs = if self.is_range_end(self.peek_kind()) {
                None
            } else {
                Some(Box::new(self.parse_pat_single()?))
            };
            return Ok(Pat::Range(Some(Box::new(pat)), rhs, inclusive));
        }
        Ok(pat)
    }

    /// Parse a pattern, including top-level OR-patterns (`a | b`). OR-patterns
    /// are *not* allowed as a closure parameter (the `|` would be read as the
    /// parameter-list delimiter), so [`Self::parse_closure_param`] uses
    /// [`Self::parse_pat_single`] instead.
    fn parse_pat(&mut self) -> ParseResult<Pat> {
        let mut pat = self.parse_pat_single()?;
        // Or-patterns: `a | b`. Valid at the top level of a `match` arm and
        // *inside* a tuple/struct variant payload (`Some(x | y)`).
        while self.eat(&TokenKind::Pipe) {
            let rhs = self.parse_pat()?;
            pat = Pat::Or(Box::new(pat), Box::new(rhs));
        }
        Ok(pat)
    }

    // ---- expressions ----

    /// Desugar `matches!(scrutinee, pattern)` into
    /// `match scrutinee { pattern => true, _ => false }`. Returns `None` for other
    /// shapes (a match guard `pattern if ..` is not desugared here).
    pub fn desugar_matches(tokens: &[Token], span: Span) -> Option<Expr> {
        let mut depth = 0i32;
        let mut comma = None;
        for (i, t) in tokens.iter().enumerate() {
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
        let c = comma?;
        let scrutinee = Parser::segment(tokens[..c].to_vec()).parse_expr().ok()?;
        let mut pat_tokens = tokens[c + 1..].to_vec();
        if matches!(pat_tokens.last().map(|t| &t.kind), Some(TokenKind::Comma)) {
            pat_tokens.pop();
        }
        let mut pp = Parser::segment(pat_tokens);
        let pat = pp.parse_pat().ok()?;
        if !pp.at_eof() {
            return None;
        }
        let lit = |b: bool| Expr {
            kind: ExprKind::Lit(Lit::Bool(b)),
            span,
        };
        Some(Expr {
            kind: ExprKind::Match(
                Box::new(scrutinee),
                vec![(pat, lit(true)), (Pat::Wild, lit(false))],
            ),
            span,
        })
    }

    pub fn parse_expr(&mut self) -> ParseResult<Expr> {
        self.parse_expr_bp(0, true)
    }

    /// Pratt parser: parse an expression with binding power >= `min_bp`.
    /// `allow_struct` controls whether a trailing `{` after a path may be
    /// interpreted as a struct literal; it is disabled inside sub-expressions
    /// (e.g. the operand of a binary operator) so that `if a > b { .. }` does
    /// not misparse `b { .. }` as a struct literal.
    fn parse_expr_bp(&mut self, min_bp: u8, allow_struct: bool) -> ParseResult<Expr> {
        let mut lhs = self.parse_unary_or_atom(allow_struct)?;

        loop {
            // `as` cast — high precedence, right-associative. It binds tighter
            // than the multiplicative operators so `1 + 2 as u32` parses as
            // `1 + (2 as u32)`, matching Rust.
            if self.check(&TokenKind::As) {
                let (l_bp, _r_bp) = (23, 22);
                if l_bp < min_bp {
                    break;
                }
                self.bump();
                let ty = self.parse_type()?;
                let span = Span::new(lhs.span.start, ty.span.end);
                lhs = Expr {
                    kind: ExprKind::Cast(Box::new(lhs), ty),
                    span,
                };
                continue;
            }
            // Range operator `..` / `..=` (with the left operand already in
            // `lhs`). `a..b` (exclusive, both sides) falls through to
            // `binop_bp` (Binary(Range)) so the `for` loop path is unchanged;
            // we only build the new `ExprKind::Range` node for `..=` (inclusive)
            // and `a..` (trailing, right-elided).
            // Range binds looser than `+`, so a trailing or inclusive range is only
            // taken when the caller accepts range precedence (`min_bp <= 4`): in
            // `comma + 1..` the `..` closes the sum, not the `1`.
            if min_bp <= 4 && self.check(&TokenKind::DotDot) {
                let next_is_eq = self
                    .tokens
                    .get(self.pos + 1)
                    .map(|t| t.kind == TokenKind::Eq)
                    .unwrap_or(false);
                if next_is_eq {
                    // Inclusive `a..=b`.
                    let _ = self.consume_range_op();
                    let rhs = if self.is_range_end(self.peek_kind()) {
                        None
                    } else {
                        Some(Box::new(self.parse_expr_bp(3, false)?))
                    };
                    let span = Span::new(
                        lhs.span.start,
                        rhs.as_ref().map(|r| r.span.end).unwrap_or(lhs.span.end),
                    );
                    lhs = Expr {
                        kind: ExprKind::Range(Some(Box::new(lhs)), rhs, true),
                        span,
                    };
                    continue;
                }
                // Exclusive trailing `a..`.
                let after = self.tokens.get(self.pos + 1).map(|t| t.kind.clone());
                if let Some(k) = after {
                    if self.is_range_end(&k) {
                        let _ = self.consume_range_op();
                        let span = Span::new(lhs.span.start, lhs.span.end);
                        lhs = Expr {
                            kind: ExprKind::Range(Some(Box::new(lhs)), None, false),
                            span,
                        };
                        continue;
                    }
                }
            }
            let Some((l_bp, r_bp, op)) = self.binop_bp() else {
                break;
            };
            if l_bp < min_bp {
                break;
            }
            self.bump(); // consume the operator token
                         // A struct literal is only allowed as an operand when the surrounding
                         // expression permits it (e.g. a top-level statement), so that `if a >
                         // b { .. }` does not misparse `b { .. }` as a struct literal instead
                         // of the `if`'s then-block. The `if`/`while`/`match`/`for` parsers
                         // disable struct literals for their condition, and that flag now
                         // propagates through to this RHS.
            let rhs = self.parse_expr_bp(r_bp, allow_struct)?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr {
                kind: ExprKind::Binary(Box::new(lhs), op, Box::new(rhs)),
                span,
            };
        }
        Ok(lhs)
    }

    fn binop_bp(&self) -> Option<(u8, u8, BinOp)> {
        use TokenKind::*;
        let op = match self.peek_kind() {
            Eq => BinOp::Assign,
            PipePipe => BinOp::Or,
            AmpAmp => BinOp::And,
            Pipe => BinOp::BitOr,
            Caret => BinOp::BitXor,
            Amp => BinOp::BitAnd,
            EqEq => BinOp::Eq,
            BangEq => BinOp::Ne,
            Lt => BinOp::Lt,
            Gt => BinOp::Gt,
            LtEq => BinOp::Le,
            GtEq => BinOp::Ge,
            LtLt => BinOp::Shl,
            GtGt => BinOp::Shr,
            DotDot => BinOp::Range,
            PlusEq => BinOp::AddAssign,
            MinusEq => BinOp::SubAssign,
            StarEq => BinOp::MulAssign,
            SlashEq => BinOp::DivAssign,
            PercentEq => BinOp::RemAssign,
            AmpEq => BinOp::BitAndAssign,
            PipeEq => BinOp::BitOrAssign,
            CaretEq => BinOp::BitXorAssign,
            LtLtEq => BinOp::ShlAssign,
            GtGtEq => BinOp::ShrAssign,
            Plus => BinOp::Add,
            Minus => BinOp::Sub,
            Star => BinOp::Mul,
            Slash => BinOp::Div,
            Percent => BinOp::Rem,
            _ => return None,
        };
        // left binding power, right binding power.
        // Left-associative operators use l = r + 1; right-associative use l = r - 1.
        let (l, r) = match op {
            BinOp::Assign
            | BinOp::AddAssign
            | BinOp::SubAssign
            | BinOp::MulAssign
            | BinOp::DivAssign
            | BinOp::RemAssign
            | BinOp::BitAndAssign
            | BinOp::BitOrAssign
            | BinOp::BitXorAssign
            | BinOp::ShlAssign
            | BinOp::ShrAssign => (1, 2), // right-associative, lowest
            BinOp::Range => (4, 3),
            BinOp::Or => (5, 4),
            BinOp::And => (7, 6),
            BinOp::BitOr => (9, 8),
            BinOp::BitXor => (11, 10),
            BinOp::BitAnd => (13, 12),
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => (15, 14),
            BinOp::Shl | BinOp::Shr => (17, 16),
            BinOp::Add | BinOp::Sub => (19, 18),
            BinOp::Mul | BinOp::Div | BinOp::Rem => (21, 20),
        };
        Some((l, r, op))
    }

    /// Whether `k` terminates a range expression (i.e. there is no right
    /// operand): a closing/separator delimiter, the `=>`/`|` that end a match
    /// arm, or end-of-input.
    fn is_range_end(&self, k: &TokenKind) -> bool {
        matches!(
            k,
            TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
                | TokenKind::LBrace
                | TokenKind::Comma
                | TokenKind::Semicolon
                | TokenKind::Eof
                | TokenKind::FatArrow
                | TokenKind::Pipe
                | TokenKind::PipePipe
        )
    }

    /// Consume a `..` or `..=` range operator, returning `true` for the inclusive
    /// `..=` form. Assumes the cursor is positioned at `..`.
    fn consume_range_op(&mut self) -> bool {
        self.eat(&TokenKind::DotDot);
        self.eat(&TokenKind::Eq)
    }

    fn parse_unary_or_atom(&mut self, allow_struct: bool) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        // A leading `..` / `..=` (open or right-open range) begins a range
        // expression with no left operand.
        if matches!(self.peek_kind(), TokenKind::DotDot) {
            let inclusive = self.consume_range_op();
            let rhs = if self.is_range_end(self.peek_kind()) {
                None
            } else {
                Some(Box::new(self.parse_expr_bp(3, false)?))
            };
            let end = rhs.as_ref().map(|r| r.span.end).unwrap_or(start);
            return Ok(Expr {
                kind: ExprKind::Range(None, rhs, inclusive),
                span: Span::new(start, end),
            });
        }
        let kind = match self.peek_kind() {
            TokenKind::Minus => {
                self.bump();
                let e = self.parse_unary_or_atom(allow_struct)?;
                ExprKind::Unary(UnOp::Neg, Box::new(e))
            }
            TokenKind::Bang => {
                self.bump();
                let e = self.parse_unary_or_atom(allow_struct)?;
                ExprKind::Unary(UnOp::Not, Box::new(e))
            }
            TokenKind::Star => {
                self.bump();
                let e = self.parse_unary_or_atom(allow_struct)?;
                ExprKind::Unary(UnOp::Deref, Box::new(e))
            }
            TokenKind::Await => {
                self.bump();
                let e = self.parse_unary_or_atom(allow_struct)?;
                ExprKind::Await(Box::new(e))
            }
            TokenKind::Amp => {
                self.bump();
                // Raw borrow `&raw const place` / `&raw mut place`: a plain borrow here.
                let raw = matches!(self.peek_kind(), TokenKind::Ident(n) if n == "raw")
                    && matches!(
                        self.tokens.get(self.pos + 1).map(|t| &t.kind),
                        Some(TokenKind::Const) | Some(TokenKind::Mut)
                    );
                if raw {
                    self.bump();
                }
                let mutable = self.eat(&TokenKind::Mut);
                if raw && !mutable {
                    self.eat(&TokenKind::Const);
                }
                let e = self.parse_unary_or_atom(allow_struct)?;
                ExprKind::Ref(Box::new(e), mutable)
            }
            // A `|`/`||` at the start of an expression always begins a closure
            // (bitwise-OR/`||` never start an expression).
            TokenKind::Pipe | TokenKind::PipePipe => {
                return self.parse_closure();
            }
            // `let` in expression position (`if let`, `while let`, let-else).
            TokenKind::Let => {
                return self.parse_let_expr();
            }
            // `move` closures: `move |x| ...` — drop the `move` and parse the
            // closure that follows.
            TokenKind::Ident(name) if name == "move" => {
                self.bump();
                return self.parse_unary_or_atom(allow_struct);
            }
            _ => return self.parse_atom(allow_struct),
        };
        Ok(Expr {
            kind,
            span: Span::new(start, self.peek().span.start),
        })
    }

    /// Parse a comma-separated list terminated by `close`, each element via
    /// `parse_one`. A trailing comma immediately before `close` is tolerated
    /// (matching Rust's grammar, e.g. `[1, 2,]` and `f(a, b,)`), so callers do
    /// not need bespoke trailing-comma handling.
    fn parse_delimited<T>(
        &mut self,
        close: TokenKind,
        mut parse_one: impl FnMut(&mut Self) -> ParseResult<T>,
    ) -> ParseResult<Vec<T>> {
        let mut items = Vec::new();
        if !self.check(&close) {
            loop {
                items.push(parse_one(self)?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                if self.check(&close) {
                    break;
                }
            }
        }
        self.expect(close)?;
        Ok(items)
    }

    fn parse_atom(&mut self, allow_struct: bool) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        // Skip a label marker (see `parse_stmt`): `'label:` lexes to a dropped
        // lifetime plus a standalone `:`, and a labeled block (`'a: { .. }`) is
        // parsed here as an expression.
        if self.peek_kind() == &TokenKind::Colon
            && self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&TokenKind::LBrace)
        {
            self.bump();
        }
        let expr = match self.peek_kind().clone() {
            TokenKind::Unsafe => {
                let e = self.parse_unsafe_block()?;
                return self.parse_postfix(e);
            }
            // A leading `::` (global path `::core::ptr::write(..)`) is the crate root.
            TokenKind::ColonColon => {
                self.bump();
                return self.parse_atom(allow_struct);
            }
            // Qualified path `<T as Trait>::name` (or `<T>::name`): the trait is
            // dropped and the path is resolved through the self type's name.
            TokenKind::Lt => {
                self.bump();
                let self_ty = self.parse_type()?;
                if self.eat(&TokenKind::As) {
                    self.parse_path(true)?;
                }
                self.expect(TokenKind::Gt)?;
                self.expect(TokenKind::ColonColon)?;
                let mut segments = match &self_ty.kind {
                    TypeKind::Path(p, _) => p.segments.clone(),
                    // A non-path self type (`<[u8]>::f`) has no name to resolve by.
                    _ => vec![Spanned::new("<qualified>".to_string(), self_ty.span)],
                };
                segments.push(self.expect_name()?);
                while self.eat(&TokenKind::ColonColon) {
                    segments.push(self.expect_name()?);
                }
                let end = self.peek().span.start;
                return self.parse_postfix(Expr {
                    kind: ExprKind::Path(Path { segments }),
                    span: Span::new(start, end),
                });
            }
            TokenKind::Int { value, suffix } => {
                self.bump();
                ExprKind::Lit(Lit::Int(value, suffix))
            }
            TokenKind::Float { text, suffix } => {
                self.bump();
                ExprKind::Lit(Lit::Float(text, suffix))
            }
            TokenKind::Str(s) => {
                self.bump();
                ExprKind::Lit(Lit::Str(s))
            }
            TokenKind::Char(c) => {
                self.bump();
                ExprKind::Lit(Lit::Char(c))
            }
            TokenKind::True => {
                self.bump();
                ExprKind::Lit(Lit::Bool(true))
            }
            TokenKind::False => {
                self.bump();
                ExprKind::Lit(Lit::Bool(false))
            }
            TokenKind::Ident(_)
            | TokenKind::I8
            | TokenKind::I16
            | TokenKind::I32
            | TokenKind::I64
            | TokenKind::I128
            | TokenKind::Isize
            | TokenKind::U8
            | TokenKind::U16
            | TokenKind::U32
            | TokenKind::U64
            | TokenKind::U128
            | TokenKind::Usize
            | TokenKind::Bool
            | TokenKind::CharKw
            | TokenKind::StrKw
            | TokenKind::F32
            | TokenKind::F64 => {
                let path = self.parse_path(false)?;
                // A trailing `{` is a struct literal only when this path is a
                // primary expression; otherwise it is a following block.
                if allow_struct && self.check(&TokenKind::LBrace) {
                    self.bump();
                    let (fields, base) = self.parse_struct_literal_body()?;
                    let end = self.peek().span.start;
                    return Ok(Expr {
                        kind: ExprKind::Struct(path, fields, base),
                        span: Span::new(start, end),
                    });
                }
                ExprKind::Path(path)
            }
            TokenKind::LParen => {
                self.bump();
                if self.eat(&TokenKind::RParen) {
                    // unit literal `()`
                    ExprKind::Lit(Lit::Str(String::new()))
                } else {
                    let first = self.parse_expr()?;
                    if self.eat(&TokenKind::Comma) {
                        // Tuple literal `(a, b, …)`. `parse_delimited` already
                        // consumes the closing `)`; for the single-element
                        // trailing-comma form `(a,)` we must consume it here.
                        let mut elems = vec![first];
                        if !self.check(&TokenKind::RParen) {
                            let more =
                                self.parse_delimited(TokenKind::RParen, |s| s.parse_expr())?;
                            elems.extend(more);
                        } else {
                            self.expect(TokenKind::RParen)?;
                        }
                        let end = self.peek().span.start;
                        return self.parse_postfix(Expr {
                            kind: ExprKind::Tuple(elems),
                            span: Span::new(start, end),
                        });
                    }
                    // Parenthesized single expression.
                    self.expect(TokenKind::RParen)?;
                    let end = self.peek().span.start;
                    // A parenthesized callee, `(f)(x)` / `(s.g)(x)`: the parens are a
                    // plain expression here, so the call applies to it directly.
                    if self.check(&TokenKind::LParen) {
                        return self.parse_postfix(first);
                    }
                    return self.parse_postfix(Expr {
                        kind: ExprKind::Block(Block {
                            stmts: vec![],
                            tail: Some(Box::new(first)),
                        }),
                        span: Span::new(start, end),
                    });
                }
            }
            TokenKind::LBrace => {
                let block = self.parse_block()?;
                ExprKind::Block(block)
            }
            TokenKind::LBracket => {
                self.bump();
                if self.eat(&TokenKind::RBracket) {
                    // Empty array `[]`.
                    let end = self.peek().span.start;
                    return self.parse_postfix(Expr {
                        kind: ExprKind::Array(Vec::new()),
                        span: Span::new(start, end),
                    });
                }
                let first = self.parse_expr()?;
                if self.eat(&TokenKind::Semicolon) {
                    // Repeat form `[value; count]`.
                    let count = self.parse_expr()?;
                    self.expect(TokenKind::RBracket)?;
                    ExprKind::ArrayRepeat(Box::new(first), Box::new(count))
                } else {
                    let mut elems = vec![first];
                    if self.eat(&TokenKind::Comma) {
                        let more = self.parse_delimited(TokenKind::RBracket, |s| s.parse_expr())?;
                        elems.extend(more);
                    } else {
                        self.expect(TokenKind::RBracket)?;
                    }
                    ExprKind::Array(elems)
                }
            }
            // These expression forms may be postfixed (e.g. `match { … }.method()`
            // or `if cond { } .field`), so they are wrapped in `parse_postfix`
            // before returning (returning them bare would bypass field/method
            // access on the whole conditional/match expression). The inner result
            // is bound to a local first to satisfy the borrow checker.
            TokenKind::If => {
                let e = self.parse_if()?;
                return self.parse_postfix(e);
            }
            TokenKind::While => {
                let e = self.parse_while()?;
                return self.parse_postfix(e);
            }
            TokenKind::Loop => {
                let e = self.parse_loop()?;
                return self.parse_postfix(e);
            }
            TokenKind::For => {
                let e = self.parse_for()?;
                return self.parse_postfix(e);
            }
            TokenKind::Match => {
                let e = self.parse_match()?;
                return self.parse_postfix(e);
            }
            TokenKind::Return => {
                self.bump();
                if self.starts_expr() && !self.check(&TokenKind::Semicolon) {
                    let e = self.parse_expr()?;
                    ExprKind::Return(Some(Box::new(e)))
                } else {
                    ExprKind::Return(None)
                }
            }
            TokenKind::Break => {
                self.bump();
                ExprKind::Break
            }
            TokenKind::Continue => {
                self.bump();
                ExprKind::Continue
            }
            other => {
                return Err(Diag::error(
                    format!("expected expression, found {}", other),
                    Some(self.peek().span),
                ))
            }
        };
        let end = self.peek().span.start;
        let base = Expr {
            kind: expr,
            span: Span::new(start, end),
        };
        self.parse_postfix(base)
    }

    fn parse_postfix(&mut self, mut expr: Expr) -> ParseResult<Expr> {
        loop {
            let start = expr.span.start;
            match self.peek_kind() {
                TokenKind::LParen => {
                    // A block expression is not callable in Rust, so a `(` that
                    // immediately follows a block (e.g. a `match` arm body
                    // `{ .. }` followed by the next arm's `(pat) =>` pattern, or a
                    // trailing block statement) must NOT be parsed as a call. Leave
                    // the `(` for the caller so match arms / statements terminate
                    // correctly; without this, `body { .. } (args)` is misread as a
                    // function call whose argument list swallows the following arm.
                    if matches!(expr.kind, ExprKind::Block(_)) {
                        break;
                    }
                    self.bump();
                    let args = self.parse_delimited(TokenKind::RParen, |s| s.parse_expr())?;
                    let end = self.peek().span.start;
                    expr = Expr {
                        kind: ExprKind::Call(Box::new(expr), args),
                        span: Span::new(start, end),
                    };
                }
                TokenKind::Dot => {
                    self.bump();
                    if self.peek_kind() == &TokenKind::LParen {
                        // tuple index: .0 — not in subset; treat as error-free path-less call skip
                        // For now only support field access by identifier.
                        return Err(Diag::error(
                            "tuple indexing `.N` is not in the Phase-1 subset",
                            Some(self.peek().span),
                        ));
                    }
                    if let TokenKind::Int { value, .. } = self.peek_kind().clone() {
                        // Tuple field access `expr.0` — represented as a field with
                        // a numeric name; resolution to the tuple element happens
                        // during type-check/lowering. This keeps tuple-heavy code
                        // (e.g. `a.0.cmp(b.0)`) parseable.
                        let span = self.peek().span;
                        self.bump();
                        let end = self.peek().span.start;
                        expr = Expr {
                            kind: ExprKind::Field(
                                Box::new(expr),
                                Spanned::new(value.to_string(), span),
                            ),
                            span: Span::new(start, end),
                        };
                    } else if self.check(&TokenKind::Ident(String::new()))
                        || matches!(self.peek_kind(), TokenKind::Ident(_))
                        || is_primitive_keyword(self.peek_kind())
                    {
                        let method = self.expect_ident()?;
                        // Optional turbofish `::<T>` (e.g. `.collect::<Vec<_>>()`).
                        // The Phase-1 subset ignores the inferred type arguments,
                        // but the parser must accept them so method chains in real
                        // Rust code parse.
                        if self.eat(&TokenKind::ColonColon) && self.eat_lt() {
                            self.parse_generic_args()?;
                        }
                        if self.eat(&TokenKind::LParen) {
                            let args =
                                self.parse_delimited(TokenKind::RParen, |s| s.parse_expr())?;
                            let end = self.peek().span.start;
                            expr = Expr {
                                kind: ExprKind::MethodCall(Box::new(expr), method, args),
                                span: Span::new(start, end),
                            };
                        } else {
                            let end = self.peek().span.start;
                            expr = Expr {
                                kind: ExprKind::Field(Box::new(expr), method),
                                span: Span::new(start, end),
                            };
                        }
                    } else if self.eat(&TokenKind::Await) {
                        // Postfix `.await` — suspend until the future is ready.
                        let end = self.peek().span.start;
                        expr = Expr {
                            kind: ExprKind::Await(Box::new(expr)),
                            span: Span::new(start, end),
                        };
                    } else {
                        return Err(Diag::error(
                            "expected field, method name, or `await` after `.`",
                            Some(self.peek().span),
                        ));
                    }
                }
                TokenKind::LBracket => {
                    self.bump();
                    let idx = self.parse_expr()?;
                    self.expect(TokenKind::RBracket)?;
                    let end = self.peek().span.start;
                    expr = Expr {
                        kind: ExprKind::Index(Box::new(expr), Box::new(idx)),
                        span: Span::new(start, end),
                    };
                }
                // Macro call: `path!(...)` / `path![...]` / `path!{...}`. The
                // inner tokens are retained verbatim for expansion/stubbing.
                // A macro call needs a path before `!`. After a block-like statement
                // (`for .. { } !p`), `!` starts the next statement, so it is not
                // consumed here.
                TokenKind::Bang if matches!(expr.kind, ExprKind::Path(_)) => {
                    self.bump(); // `!`
                    let path = match &expr.kind {
                        ExprKind::Path(p) => p.clone(),
                        _ => {
                            return Err(Diag::error(
                                "macro call must follow a path",
                                Some(expr.span),
                            ))
                        }
                    };
                    let (open, close, delimiter) = if self.eat(&TokenKind::LParen) {
                        (TokenKind::LParen, TokenKind::RParen, MacroDelim::Paren)
                    } else if self.eat(&TokenKind::LBracket) {
                        (
                            TokenKind::LBracket,
                            TokenKind::RBracket,
                            MacroDelim::Bracket,
                        )
                    } else if self.eat(&TokenKind::LBrace) {
                        (TokenKind::LBrace, TokenKind::RBrace, MacroDelim::Brace)
                    } else {
                        return Err(Diag::error(
                            "expected `(`, `[`, or `{` after macro name",
                            Some(self.peek().span),
                        ));
                    };
                    // Track depth only on the macro's *own* delimiter pair. Other
                    // delimiters (`(`, `{`) inside the token stream are captured
                    // verbatim as macro content; mixing the two made a nested
                    // `(` (e.g. `vec![f(g())]`) desynchronize depth and swallow
                    // the rest of the file.
                    let mut depth = 1i32;
                    let mut tokens = Vec::new();
                    loop {
                        match self.peek_kind() {
                            TokenKind::Eof => break,
                            k if *k == close && depth == 1 => {
                                self.bump();
                                break;
                            }
                            k if *k == close => {
                                // A nested closing delimiter is macro *content*
                                // (e.g. the `)` of `v.len()` inside
                                // `println!("{}", v.len())`) — keep it in the
                                // token stream so argument re-parsing sees the
                                // method call's argument list.
                                let t = self.bump();
                                tokens.push(t);
                                depth -= 1;
                            }
                            k if *k == open => {
                                // Likewise a nested opening delimiter is
                                // content; only the macro's own pair (depth 1)
                                // is treated as structure.
                                let t = self.bump();
                                tokens.push(t);
                                depth += 1;
                            }
                            _ => {
                                tokens.push(self.bump());
                            }
                        }
                    }
                    let end = self.peek().span.start;
                    // `matches!(e, pat)` is an ordinary `match` (its second argument is a
                    // pattern, not an expression); other macros stay macro calls.
                    let is_matches = path.segments.last().is_some_and(|s| s.node == "matches");
                    let desugared = if is_matches {
                        Parser::desugar_matches(&tokens, Span::new(start, end))
                    } else {
                        None
                    };
                    expr = match desugared {
                        Some(e) => e,
                        None => Expr {
                            kind: ExprKind::Macro(MacroCall {
                                path,
                                delimiter,
                                tokens,
                            }),
                            span: Span::new(start, end),
                        },
                    };
                }
                // The `?` operator: `expr?`.
                TokenKind::Question => {
                    self.bump();
                    let end = self.peek().span.start;
                    expr = Expr {
                        kind: ExprKind::Try(Box::new(expr)),
                        span: Span::new(start, end),
                    };
                }
                // A `.N` float token immediately following a receiver is numeric
                // tuple/struct field access. The lexer fuses `.0` into a single
                // `Float` literal (so standalone leading-dot floats like `.5`
                // also parse); here we recover the field-access reading, which is
                // how `expr.0` / `expr.5` appear in real code.
                TokenKind::Float { text, .. }
                    if text.starts_with('.') && text[1..].chars().all(|c| c.is_ascii_digit()) =>
                {
                    let span = self.peek().span;
                    let field = text[1..].to_string();
                    self.bump();
                    let end = self.peek().span.start;
                    expr = Expr {
                        kind: ExprKind::Field(Box::new(expr), Spanned::new(field, span)),
                        span: Span::new(start, end),
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// Parse the body of a struct literal: `field: expr, ..base }`.
    fn parse_struct_literal_body(&mut self) -> ParseResult<StructLiteral> {
        let mut fields = Vec::new();
        let mut base = None;
        while !self.eat(&TokenKind::RBrace) {
            // A field may carry attributes; a false `#[cfg(..)]` drops the field.
            let attrs = self.parse_attributes()?;
            let enabled = crate::cfg::attrs_enabled(&attrs);
            if self.eat(&TokenKind::DotDot) {
                let e = self.parse_expr()?;
                base = Some(Box::new(e));
                self.eat(&TokenKind::Comma);
                self.expect(TokenKind::RBrace)?;
                break;
            }
            let name = self.expect_ident()?;
            // Field shorthand `Foo { x }` desugars to `Foo { x: x }`; an explicit
            // `Foo { x: e }` is also accepted.
            let value = if self.eat(&TokenKind::Colon) {
                self.parse_expr()?
            } else {
                Expr {
                    kind: ExprKind::Path(Path {
                        segments: vec![name.clone()],
                    }),
                    span: name.span,
                }
            };
            if enabled {
                fields.push((name, value));
            }
            if !self.eat(&TokenKind::Comma) {
                self.expect(TokenKind::RBrace)?;
                break;
            }
        }
        Ok((fields, base))
    }

    fn parse_if(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        self.expect(TokenKind::If)?;
        // The condition must not consume the then-block's `{` as a struct
        // literal (`if c { .. }` is not `if (c { .. })`).
        let cond = self.parse_expr_bp(1, false)?;
        let then_block = self.parse_block()?;
        let else_branch = if self.eat(&TokenKind::Else) {
            if self.check(&TokenKind::If) {
                Some(Box::new(self.parse_if()?))
            } else {
                let b = self.parse_block()?;
                let end = b
                    .stmts
                    .last()
                    .map(|_| self.peek().span.end)
                    .unwrap_or(start);
                Some(Box::new(Expr {
                    kind: ExprKind::Block(b),
                    span: Span::new(start, end),
                }))
            }
        } else {
            None
        };
        let end = self.peek().span.start;
        Ok(Expr {
            kind: ExprKind::If(Box::new(cond), then_block, else_branch),
            span: Span::new(start, end),
        })
    }

    fn parse_while(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        self.expect(TokenKind::While)?;
        let cond = self.parse_expr_bp(1, false)?;
        let block = self.parse_block()?;
        let end = self.peek().span.start;
        Ok(Expr {
            kind: ExprKind::While(Box::new(cond), block),
            span: Span::new(start, end),
        })
    }

    fn parse_loop(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        self.expect(TokenKind::Loop)?;
        let block = self.parse_block()?;
        let end = self.peek().span.start;
        Ok(Expr {
            kind: ExprKind::Loop(block),
            span: Span::new(start, end),
        })
    }

    fn parse_for(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        self.expect(TokenKind::For)?;
        let pat = self.parse_pat()?;
        self.expect(TokenKind::In)?;
        let iter = self.parse_expr_bp(1, false)?;
        let block = self.parse_block()?;
        let end = self.peek().span.start;
        Ok(Expr {
            kind: ExprKind::For(pat, Box::new(iter), block),
            span: Span::new(start, end),
        })
    }

    fn parse_match(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        self.expect(TokenKind::Match)?;
        // `allow_struct = false`: a trailing `{` after the scrutinee belongs to
        // the `match` body, not a struct-literal scrutinee (which must be
        // parenthesized, e.g. `match (Point { .. }) { .. }`).
        let scrutinee = self.parse_expr_bp(1, false)?;
        self.expect(TokenKind::LBrace)?;
        let mut arms = Vec::new();
        while !self.eat(&TokenKind::RBrace) {
            // An arm may carry attributes; a false `#[cfg(..)]` drops the arm.
            let attrs = self.parse_attributes()?;
            let enabled = crate::cfg::attrs_enabled(&attrs);
            // `parse_pat` already folds `|` into `Pat::Or`, so no extra loop here.
            let pat = self.parse_pat()?;
            // Match guard `Pattern if <cond> => body`. The Phase-1 subset does not
            // model guards, so the condition is parsed and discarded; this keeps
            // real-world code (and the compiler's own source) parseable.
            if self.eat(&TokenKind::If) {
                self.parse_expr()?;
            }
            self.expect(TokenKind::FatArrow)?;
            let body = self.parse_expr()?;
            self.eat(&TokenKind::Comma);
            if enabled {
                arms.push((pat, body));
            }
        }
        let end = self.peek().span.start;
        Ok(Expr {
            kind: ExprKind::Match(Box::new(scrutinee), arms),
            span: Span::new(start, end),
        })
    }

    fn parse_unsafe_block(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        self.bump(); // `unsafe`
        let block = self.parse_block()?;
        let end = self.peek().span.start;
        Ok(Expr {
            kind: ExprKind::Unsafe(block),
            span: Span::new(start, end),
        })
    }

    /// Parse a `let` in expression position, used by `if let` / `while let`
    /// conditions. It parses only the `let pat = expr` binding (the surrounding
    /// `if`/`while` is responsible for the following block), and represents the
    /// binding as a single-statement `Block` so existing expression handling
    /// applies without a new AST node.
    fn parse_let_expr(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        let local = self.parse_local(false)?;
        let end = self.peek().span.start;
        Ok(Expr {
            kind: ExprKind::Block(Block {
                stmts: vec![Stmt {
                    kind: StmtKind::Local(local),
                }],
                tail: None,
            }),
            span: Span::new(start, end),
        })
    }

    /// Parse a closure expression `|params| body` or `|| body`, with optional
    /// `-> Ret` return type and an expression body.
    fn parse_closure(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        let no_args = self.eat(&TokenKind::PipePipe); // `||`
        if !no_args {
            self.eat(&TokenKind::Pipe); // `|`
        }
        let mut params = Vec::new();
        if !no_args {
            if !self.check(&TokenKind::Pipe) && !self.check(&TokenKind::PipePipe) {
                loop {
                    params.push(self.parse_closure_param()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                    if self.check(&TokenKind::Pipe) || self.check(&TokenKind::PipePipe) {
                        break;
                    }
                }
            }
            // closing `|`
            if !self.eat(&TokenKind::Pipe) {
                self.eat(&TokenKind::PipePipe);
            }
        }
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.parse_expr()?;
        let end = body.span.end;
        Ok(Expr {
            kind: ExprKind::Closure {
                params,
                ret,
                body: Box::new(body),
            },
            span: Span::new(start, end),
        })
    }

    /// Parse a single closure parameter, which may carry an optional `mut` and an
    /// optional `: Type` annotation (unlike fn params, the type is optional).
    fn parse_closure_param(&mut self) -> ParseResult<Param> {
        let _mutable = self.eat(&TokenKind::Mut);
        // Single pattern only: a top-level OR-pattern (`a | b`) would swallow the
        // parameter-list's closing `|` (see `parse_pat_single`).
        let pat = self.parse_pat_single()?;
        let ty = if self.eat(&TokenKind::Colon) {
            self.parse_type()?
        } else {
            Type {
                kind: TypeKind::Infer,
                span: self.peek().span,
            }
        };
        Ok(Param { pat, ty })
    }

    /// Parse a `::`-separated path. When `allow_generics` is set, a leading
    /// `<...>` after the first segment is consumed as generic arguments
    /// (e.g. `Vec<i32>`) — this is type/bound syntax. In expression context it
    /// must be `false`, otherwise a comparison like `i < n` would be misparsed as
    /// a path-with-generic-args `i<n>` and the following `<` swallowed as a type.
    /// The `::<...>` turbofish form is always allowed regardless of this flag.
    fn parse_path(&mut self, allow_generics: bool) -> ParseResult<Path> {
        let mut segments = Vec::new();
        segments.push(self.expect_name()?);
        loop {
            // Generic arguments in angle-bracket form (`Vec<i32>`, `Result<T, E>`).
            if allow_generics && self.eat_lt() {
                self.parse_generic_args()?;
            }
            if self.eat(&TokenKind::ColonColon) {
                // Turbofish `::<...>` between segments.
                if self.eat_lt() {
                    self.parse_generic_args()?;
                    // A trailing turbofish (`size_of::<T>()`) ends the path.
                    if !self.check(&TokenKind::ColonColon) {
                        break;
                    }
                    self.bump();
                }
                segments.push(self.expect_name()?);
            } else {
                break;
            }
        }
        Ok(Path { segments })
    }

    /// Accept an identifier or a primitive-type keyword as a path segment name.
    /// Whether a `+`-separated bound list ends here: a closing token, or the
    /// `where` keyword (a dropped lifetime bound can leave a `+` right before it).
    fn at_bound_list_end(&self) -> bool {
        matches!(
            self.peek_kind(),
            TokenKind::Comma
                | TokenKind::Gt
                | TokenKind::GtGt
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::LBrace
                | TokenKind::Eq
                | TokenKind::Semicolon
        ) || matches!(self.peek_kind(), TokenKind::Ident(n) if n == "where")
    }

    /// Skip a higher-ranked binder `for<'a, 'b>` (its lifetimes are erased by the
    /// lexer, so only the angle brackets remain).
    fn skip_hrtb(&mut self) {
        if matches!(self.peek_kind(), TokenKind::For)
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Lt) | Some(TokenKind::LtLt)
            )
        {
            self.bump();
            self.eat_lt();
            while !self.check(&TokenKind::Gt) && !self.at_eof() {
                self.bump();
            }
            let _ = self.expect_gt();
        }
    }

    /// Consume one `<` that opens generic arguments. The lexer emits `<<` for
    /// `Either<<T as Tr>::X, U>`; the first `<` is consumed and the second is left
    /// in place for the next parse.
    fn eat_lt(&mut self) -> bool {
        match self.peek_kind() {
            TokenKind::Lt => {
                self.bump();
                true
            }
            TokenKind::LtLt => {
                self.tokens[self.pos].kind = TokenKind::Lt;
                true
            }
            _ => false,
        }
    }

    fn expect_name(&mut self) -> ParseResult<Spanned<String>> {
        let t = self.bump();
        match &t.kind {
            TokenKind::Ident(name) => Ok(Spanned::new(name.clone(), t.span)),
            k if is_primitive_keyword(k) => Ok(Spanned::new(primitive_name(k), t.span)),
            _ => Err(Diag::error(
                format!("expected name, found {}", t.kind),
                Some(t.span),
            )),
        }
    }

    // ---- types ----

    fn parse_type(&mut self) -> ParseResult<Type> {
        let start = self.peek().span.start;
        let kind = match self.peek_kind() {
            TokenKind::Amp => {
                self.bump();
                let is_mut = self.eat(&TokenKind::Mut);
                let inner = self.parse_type()?;
                if is_mut {
                    TypeKind::RefMut(Box::new(inner))
                } else {
                    TypeKind::Ref(Box::new(inner))
                }
            }
            TokenKind::Star => {
                // Raw pointer type: `*const T` or `*mut T`.
                self.bump();
                let is_mut = if self.eat(&TokenKind::Mut) {
                    true
                } else {
                    self.eat(&TokenKind::Const);
                    false
                };
                let inner = self.parse_type()?;
                if is_mut {
                    TypeKind::RefMut(Box::new(inner))
                } else {
                    TypeKind::Ref(Box::new(inner))
                }
            }
            TokenKind::LBracket => {
                self.bump();
                let inner = self.parse_type()?;
                if self.eat(&TokenKind::Semicolon) {
                    // Array length: any integer literal, e.g. `[u8; 6]`. The old
                    // code matched only the literal `0`, so `[T; N]` for N != 0
                    // failed to parse.
                    // A literal length keeps its value; any other expression
                    // (`[T; i128::MAX]`, `[(); 0 - x]`) is parsed and recorded as
                    // unknown (0).
                    let len_expr = self.parse_expr()?;
                    let n = match &len_expr.kind {
                        ExprKind::Lit(Lit::Int(v, _)) => *v,
                        _ => 0,
                    };
                    self.expect(TokenKind::RBracket)?;
                    TypeKind::Array(Box::new(inner), n)
                } else {
                    self.expect(TokenKind::RBracket)?;
                    TypeKind::Slice(Box::new(inner))
                }
            }
            TokenKind::LParen => {
                self.bump();
                // `parse_delimited` consumes the closing `)` itself for non-empty
                // tuples `(A, B)`; for the empty-tuple (`unit`) type there are no
                // elements, so only the lone `)` needs swallowing (and must not be
                // eaten again, or it would consume the enclosing paren).
                let tys = if self.check(&TokenKind::RParen) {
                    self.bump();
                    Vec::new()
                } else {
                    self.parse_delimited(TokenKind::RParen, |s| s.parse_type())?
                };
                TypeKind::Tuple(tys)
            }
            TokenKind::Underscore => {
                self.bump();
                TypeKind::Infer
            }
            TokenKind::Impl => {
                // `impl Trait` in type position (parameter types, return-position
                // impl Trait, `impl FnMut(..)`). The Phase-1 subset does not model
                // existential/`impl Trait` types, so it is parsed as the trait path
                // opaquely (generic/tuple arguments are consumed and ignored). This
                // keeps the compiler's own source parseable.
                self.bump();
                let path = self.parse_path(true)?;
                if self.eat(&TokenKind::LParen) {
                    // `impl FnMut(&str) -> Ret (+ Bound)*`-style closure/trait
                    // types: consume the balanced paren list, the optional return
                    // type, and any trailing `+ Bound` so the closure/trait-object
                    // types in the compiler's own source parse (represented
                    // opaquely as the leading path). Parse-only; ignored.
                    let mut depth = 1;
                    while depth > 0 {
                        match self.peek_kind() {
                            TokenKind::LParen => {
                                self.bump();
                                depth += 1;
                            }
                            TokenKind::RParen => {
                                self.bump();
                                depth -= 1;
                            }
                            TokenKind::Eof => break,
                            _ => {
                                self.bump();
                            }
                        }
                    }
                    if self.eat(&TokenKind::Arrow) {
                        self.parse_type()?;
                    }
                    while self.eat(&TokenKind::Plus) {
                        // A dropped lifetime bound (`+ 'static`) leaves a dangling
                        // `+` before the enclosing delimiter; stop instead of
                        // parsing it as a path.
                        if self.at_bound_list_end() {
                            break;
                        }
                        self.parse_path(true)?;
                    }
                }
                TypeKind::Path(path, Vec::new())
            }
            // `dyn Trait` / `dyn Fn(..) -> R + Send`: the trait path stands for the
            // object type; the argument list and extra bounds are parsed and dropped.
            TokenKind::Ident(name) if name == "dyn" => {
                self.bump();
                let path = self.parse_path(true)?;
                if self.eat(&TokenKind::LParen) {
                    self.parse_delimited(TokenKind::RParen, |s| s.parse_type())?;
                    if self.eat(&TokenKind::Arrow) {
                        self.parse_type()?;
                    }
                }
                while self.eat(&TokenKind::Plus) {
                    if self.at_bound_list_end() {
                        break;
                    }
                    self.parse_bound()?;
                }
                TypeKind::Path(path, Vec::new())
            }
            _ if self.is_primitive_type() || matches!(self.peek_kind(), TokenKind::Ident(_)) => {
                let path = self.parse_path(false)?;
                // `dyn Fn(..) -> ..` / `impl Fn(..)` / bare `Fn(..)` trait types:
                // consume the parenthesized argument list, optional `-> Ret`, and
                // any `+ Bound` trait-object bounds so the closure/trait-object
                // types in the compiler's own source parse (represented opaquely
                // as the leading path).
                let last = path.segments.last().map(|s| s.node.clone());
                let is_fn_like =
                    matches!(last.as_deref(), Some("Fn") | Some("FnMut") | Some("FnOnce"));
                let is_object = matches!(last.as_deref(), Some("dyn") | Some("impl"));
                if (is_fn_like || is_object)
                    && self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&TokenKind::LParen)
                {
                    self.consume_fn_trait()?;
                }
                let args = if self.eat_lt() {
                    self.parse_generic_args()?
                } else {
                    Vec::new()
                };
                TypeKind::Path(path, args)
            }
            TokenKind::Bang => {
                self.bump();
                TypeKind::Never
            }
            // A leading `::` (global path `::core::fmt::Result`) names the crate root.
            TokenKind::ColonColon => {
                self.bump();
                return self.parse_type();
            }
            // `&&T` is a reference to a reference (the lexer emits one `&&` token).
            TokenKind::AmpAmp => {
                self.bump();
                let is_mut = self.eat(&TokenKind::Mut);
                let inner = self.parse_type()?;
                let mid = Type {
                    kind: if is_mut {
                        TypeKind::RefMut(Box::new(inner))
                    } else {
                        TypeKind::Ref(Box::new(inner))
                    },
                    span: Span::new(start, start),
                };
                TypeKind::Ref(Box::new(mid))
            }
            // Qualified type `<T as Trait>::Name`: the trait is dropped and the path
            // is named by the self type's name and the associated name.
            TokenKind::Lt => {
                self.bump();
                let self_ty = self.parse_type()?;
                if self.eat(&TokenKind::As) {
                    self.parse_path(true)?;
                }
                self.expect_gt()?;
                self.expect(TokenKind::ColonColon)?;
                let mut segments = match &self_ty.kind {
                    TypeKind::Path(p, _) => p.segments.clone(),
                    _ => vec![Spanned::new("<qualified>".to_string(), self_ty.span)],
                };
                segments.push(self.expect_name()?);
                TypeKind::Path(Path { segments }, Vec::new())
            }
            // `unsafe fn(..)` is a function-pointer type; `unsafe` is not modeled.
            TokenKind::Unsafe => {
                self.bump();
                return self.parse_type();
            }
            // Function-pointer type `fn(A, B) -> R`, carried as a path named `fn`
            // whose arguments are the parameter types followed by the return type.
            TokenKind::Fn => {
                let name_span = self.bump().span;
                self.expect(TokenKind::LParen)?;
                let mut args = Vec::new();
                loop {
                    if self.eat(&TokenKind::RParen) {
                        break;
                    }
                    // Named parameter `name: Type` (`unsafe fn(searcher: &S)`): the
                    // name is dropped.
                    if matches!(self.peek_kind(), TokenKind::Ident(_))
                        && self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&TokenKind::Colon)
                    {
                        self.bump();
                        self.bump();
                    }
                    args.push(self.parse_type()?);
                    if !self.eat(&TokenKind::Comma) {
                        self.expect(TokenKind::RParen)?;
                        break;
                    }
                }
                if self.eat(&TokenKind::Arrow) {
                    args.push(self.parse_type()?);
                }
                TypeKind::Path(
                    Path {
                        segments: vec![Spanned::new("fn".to_string(), name_span)],
                    },
                    args,
                )
            }
            other => {
                return Err(Diag::error(
                    format!("expected type, found {}", other),
                    Some(self.peek().span),
                ))
            }
        };
        let end = self.peek().span.start;
        Ok(Type {
            kind,
            span: Span::new(start, end),
        })
    }

    /// Consume a `Fn(Args) -> Ret (+ Bound)*` trait-type tail. Parse-only: the
    /// argument list, return type, and trailing trait-object bounds are parsed
    /// and discarded so closure/trait-object types in the compiler's own source
    /// parse. The leading path (e.g. `Fn`, `dyn`) already carries the name.
    fn consume_fn_trait(&mut self) -> ParseResult<()> {
        self.bump(); // `Fn`/`FnMut`/`FnOnce`
        self.expect(TokenKind::LParen)?;
        self.parse_delimited(TokenKind::RParen, |s| s.parse_type())?;
        if self.eat(&TokenKind::Arrow) {
            self.parse_type()?;
        }
        while self.eat(&TokenKind::Plus) {
            // A dropped lifetime bound (`+ 'static`) leaves a dangling `+` before
            // the enclosing delimiter; stop instead of parsing it as a path.
            if matches!(
                self.peek_kind(),
                TokenKind::Comma | TokenKind::LBrace | TokenKind::Gt | TokenKind::GtGt
            ) {
                break;
            }
            self.parse_path(true)?;
        }
        Ok(())
    }

    /// Parse a single trait bound in a `where` clause or generic parameter list.
    /// Returns `None` for `Fn`-family trait types (which the Phase-1 subset
    /// cannot model), so their arguments/bounds are consumed and dropped to keep
    /// parsing alive.
    fn parse_bound(&mut self) -> ParseResult<Option<TypeBound>> {
        self.skip_hrtb();
        // `?Sized` relaxes a bound; it is parsed and not modeled.
        if self.eat(&TokenKind::Question) {
            self.parse_path(true)?;
            return Ok(None);
        }
        if let TokenKind::Ident(name) = self.peek_kind().clone() {
            if (name == "Fn" || name == "FnMut" || name == "FnOnce")
                && self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&TokenKind::LParen)
            {
                self.consume_fn_trait()?;
                return Ok(None);
            }
        }
        Ok(Some(TypeBound::Trait(self.parse_path(true)?)))
    }

    fn is_primitive_type(&self) -> bool {
        use TokenKind::*;
        matches!(
            self.peek_kind(),
            I8 | I16
                | I32
                | I64
                | I128
                | Isize
                | U8
                | U16
                | U32
                | U64
                | U128
                | Usize
                | Bool
                | CharKw
                | StrKw
                | F32
                | F64
        )
    }
}

/// Whether a token is a primitive-type keyword usable as a type path segment.
fn is_primitive_keyword(k: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        k,
        I8 | I16
            | I32
            | I64
            | I128
            | Isize
            | U8
            | U16
            | U32
            | U64
            | U128
            | Usize
            | Bool
            | CharKw
            | StrKw
            | F32
            | F64
    )
}

/// Map a primitive-type keyword token to its textual name.
fn primitive_name(k: &TokenKind) -> String {
    use TokenKind::*;
    match k {
        I8 => "i8",
        I16 => "i16",
        I32 => "i32",
        I64 => "i64",
        I128 => "i128",
        Isize => "isize",
        U8 => "u8",
        U16 => "u16",
        U32 => "u32",
        U64 => "u64",
        U128 => "u128",
        Usize => "usize",
        Bool => "bool",
        CharKw => "char",
        StrKw => "str",
        F32 => "f32",
        F64 => "f64",
        _ => "",
    }
    .to_string()
}
