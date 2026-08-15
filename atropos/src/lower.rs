//! Lower the Clotho AST into the Atropos MIR (SSA form) for the Phase-1 subset.

use crate::mir;
use clotho::ast::{self, BinOp as ABinOp, Expr, ExprKind, ItemKind, Pat, Program, Stmt, StmtKind, UnOp as AUnOp};
use std::collections::HashMap;
use std::fmt;

/// An error encountered while lowering to MIR.
#[derive(Debug, Clone)]
pub struct LowerError(pub String);

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lowering error: {}", self.0)
    }
}

type Result<T> = std::result::Result<T, LowerError>;

/// Lower a whole program. `use` declarations are ignored; unsupported items
/// (enum, const, static, nested items) produce an error.
pub fn lower_program(prog: &Program) -> Result<mir::Program> {
    let mut out = mir::Program::default();
    for item in &prog.items {
        match &item.kind {
            ItemKind::Struct(s) => {
                let fields = s
                    .fields
                    .iter()
                    .map(|f| (f.name.node.clone(), type_to_c(&f.ty)))
                    .collect();
                out.structs.push(mir::StructDef {
                    name: s.name.node.clone(),
                    fields,
                });
            }
            ItemKind::Use(_) => { /* ignored in Phase 1 */ }
            ItemKind::Function(f) => {
                let l = Lower::new(&out.structs);
                let func = l.lower_function(f)?;
                out.functions.push(func);
            }
            other => {
                return Err(LowerError(format!(
                    "unsupported top-level item in codegen subset: {:?}",
                    matches!(other, ItemKind::Enum(_) | ItemKind::Const(_) | ItemKind::Static(_))
                )));
            }
        }
    }
    Ok(out)
}

/// Convert a Clotho type to a C type string, or `"long long"` when unknown.
fn type_to_c(ty: &ast::Type) -> String {
    match &ty.kind {
        ast::TypeKind::Path(p) => p
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_else(|| "long long".to_string()),
        ast::TypeKind::Ref(inner) => format!("{}*", type_to_c(inner)),
        ast::TypeKind::RefMut(inner) => format!("{}*", type_to_c(inner)),
        _ => "long long".to_string(),
    }
}

struct Lower {
    structs: Vec<mir::StructDef>,
    func: mir::Function,
    temp_counter: usize,
    current: usize,
    /// Inferred types for SSA temporaries. `long long` when unknown.
    types: HashMap<String, String>,
    /// Stack of `(break_target, continue_target)` for loops.
    loops: Vec<(usize, usize)>,
}

impl Lower {
    fn new(structs: &[mir::StructDef]) -> Self {
        Lower {
            structs: structs.to_vec(),
            func: mir::Function {
                name: String::new(),
                params: Vec::new(),
                ret: None,
                locals: Vec::new(),
                temp_types: HashMap::new(),
                blocks: Vec::new(),
            },
            temp_counter: 0,
            current: 0,
            types: HashMap::new(),
            loops: Vec::new(),
        }
    }

    fn lower_function(mut self, f: &ast::Function) -> Result<mir::Function> {
        self.func.name = f.name.node.clone();
        self.func.ret = f.ret.as_ref().map(type_to_c);
        for p in &f.params {
            let name = pat_name(&p.pat)?;
            let ty = type_to_c(&p.ty);
            self.func.params.push(name.clone());
            self.func.locals.push((name, Some(ty)));
        }
        self.new_block(); // entry = block 0, current = 0
        let value = self.lower_block_value(&f.body)?;
        // Terminate the function with a return of the tail value.
        if let mir::Terminator::Unreachable = self.func.blocks[self.current].term {
            let ret = if self.func.ret.is_some() {
                Some(value)
            } else {
                None
            };
            self.set_term(mir::Terminator::Return(ret));
        }
        Ok(self.func)
    }

    fn new_block(&mut self) -> usize {
        self.func.blocks.push(mir::Block::default());
        self.func.blocks.len() - 1
    }

    fn set_term(&mut self, term: mir::Terminator) {
        self.func.blocks[self.current].term = term;
    }

    fn emit(&mut self, place: String, rvalue: mir::Rvalue) {
        self.func.blocks[self.current].stmts.push(mir::Stmt { place, rvalue });
    }

    fn fresh_temp(&mut self, ty: impl Into<String>) -> String {
        let t = format!("t{}", self.temp_counter);
        self.temp_counter += 1;
        self.types.insert(t.clone(), ty.into());
        t
    }

    fn temp_type_of(&self, name: &str) -> String {
        self.types.get(name).cloned().unwrap_or_else(|| "long long".to_string())
    }

