//! Clotho recursive-descent parser for the Phase-1 Rust subset.
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
            Visibility::Pub
        } else {
            Visibility::Private
        }
    }

    fn parse_item(&mut self) -> ParseResult<Item> {
        let visibility = self.parse_visibility();
        let kind = self.parse_item_kind()?;
        Ok(Item { visibility, kind })
    }

    fn parse_item_kind(&mut self) -> ParseResult<ItemKind> {
        match self.peek_kind() {
            TokenKind::Fn => Ok(ItemKind::Function(self.parse_function()?)),
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

    fn parse_function(&mut self) -> ParseResult<Function> {
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                let pat = self.parse_pat()?;
                self.expect(TokenKind::Colon)?;
                let ty = self.parse_type()?;
                params.push(Param { pat, ty });
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
        let body = self.parse_block()?;
        Ok(Function {
            name,
            params,
            ret,
            body,
        })
    }

    fn parse_struct(&mut self) -> ParseResult<StructDef> {
        self.expect(TokenKind::Struct)?;
        let name = self.expect_ident()?;
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
        Ok(StructDef { name, fields })
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

        while let Some((l_bp, r_bp, op)) = self.binop_bp() {
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
            TokenKind::Amp => {
                self.bump();
                self.eat(&TokenKind::Mut);
                let e = self.parse_unary_or_atom(allow_struct)?;
                ExprKind::Ref(Box::new(e))
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
                    } else {
                        return Err(Diag::error(
                            "expected field or method name after `.`",
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

    fn parse_path(&mut self) -> ParseResult<Path> {
        let mut segments = Vec::new();
        segments.push(self.expect_name()?);
        while self.eat(&TokenKind::ColonColon) {
            segments.push(self.expect_name()?);
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
                TypeKind::Path(path)
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
