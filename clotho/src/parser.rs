//! Clotho recursive-descent parser for the Phase-1 Rust subset.
// MARKER_KILO_EDITS_ACTIVE
//!
//! The parser is a Pratt-style precedence parser driven by a cursor over the
//! token stream produced by [`crate::lexer::Lexer`]. It is single-pass with a
//! bounded lookahead of two tokens.

use crate::ast::*;
use crate::error::{Diag, ParseResult};
use crate::token::{Span, Token, TokenKind};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    pub fn from_source(src: &str) -> Self {
        Parser::new(crate::lexer::tokenize(src))
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
        let t = self.tokens.get(self.pos).cloned().unwrap();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Eof)
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
            items.push(self.parse_item()?);
        }
        Ok(Program { items })
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
        if matches!(
            self.tokens.get(i).map(|t| &t.kind),
            Some(TokenKind::Unsafe)
        ) {
            i += 1;
        }
        matches!(
            self.tokens.get(i).map(|t| &t.kind),
            Some(TokenKind::Ident(n))
                if n == "trait" || n == "macro_rules" || n == "extern" || n == "union"
        )
    }

    /// Consume a complete item we don't model (balanced braces, or up to the next
    /// `;` at brace-depth 0, or EOF).
    fn skip_rest_of_item(&mut self) -> ParseResult<()> {
        let mut depth = 0i32;
        loop {
            match self.peek_kind() {
                TokenKind::Eof => break,
                TokenKind::LBrace => {
                    self.bump();
                    depth += 1;
                }
                TokenKind::RBrace => {
                    if depth == 0 {
                        break;
                    }
                    self.bump();
                    depth -= 1;
                }
                TokenKind::Semicolon => {
                    if depth == 0 {
                        self.bump();
                        break;
                    }
                    self.bump();
                }
                _ => {
                    self.bump();
                }
            }
        }
        Ok(())
    }

    fn parse_item(&mut self) -> ParseResult<Item> {
        loop {
            let _ = self.skip_attributes();
            let visibility = self.parse_visibility();
            // Items the Phase-1 subset does not yet model (e.g. `trait`,
            // `macro_rules!`, `extern`) are dropped silently so the program still
            // parses. The self-host probe tolerates this; full modeling is a
            // tracked Phase-3/4 follow-up.
            if self.is_skippable_item() {
                self.skip_rest_of_item()?;
                continue;
            }
            let kind = self.parse_item_kind()?;
            return Ok(Item {
                visibility,
                kind,
            });
        }
    }

    fn parse_item_kind(&mut self) -> ParseResult<ItemKind> {
        // `unsafe fn` / `unsafe impl` keep their unsafe-ness; `unsafe trait` is
        // dropped by the caller's `is_skippable_item` check.
        let is_unsafe = self.eat(&TokenKind::Unsafe);
        // `impl` lexes as an identifier (it is not a keyword token), so detect
        // it by name before the keyword-driven match below.
        if let TokenKind::Ident(name) = self.peek_kind() {
            if name == "impl" {
                return Ok(ItemKind::Impl(self.parse_impl(is_unsafe)?));
            }
            if name == "mod" {
                return Ok(ItemKind::Module(self.parse_module()?));
            }
            if name == "type" {
                return Ok(ItemKind::TypeAlias(self.parse_type_alias()?));
            }
        }
        match self.peek_kind() {
            TokenKind::Fn | TokenKind::Async => {
                Ok(ItemKind::Function(self.parse_function(is_unsafe)?))
            }
            TokenKind::Struct => Ok(ItemKind::Struct(self.parse_struct()?)),
            TokenKind::Enum => Ok(ItemKind::Enum(self.parse_enum()?)),
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
        self.expect(TokenKind::Eq)?;
        let ty = self.parse_type()?;
        self.eat(&TokenKind::Semicolon);
        Ok(TypeAliasDef { name, ty })
    }

    fn parse_function(&mut self, is_unsafe: bool) -> ParseResult<Function> {
        let is_async = self.eat(&TokenKind::Async);
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        let generics = self.parse_generics()?;
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                params.push(self.parse_param()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RParen)?;
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
            let is_mut = if is_ref { self.eat(&TokenKind::Mut) } else { false };
            let name = self.expect_ident()?; // `self`
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
            TokenKind::Amp => match self.tokens.get(self.pos + 1).map(|t| &t.kind) {
                Some(TokenKind::Ident(n)) => n.as_str() == "self",
                Some(TokenKind::Mut) => true,
                _ => false,
            },
            _ => false,
        }
    }

    fn parse_struct(&mut self) -> ParseResult<StructDef> {
        self.expect(TokenKind::Struct)?;
        let name = self.expect_ident()?;
        let generics = self.parse_generics()?;
        self.expect(TokenKind::LBrace)?;
        let mut fields = Vec::new();
        while !self.eat(&TokenKind::RBrace) {
            let fname = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let ty = self.parse_type()?;
            fields.push(Field { name: fname, ty });
            if !self.eat(&TokenKind::Comma) {
                self.expect(TokenKind::RBrace)?;
                break;
            }
        }
        Ok(StructDef { name, generics, fields })
    }

    fn parse_enum(&mut self) -> ParseResult<EnumDef> {
        self.expect(TokenKind::Enum)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LBrace)?;
        let mut variants = Vec::new();
        while !self.eat(&TokenKind::RBrace) {
            let vname = self.expect_ident()?;
            let fields = if self.eat(&TokenKind::LParen) {
                let mut fs = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    loop {
                        let fname = self.expect_ident()?;
                        self.expect(TokenKind::Colon)?;
                        let ty = self.parse_type()?;
                        fs.push(Field { name: fname, ty });
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RParen)?;
                Some(fs)
            } else {
                None
            };
            variants.push(Variant {
                name: vname,
                fields,
            });
            if !self.eat(&TokenKind::Comma) {
                self.expect(TokenKind::RBrace)?;
                break;
            }
        }
        Ok(EnumDef { name, variants })
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
            items.push(self.parse_item()?);
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
                items.push(self.parse_item()?);
            }
            ModuleKind::Inline(items)
        };
        Ok(ModuleDef { name, kind })
    }

    /// Parse an optional generic parameter list `<T: Bound, U>` (empty if the
    /// next token is not `<`). The list is the Phase-3 "trait bounds" surface
    /// for item-level generic declarations.
    fn parse_generics(&mut self) -> ParseResult<Generics> {
        if !self.eat(&TokenKind::Lt) {
            return Ok(Generics::default());
        }
        let mut params = Vec::new();
        if !self.check(&TokenKind::Gt) {
            loop {
                let name = self.expect_ident()?;
                let mut bounds = Vec::new();
                if self.eat(&TokenKind::Colon) {
                    loop {
                        let path = self.parse_path()?;
                        bounds.push(TypeBound::Trait(path));
                        if !self.eat(&TokenKind::Plus) {
                            break;
                        }
                    }
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
            let ty = self.parse_type()?;
            self.expect(TokenKind::Colon)?;
            let mut bounds = Vec::new();
            loop {
                let path = self.parse_path()?;
                bounds.push(TypeBound::Trait(path));
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
                self.tokens
                    .insert(self.pos + 1, Token { kind: TokenKind::Gt, span: tok.span });
                self.tokens[self.pos] = Token { kind: TokenKind::Gt, span: tok.span };
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
        if !self.check(&TokenKind::Gt) && !matches!(self.peek_kind(), TokenKind::GtGt) {
            loop {
                args.push(self.parse_type()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_gt()?;
        Ok(args)
    }

    fn parse_use_tree(&mut self) -> ParseResult<UseTree> {
        let mut prefix = Vec::new();
        prefix.push(self.expect_ident()?);
        while self.eat(&TokenKind::ColonColon) {
            if self.eat(&TokenKind::Star) {
                return Ok(UseTree {
                    prefix,
                    kind: UseKind::Glob,
                });
            }
            // nested braces
            if self.eat(&TokenKind::LBrace) {
                let mut nested = Vec::new();
                if !self.check(&TokenKind::RBrace) {
                    loop {
                        nested.push(self.parse_use_tree()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RBrace)?;
                return Ok(UseTree {
                    prefix,
                    kind: UseKind::Nested(nested),
                });
            }
            prefix.push(self.expect_ident()?);
        }
        Ok(UseTree {
            prefix,
            kind: UseKind::Leaf,
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

    /// Skip a leading attribute group, plus any leading `pub`/`unsafe`/`async`
    /// visibility/modifier keyword so that the real item parser sees a clean
    /// token stream. The Phase-1 lexer does not emit attribute tokens, so this
    /// is effectively a no-op placeholder reserved for future attribute
    /// support.
    fn skip_attributes(&mut self) -> ParseResult<()> {
        Ok(())
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

    fn parse_stmt(&mut self) -> ParseResult<Stmt> {
        match self.peek_kind() {
            TokenKind::Let => {
                let local = self.parse_local()?;
                Ok(Stmt {
                    kind: StmtKind::Local(local),
                })
            }
            TokenKind::Fn
            | TokenKind::Struct
            | TokenKind::Enum
            | TokenKind::Const
            | TokenKind::Static
            | TokenKind::Use => {
                let item = self.parse_item()?;
                Ok(Stmt {
                    kind: StmtKind::Item(item),
                })
            }
            TokenKind::Return
            | TokenKind::Break
            | TokenKind::Continue
            | TokenKind::While
            | TokenKind::Loop
            | TokenKind::For
            | TokenKind::Match => {
                let e = self.parse_expr()?;
                self.eat(&TokenKind::Semicolon);
                Ok(Stmt {
                    kind: StmtKind::Semi(e),
                })
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

    fn parse_local(&mut self) -> ParseResult<Local> {
        self.expect(TokenKind::Let)?;
        let mutable = self.eat(&TokenKind::Mut);
        let pat = self.parse_pat()?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let init = if self.eat(&TokenKind::Eq) {
            Some(self.parse_expr()?)
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

    fn parse_pat(&mut self) -> ParseResult<Pat> {
        match self.peek_kind() {
            TokenKind::Underscore => {
                self.bump();
                Ok(Pat::Wild)
            }
            TokenKind::Ident(_) => {
                let name = self.expect_ident()?;
                Ok(Pat::Ident(name))
            }
            _ => Err(Diag::error(
                format!("expected pattern, found {}", self.peek_kind()),
                Some(self.peek().span),
            )),
        }
    }

    // ---- expressions ----

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
            let Some((l_bp, r_bp, op)) = self.binop_bp() else {
                break;
            };
            if l_bp < min_bp {
                break;
            }
            self.bump(); // consume the operator token
            let rhs = self.parse_expr_bp(r_bp, false)?;
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

    fn parse_unary_or_atom(&mut self, allow_struct: bool) -> ParseResult<Expr> {
        let start = self.peek().span.start;
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
                let mutable = self.eat(&TokenKind::Mut);
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

    fn parse_atom(&mut self, allow_struct: bool) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        let expr = match self.peek_kind().clone() {
            TokenKind::Unsafe => {
                return self.parse_unsafe_block();
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
            TokenKind::Ident(_) => {
                let path = self.parse_path()?;
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
                    // unit literal
                    ExprKind::Lit(Lit::Str(String::new()))
                } else {
                    let e = self.parse_expr()?;
                    self.expect(TokenKind::RParen)?;
                    return self.parse_postfix(Expr {
                        kind: ExprKind::Block(Block {
                            stmts: vec![],
                            tail: Some(Box::new(e)),
                        }),
                        span: Span::new(start, self.peek().span.start),
                    });
                }
            }
            TokenKind::LBrace => {
                let block = self.parse_block()?;
                ExprKind::Block(block)
            }
            TokenKind::LBracket => {
                self.bump();
                let mut elems = Vec::new();
                if !self.check(&TokenKind::RBracket) {
                    loop {
                        elems.push(self.parse_expr()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RBracket)?;
                ExprKind::Array(elems)
            }
            TokenKind::If => return self.parse_if(),
            TokenKind::While => return self.parse_while(),
            TokenKind::Loop => return self.parse_loop(),
            TokenKind::For => return self.parse_for(),
            TokenKind::Match => return self.parse_match(),
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
                    self.bump();
                    let mut args = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if !self.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(TokenKind::RParen)?;
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
                    if self.check(&TokenKind::Ident(String::new()))
                        || matches!(self.peek_kind(), TokenKind::Ident(_))
                    {
                        let method = self.expect_ident()?;
                        if self.eat(&TokenKind::LParen) {
                            let mut args = Vec::new();
                            if !self.check(&TokenKind::RParen) {
                                loop {
                                    args.push(self.parse_expr()?);
                                    if !self.eat(&TokenKind::Comma) {
                                        break;
                                    }
                                }
                            }
                            self.expect(TokenKind::RParen)?;
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
                TokenKind::Bang => {
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
                    let (delimiter, close) = if self.eat(&TokenKind::LParen) {
                        (MacroDelim::Paren, TokenKind::RParen)
                    } else if self.eat(&TokenKind::LBracket) {
                        (MacroDelim::Bracket, TokenKind::RBracket)
                    } else if self.eat(&TokenKind::LBrace) {
                        (MacroDelim::Brace, TokenKind::RBrace)
                    } else {
                        return Err(Diag::error(
                            "expected `(`, `[`, or `{` after macro name",
                            Some(self.peek().span),
                        ));
                    };
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
                                self.bump();
                                depth -= 1;
                            }
                            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                                self.bump();
                                depth += 1;
                            }
                            _ => {
                                tokens.push(self.bump());
                            }
                        }
                    }
                    let end = self.peek().span.start;
                    expr = Expr {
                        kind: ExprKind::Macro(MacroCall {
                            path,
                            delimiter,
                            tokens,
                        }),
                        span: Span::new(start, end),
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
            if self.eat(&TokenKind::DotDot) {
                let e = self.parse_expr()?;
                base = Some(Box::new(e));
                self.eat(&TokenKind::Comma);
                self.expect(TokenKind::RBrace)?;
                break;
            }
            let name = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let value = self.parse_expr()?;
            fields.push((name, value));
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
        let cond = self.parse_expr_bp(1, true)?;
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
        let cond = self.parse_expr_bp(1, true)?;
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
        let iter = self.parse_expr_bp(1, true)?;
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
        let scrutinee = self.parse_expr_bp(1, true)?;
        self.expect(TokenKind::LBrace)?;
        let mut arms = Vec::new();
        while !self.eat(&TokenKind::RBrace) {
            let pat = self.parse_pat()?;
            self.expect(TokenKind::FatArrow)?;
            let body = self.parse_expr()?;
            self.eat(&TokenKind::Comma);
            arms.push((pat, body));
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

    /// Parse a `let` expression (`if let`, `while let`, let-else) in expression
    /// position. The Phase-1 subset does not model pattern guards, so this is a
    /// structural parse that defers to `parse_local` + the surrounding
    /// expression context where possible.
    fn parse_let_expr(&mut self) -> ParseResult<Expr> {
        let start = self.peek().span.start;
        let local = self.parse_local()?;
        // A `let` expression standing alone is not supported by the Phase-1
        // subset; surface it as a blockless expression that lower/infer will
        // reject with a clear diagnostic.
        let _ = local;
        let e = self.parse_expr()?;
        Ok(Expr {
            kind: ExprKind::Block(Block {
                stmts: Vec::new(),
                tail: Some(Box::new(e)),
            }),
            span: Span::new(start, self.peek().span.start),
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
        let pat = self.parse_pat()?;
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

    fn parse_path(&mut self) -> ParseResult<Path> {
        let mut segments = Vec::new();
        segments.push(self.expect_name()?);
        loop {
            // Generic arguments in angle-bracket form (`Vec<i32>`, `Result<T, E>`).
            if self.eat(&TokenKind::Lt) {
                self.parse_generic_args()?;
            }
            if self.eat(&TokenKind::ColonColon) {
                // Turbofish `::<...>` between segments.
                if self.eat(&TokenKind::Lt) {
                    self.parse_generic_args()?;
                }
                segments.push(self.expect_name()?);
            } else {
                break;
            }
        }
        Ok(Path { segments })
    }

    /// Accept an identifier or a primitive-type keyword as a path segment name.
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
                    let n = match self.expect(TokenKind::Int {
                        value: 0,
                        suffix: None,
                    }) {
                        Ok(t) => match t.kind {
                            TokenKind::Int { value, .. } => value,
                            _ => 0,
                        },
                        Err(_) => 0,
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
                let mut tys = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    loop {
                        tys.push(self.parse_type()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RParen)?;
                TypeKind::Tuple(tys)
            }
            TokenKind::Underscore => {
                self.bump();
                TypeKind::Infer
            }
            _ if self.is_primitive_type() || matches!(self.peek_kind(), TokenKind::Ident(_)) => {
                let path = self.parse_path()?;
                let args = if self.eat(&TokenKind::Lt) {
                    self.parse_generic_args()?
                } else {
                    Vec::new()
                };
                TypeKind::Path(path, args)
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