    /// Lower a block, returning the operand holding its value (tail expression
    /// or `()` when there is none).
    fn lower_block_value(&mut self, b: &ast::Block) -> Result<mir::Operand> {
        for s in &b.stmts {
            self.lower_stmt(s)?;
        }
        match &b.tail {
            Some(tail) => self.lower_expr(tail),
            None => Ok(mir::Operand::Unit),
        }
    }

    fn lower_stmt(&mut self, s: &Stmt) -> Result<()> {
        match &s.kind {
            StmtKind::Local(local) => {
                let name = pat_name(&local.pat)?;
                let ty = local.ty.as_ref().map(type_to_c);
                self.func.locals.push((name.clone(), ty.clone()));
                if let Some(init) = &local.init {
                    let v = self.lower_expr(init)?;
                    self.emit(name.clone(), mir::Rvalue::Use(v));
                }
                Ok(())
            }
            StmtKind::Semi(e) | StmtKind::Expr(e) => {
                self.lower_expr(e)?;
                Ok(())
            }
            StmtKind::Item(_) => Err(LowerError(
                "nested items are not supported in the codegen subset".into(),
            )),
        }
    }

    fn lower_expr(&mut self, e: &Expr) -> Result<mir::Operand> {
        match &e.kind {
            ExprKind::Lit(ast::Lit::Int(v, _)) => Ok(mir::Operand::Const(*v as i128)),
            ExprKind::Lit(ast::Lit::Bool(b)) => {
                Ok(mir::Operand::Const(if *b { 1 } else { 0 }))
            }
            ExprKind::Lit(ast::Lit::Float(text, _)) => {
                // Store as integer bits lossily; demo avoids floats in codegen.
                let f: f64 = text.parse().unwrap_or(0.0);
                Ok(mir::Operand::Const(f as i128))
            }
            ExprKind::Lit(_) => Err(LowerError(
                "string/char literals are not supported in the codegen subset".into(),
            )),
            ExprKind::Path(p) => {
                let name = p.segments.last().map(|s| s.node.clone()).unwrap_or_default();
                Ok(mir::Operand::var(name))
            }
            ExprKind::Binary(l, abop, r) => {
                // Assignment / compound assignment mutate the left-hand place.
                if let Some(arith) = compound_op(*abop) {
                    let lhs = self.lower_expr(l)?;
                    let rhs = self.lower_expr(r)?;
                    let place = match &lhs {
                        mir::Operand::Var(n) => n.clone(),
                        _ => return Err(LowerError("assignment target must be a variable".into())),
                    };
                    let rvalue = mir::Rvalue::Binary(arith, lhs, rhs);
                    self.emit(place.clone(), rvalue);
                    return Ok(mir::Operand::var(place));
                }
                if matches!(abop, ABinOp::Assign) {
                    let lhs = self.lower_expr(l)?;
                    let rhs = self.lower_expr(r)?;
                    let place = match &lhs {
                        mir::Operand::Var(n) => n.clone(),
                        _ => return Err(LowerError("assignment target must be a variable".into())),
                    };
                    self.emit(place.clone(), mir::Rvalue::Use(rhs));
                    return Ok(mir::Operand::var(place));
                }
                if matches!(abop, ABinOp::Range) {
                    return Err(LowerError(
                        "range `..` is only supported as a `for` iterator".into(),
                    ));
                }
                let mop = map_binop(*abop)?;
                let l = self.lower_expr(l)?;
                let r = self.lower_expr(r)?;
                let t = self.fresh_temp("long long");
                self.emit(t.clone(), mir::Rvalue::Binary(mop, l, r));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Unary(aop, e) => {
                let mop = match aop {
                    AUnOp::Neg => mir::UnOp::Neg,
                    AUnOp::Not => mir::UnOp::Not,
                    AUnOp::Deref => mir::UnOp::Deref,
                };
                let o = self.lower_expr(e)?;
                let t = self.fresh_temp("long long");
                self.emit(t.clone(), mir::Rvalue::Unary(mop, o));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Call(callee, args) => {
                let name = match &callee.kind {
                    ExprKind::Path(p) => p.segments.last().map(|s| s.node.clone()).unwrap_or_default(),
                    _ => return Err(LowerError("call target must be a path".into())),
                };
                let mut operands = Vec::new();
                for a in args {
                    operands.push(self.lower_expr(a)?);
                }
                let t = self.fresh_temp("long long");
                self.emit(t.clone(), mir::Rvalue::Call(name, operands));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Field(base, field) => {
                let o = self.lower_expr(base)?;
                let base_ty = self.temp_type_of(&name_of(&o));
                let field_ty = lookup_field_type(&self.structs, base_ty.as_str(), &field.node)
                    .unwrap_or_else(|| "long long".to_string());
                let t = self.fresh_temp(field_ty);
                self.emit(t.clone(), mir::Rvalue::Field(o, field.node.clone()));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Ref(e) => {
                let o = self.lower_expr(e)?;
                let inner = self.temp_type_of(&name_of(&o));
                let t = self.fresh_temp(format!("{}*", inner));
                self.emit(t.clone(), mir::Rvalue::Ref(o));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Struct(path, fields, _base) => {
                let name = path.segments.last().map(|s| s.node.clone()).unwrap_or_default();
                let mut fvals = Vec::new();
                for (fname, fval) in fields {
                    fvals.push((fname.node.clone(), self.lower_expr(fval)?));
                }
                let t = self.fresh_temp(name.clone());
                self.emit(t.clone(), mir::Rvalue::StructLit(name, fvals));
                Ok(mir::Operand::var(t))
            }
            ExprKind::Block(b) => self.lower_block_value(b),
            ExprKind::If(cond, then_b, else_b) => {
                let cond_op = self.lower_expr(cond)?;
                let result = self.fresh_temp("long long");
                let then_blk = self.new_block();
                let else_blk = self.new_block();
                let merge = self.new_block();
                self.set_term(mir::Terminator::If(cond_op, then_blk, else_blk));

                self.current = then_blk;
                let v_then = self.lower_block_value(then_b)?;
                self.emit(result.clone(), mir::Rvalue::Use(v_then));
                self.set_term(mir::Terminator::Goto(merge));

                self.current = else_blk;
                let v_else = match else_b {
                    Some(eb) => self.lower_expr(eb)?,
                    None => mir::Operand::Unit,
                };
                self.emit(result.clone(), mir::Rvalue::Use(v_else));
                self.set_term(mir::Terminator::Goto(merge));

                self.current = merge;
                Ok(mir::Operand::var(result))
            }
            ExprKind::While(cond, body) => self.lower_loop(Some(&**cond), body),
            ExprKind::Loop(body) => self.lower_loop(None, body),
            ExprKind::For(pat, iter, body) => {
                // Desugar `for i in a..b { ... }` into a while loop over `i`.
                let name = pat_name(pat)?;
                let (a, b) = match &iter.kind {
                    ExprKind::Binary(l, ABinOp::Range, r) => (l, r),
                    _ => return Err(LowerError("`for` iterator must be a range `a..b`".into())),
                };
                let a_op = self.lower_expr(a)?;
                let b_op = self.lower_expr(b)?;
                self.func.locals.push((name.clone(), Some("long long".into())));
                self.emit(name.clone(), mir::Rvalue::Use(a_op));

                let head = self.new_block();
                let body_blk = self.new_block();
                let exit = self.new_block();
                self.set_term(mir::Terminator::Goto(head));

                self.current = head;
                let cond = mir::Operand::var(name.clone());
                let cmp = self.fresh_temp("long long");
                self.emit(
                    cmp.clone(),
                    mir::Rvalue::Binary(mir::BinOp::Lt, cond, b_op),
                );
                self.set_term(mir::Terminator::If(mir::Operand::var(cmp), body_blk, exit));

                self.current = body_blk;
                self.loops.push((exit, head));
                self.lower_block_value(body)?;
                self.loops.pop();
                // i = i + 1
                let i = mir::Operand::var(name.clone());
                self.emit(name.clone(), mir::Rvalue::Binary(mir::BinOp::Add, i, mir::Operand::Const(1)));
                self.set_term(mir::Terminator::Goto(head));

                self.current = exit;
                Ok(mir::Operand::Unit)
            }
            ExprKind::Return(opt) => {
                let v = match opt {
                    Some(e) => Some(self.lower_expr(e)?),
                    None => None,
                };
                self.set_term(mir::Terminator::Return(v));
                // Subsequent statements land in a fresh unreachable block.
                self.current = self.new_block();
                Ok(mir::Operand::Unit)
            }
            ExprKind::Break => {
                let (exit, _cont) = self
                    .loops
                    .last()
                    .copied()
                    .ok_or_else(|| LowerError("`break` outside of a loop".into()))?;
                self.set_term(mir::Terminator::Goto(exit));
                self.current = self.new_block();
                Ok(mir::Operand::Unit)
            }
            ExprKind::Continue => {
                let (_exit, cont) = self
                    .loops
                    .last()
                    .copied()
                    .ok_or_else(|| LowerError("`continue` outside of a loop".into()))?;
                self.set_term(mir::Terminator::Goto(cont));
                self.current = self.new_block();
                Ok(mir::Operand::Unit)
            }
            ExprKind::Match(..) => Err(LowerError("`match` is not in the codegen subset".into())),
            ExprKind::MethodCall(..) => {
                Err(LowerError("method calls are not in the codegen subset".into()))
            }
            ExprKind::Index(..) => Err(LowerError("indexing is not in the codegen subset".into())),
            ExprKind::Array(..) => Err(LowerError("arrays are not in the codegen subset".into())),
            ExprKind::Deref(e) => {
                let o = self.lower_expr(e)?;
                let t = self.fresh_temp("long long");
                self.emit(t.clone(), mir::Rvalue::Unary(mir::UnOp::Deref, o));
                Ok(mir::Operand::var(t))
            }
        }
    }

    fn lower_loop(
        &mut self,
        cond: Option<&Expr>,
        body: &ast::Block,
    ) -> Result<mir::Operand> {
        let head = self.new_block();
        let body_blk = self.new_block();
        let exit = self.new_block();
        self.set_term(mir::Terminator::Goto(head));

        self.current = head;
        let entry = if let Some(c) = cond {
            let c_op = self.lower_expr(c)?;
            self.set_term(mir::Terminator::If(c_op, body_blk, exit));
            body_blk
        } else {
            self.set_term(mir::Terminator::Goto(body_blk));
            body_blk
        };

        self.current = entry;
        self.loops.push((exit, head));
        self.lower_block_value(body)?;
        self.loops.pop();
        self.set_term(mir::Terminator::Goto(head));

        self.current = exit;
        Ok(mir::Operand::Unit)
    }
}

fn pat_name(p: &Pat) -> Result<String> {
    match p {
        Pat::Ident(n) => Ok(n.node.clone()),
        Pat::Wild => Ok("_".to_string()),
        _ => Err(LowerError("only identifier patterns are supported".into())),
    }
}

fn name_of(o: &mir::Operand) -> String {
    match o {
        mir::Operand::Var(n) => n.clone(),
        _ => String::new(),
    }
}

fn compound_op(op: ABinOp) -> Option<mir::BinOp> {
    Some(match op {
        ABinOp::AddAssign => mir::BinOp::Add,
        ABinOp::SubAssign => mir::BinOp::Sub,
        ABinOp::MulAssign => mir::BinOp::Mul,
        ABinOp::DivAssign => mir::BinOp::Div,
        ABinOp::RemAssign => mir::BinOp::Rem,
        ABinOp::BitAndAssign => mir::BinOp::BitAnd,
        ABinOp::BitOrAssign => mir::BinOp::BitOr,
        ABinOp::BitXorAssign => mir::BinOp::BitXor,
        ABinOp::ShlAssign => mir::BinOp::Shl,
        ABinOp::ShrAssign => mir::BinOp::Shr,
        _ => return None,
    })
}

fn map_binop(op: ABinOp) -> Result<mir::BinOp> {
    Ok(match op {
        ABinOp::Add => mir::BinOp::Add,
        ABinOp::Sub => mir::BinOp::Sub,
        ABinOp::Mul => mir::BinOp::Mul,
        ABinOp::Div => mir::BinOp::Div,
        ABinOp::Rem => mir::BinOp::Rem,
        ABinOp::And => mir::BinOp::And,
        ABinOp::Or => mir::BinOp::Or,
        ABinOp::BitAnd => mir::BinOp::BitAnd,
        ABinOp::BitOr => mir::BinOp::BitOr,
        ABinOp::BitXor => mir::BinOp::BitXor,
        ABinOp::Shl => mir::BinOp::Shl,
        ABinOp::Shr => mir::BinOp::Shr,
        ABinOp::Eq => mir::BinOp::Eq,
        ABinOp::Ne => mir::BinOp::Ne,
        ABinOp::Lt => mir::BinOp::Lt,
        ABinOp::Gt => mir::BinOp::Gt,
        ABinOp::Le => mir::BinOp::Le,
        ABinOp::Ge => mir::BinOp::Ge,
        other => {
            return Err(LowerError(format!(
                "operator {:?} is not supported in the codegen subset",
                other
            )))
        }
    })
}

fn lookup_field_type(
    structs: &[mir::StructDef],
    base_ty: &str,
    field: &str,
) -> Option<String> {
    structs
        .iter()
        .find(|s| s.name == base_ty)
        .and_then(|s| s.fields.iter().find(|(f, _)| f == field))
        .map(|(_, t)| t.clone())
}
