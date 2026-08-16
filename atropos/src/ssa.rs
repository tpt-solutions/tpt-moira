//! Atropos SSA mid-end: a custom, lightweight SSA-style IR plus minimal
//! optimization passes.
//!
//! This is the "custom lightweight SSA backend" called for in the roadmap — it
//! is built from scratch (no Cranelift/LLVM dependency). Moira lowers the
//! Clotho AST into the straightforward [`mir`] first; [`build_ssa`] then rewrites
//! that into a single-assignment value graph where every pure operation binds a
//! fresh value and operands reference either another value (intra-block SSA) or
//! a named slot (for values that cross basic-block boundaries, e.g. the result
//! of an `if`/`else`). Cross-block names are kept as *mutable slots* rather than
//! φ-nodes; a stack-machine native backend consumes exactly this shape, so no
//! φ-lowering pass is needed.
//!
//! Optimization passes ([`constant_fold`], [`dce`]) run over this graph and are
//! what give Atropos its "minimal optimization passes" deliverable.

use crate::mir;
use std::collections::{HashMap, HashSet};

/// An operand of an SSA operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Val {
    /// A reference to another SSA value (intra-block definition).
    Value(usize),
    /// A reference to a named slot (parameter, local, or cross-block temp).
    Name(String),
    /// An integer constant.
    Const(i128),
    /// The unit value `()`.
    Unit,
}

/// The right-hand side of an SSA value binding.
#[derive(Debug, Clone)]
pub enum Op {
    Const(i128),
    Copy(Val),
    Binary(mir::BinOp, Val, Val),
    Unary(mir::UnOp, Val),
    Call(String, Vec<Val>),
    StructLit(String, Vec<(String, Val)>),
    Field(Val, String),
    Ref(Val),
}

/// A single SSA value: an operation producing a typed result.
#[derive(Debug, Clone)]
pub struct Value {
    pub op: Op,
    pub ty: String,
    /// The source name this value was bound to, if any (used for slot
    /// allocation and dead-code analysis of cross-block temps).
    pub name: Option<String>,
}

/// The terminating instruction of a basic block.
#[derive(Debug, Clone, Default)]
pub enum SsaTerm {
    Goto(usize),
    If(Val, usize, usize),
    Return(Option<Val>),
    #[default]
    Unreachable,
}

/// A basic block: a linear list of value indices followed by one terminator.
#[derive(Debug, Clone, Default)]
pub struct SsaBlock {
    pub stmts: Vec<usize>,
    pub term: SsaTerm,
}

/// A function in SSA form.
#[derive(Debug, Clone, Default)]
pub struct SsaFunction {
    pub name: String,
    pub params: Vec<String>,
    pub ret: Option<String>,
    pub locals: Vec<(String, Option<String>)>,
    pub blocks: Vec<SsaBlock>,
    pub values: Vec<Value>,
    pub structs: Vec<mir::StructDef>,
}

/// A whole program in SSA form.
#[derive(Debug, Clone, Default)]
pub struct SsaProgram {
    pub structs: Vec<mir::StructDef>,
    pub functions: Vec<SsaFunction>,
}

impl SsaProgram {
    /// Lower a MIR program into SSA form.
    pub fn from_mir(mir: &mir::Program) -> Result<SsaProgram, String> {
        build_ssa(mir)
    }
}

/// Lower a MIR program into SSA form.
pub fn build_ssa(mir: &mir::Program) -> Result<SsaProgram, String> {
    let mut out = SsaProgram {
        structs: mir.structs.clone(),
        functions: Vec::new(),
    };
    for f in &mir.functions {
        out.functions.push(build_function(f)?);
    }
    Ok(out)
}

fn build_function(f: &mir::Function) -> Result<SsaFunction, String> {
    let mut func = SsaFunction {
        name: f.name.clone(),
        params: f.params.clone(),
        ret: f.ret.clone(),
        locals: f.locals.clone(),
        blocks: Vec::new(),
        values: Vec::new(),
        structs: mir_structs_of(f),
    };

    for block in &f.blocks {
        let mut ssa_block = SsaBlock::default();
        let mut last_def: HashMap<String, usize> = HashMap::new();
        let resolve = |op: &mir::Operand, ld: &HashMap<String, usize>| -> Val {
            match op {
                mir::Operand::Var(n) => {
                    if let Some(&idx) = ld.get(n) {
                        Val::Value(idx)
                    } else {
                        Val::Name(n.clone())
                    }
                }
                mir::Operand::Const(c) => Val::Const(*c),
                mir::Operand::Unit => Val::Unit,
            }
        };
        for stmt in &block.stmts {
            let op = match &stmt.rvalue {
                mir::Rvalue::Use(o) => Op::Copy(resolve(o, &last_def)),
                mir::Rvalue::Binary(b, l, r) => {
                    Op::Binary(*b, resolve(l, &last_def), resolve(r, &last_def))
                }
                mir::Rvalue::Unary(u, o) => Op::Unary(*u, resolve(o, &last_def)),
                mir::Rvalue::Call(name, args) => Op::Call(
                    name.clone(),
                    args.iter().map(|a| resolve(a, &last_def)).collect(),
                ),
                mir::Rvalue::StructLit(name, fields) => Op::StructLit(
                    name.clone(),
                    fields
                        .iter()
                        .map(|(fld, o)| (fld.clone(), resolve(o, &last_def)))
                        .collect(),
                ),
                mir::Rvalue::Field(o, fld, _deref) => {
                    Op::Field(resolve(o, &last_def), fld.clone())
                }
                mir::Rvalue::Ref(o, _) => Op::Ref(resolve(o, &last_def)),
                // `StoreField` and the suspending `Switch` terminator belong to
                // the `async fn` state machine, which is generated only for the
                // Stage-0 C backend. The from-scratch native backend does not
                // support it (it already rejects struct literals), so we reject
                // here with a clear message rather than miscompile.
                mir::Rvalue::StoreField(..) => {
                    return Err(
                        "the native backend does not support `async fn` state machines (use the \
                         C backend, `--emit c`)"
                            .to_string(),
                    )
                }
            };
            let ty = f
                .temp_types
                .get(&stmt.place)
                .cloned()
                .unwrap_or_else(|| "long long".to_string());
            let idx = func.values.len();
            func.values.push(Value {
                op,
                ty,
                name: Some(stmt.place.clone()),
            });
            ssa_block.stmts.push(idx);
            last_def.insert(stmt.place.clone(), idx);
        }

        ssa_block.term = match &block.term {
            mir::Terminator::Goto(t) => SsaTerm::Goto(*t),
            mir::Terminator::If(c, t, e) => SsaTerm::If(resolve(c, &last_def), *t, *e),
            mir::Terminator::Return(Some(o)) => SsaTerm::Return(Some(resolve(o, &last_def))),
            mir::Terminator::Return(None) => SsaTerm::Return(None),
            mir::Terminator::Switch(..) => {
                return Err(
                    "the native backend does not support `async fn` state machines (use the C \
                     backend, `--emit c`)"
                        .to_string(),
                )
            }
            mir::Terminator::Unreachable => SsaTerm::Unreachable,
        };

        func.blocks.push(ssa_block);
    }

    Ok(func)
}

fn mir_structs_of(f: &mir::Function) -> Vec<mir::StructDef> {
    let _ = f;
    Vec::new()
}

/// Collect names referenced by `Val::Name` in a terminator.
fn collect_names_in_term(term: &SsaTerm, visit: &mut impl FnMut(&str)) {
    match term {
        SsaTerm::If(Val::Name(n), _, _) => visit(n),
        SsaTerm::Return(Some(Val::Name(n))) => visit(n),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Optimization passes
// ---------------------------------------------------------------------------

impl SsaProgram {
    /// Run constant folding and dead-code elimination to a fixed point.
    pub fn optimize(&mut self) {
        for _ in 0..8 {
            let mut changed = false;
            for func in &mut self.functions {
                changed |= constant_fold(func);
            }
            for func in &mut self.functions {
                changed |= dce(func);
            }
            if !changed {
                break;
            }
        }
    }
}

/// If an operand is statically known to be a constant, return it.
fn const_of(v: &Val, values: &[Value]) -> Option<i128> {
    match v {
        Val::Const(c) => Some(*c),
        Val::Value(i) => match &values[*i].op {
            Op::Const(c) => Some(*c),
            _ => None,
        },
        _ => None,
    }
}

/// Evaluate a binary operation on two constants, if both are known.
fn eval_binary(b: mir::BinOp, l: i128, r: i128) -> Option<i128> {
    use mir::BinOp::*;
    let logical = |x: i128| x != 0;
    Some(match b {
        Add => l.wrapping_add(r),
        Sub => l.wrapping_sub(r),
        Mul => l.wrapping_mul(r),
        Div => {
            if r == 0 {
                return None;
            } else {
                l.wrapping_div(r)
            }
        }
        Rem => {
            if r == 0 {
                return None;
            } else {
                l.wrapping_rem(r)
            }
        }
        And => (logical(l) && logical(r)) as i128,
        Or => (logical(l) || logical(r)) as i128,
        BitAnd => l & r,
        BitOr => l | r,
        BitXor => l ^ r,
        Shl => l.wrapping_shl(r as u32),
        Shr => l.wrapping_shr(r as u32),
        Eq => (l == r) as i128,
        Ne => (l != r) as i128,
        Lt => (l < r) as i128,
        Gt => (l > r) as i128,
        Le => (l <= r) as i128,
        Ge => (l >= r) as i128,
        Assign => r,
    })
}

/// Fold constant expressions within a single function. Returns true if any
/// value was folded.
pub fn constant_fold(func: &mut SsaFunction) -> bool {
    let mut changed = false;
    let n = func.values.len();
    for i in 0..n {
        let folded = {
            let v = &func.values[i];
            match &v.op {
                Op::Binary(b, l, r) => {
                    let lc = const_of(l, &func.values);
                    let rc = const_of(r, &func.values);
                    match (lc, rc) {
                        (Some(lc), Some(rc)) => eval_binary(*b, lc, rc),
                        _ => None,
                    }
                }
                Op::Unary(mir::UnOp::Neg, o) => const_of(o, &func.values).map(|c| -c),
                Op::Unary(mir::UnOp::Not, o) => {
                    const_of(o, &func.values).map(|c| if c == 0 { 1 } else { 0 })
                }
                Op::Copy(o) => const_of(o, &func.values),
                _ => None,
            }
        };
        if let Some(c) = folded {
            func.values[i].op = Op::Const(c);
            changed = true;
        }
    }
    changed
}

/// Remove values that are never used (dead-code elimination). Returns true if
/// any value was removed.
pub fn dce(func: &mut SsaFunction) -> bool {
    // Names referenced by `Val::Name` operands anywhere in the function.
    let mut used_names: HashSet<String> = HashSet::new();
    let mut seed_live: HashSet<usize> = HashSet::new();

    for v in &func.values {
        collect_names(&v.op, &mut used_names);
    }
    // Names referenced by terminators (e.g. the result of an `if` used by a
    // `return`) must keep their definitions alive too.
    for b in &func.blocks {
        collect_names_in_term(&b.term, &mut |n: &str| {
            used_names.insert(n.to_string());
        });
    }
    // Value names that are read via `Val::Name` elsewhere must keep their defs.
    let value_name: Vec<Option<String>> = func.values.iter().map(|v| v.name.clone()).collect();
    for (i, nm) in value_name.iter().enumerate() {
        if let Some(n) = nm {
            if used_names.contains(n) {
                seed_live.insert(i);
            }
        }
        // Calls have side effects: always keep.
        if matches!(func.values[i].op, Op::Call(..)) {
            seed_live.insert(i);
        }
    }
    // Terminator operands that are value references are live.
    for b in &func.blocks {
        match &b.term {
            SsaTerm::If(Val::Value(i), _, _) => {
                seed_live.insert(*i);
            }
            SsaTerm::Return(Some(Val::Value(i))) => {
                seed_live.insert(*i);
            }
            _ => {}
        }
    }

    // Propagate liveness through value->value references.
    let mut live = seed_live;
    let mut stack: Vec<usize> = live.iter().copied().collect();
    while let Some(i) = stack.pop() {
        let ops = operands_of(&func.values[i].op);
        for o in ops {
            if let Val::Value(j) = o {
                if live.insert(j) {
                    stack.push(j);
                }
            }
        }
    }

    if live.len() == func.values.len() {
        return false;
    }

    // Remap kept values to new indices.
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut new_values: Vec<Value> = Vec::with_capacity(live.len());
    for (i, v) in func.values.iter().enumerate() {
        if live.contains(&i) {
            remap.insert(i, new_values.len());
            new_values.push(v.clone());
        }
    }
    // Rewrite operands in kept values.
    for v in &mut new_values {
        rewrite_operands(&mut v.op, &remap);
    }
    // Rewrite terminators and block stmt lists.
    for b in &mut func.blocks {
        b.stmts.retain(|s| live.contains(s));
        for s in &mut b.stmts {
            *s = remap[s];
        }
        match &mut b.term {
            SsaTerm::If(c, _, _) => remap_val(c, &remap),
            SsaTerm::Return(Some(v)) => remap_val(v, &remap),
            _ => {}
        }
    }
    func.values = new_values;
    true
}

fn collect_names(op: &Op, out: &mut HashSet<String>) {
    match op {
        Op::Copy(v) | Op::Unary(_, v) | Op::Ref(v) | Op::Field(v, _) => {
            if let Val::Name(n) = v {
                out.insert(n.clone());
            }
        }
        Op::Binary(_, l, r) => {
            if let Val::Name(n) = l {
                out.insert(n.clone());
            }
            if let Val::Name(n) = r {
                out.insert(n.clone());
            }
        }
        Op::Call(_, args) => {
            for a in args {
                if let Val::Name(n) = a {
                    out.insert(n.clone());
                }
            }
        }
        Op::StructLit(_, fields) => {
            for (_, a) in fields {
                if let Val::Name(n) = a {
                    out.insert(n.clone());
                }
            }
        }
        Op::Const(_) => {}
    }
}

fn operands_of(op: &Op) -> Vec<Val> {
    match op {
        Op::Copy(v) | Op::Unary(_, v) | Op::Ref(v) | Op::Field(v, _) => vec![v.clone()],
        Op::Binary(_, l, r) => vec![l.clone(), r.clone()],
        Op::Call(_, args) => args.clone(),
        Op::StructLit(_, fields) => fields.iter().map(|(_, v)| v.clone()).collect(),
        Op::Const(_) => vec![],
    }
}

fn rewrite_operands(op: &mut Op, remap: &HashMap<usize, usize>) {
    let fix = |v: &mut Val| {
        if let Val::Value(i) = v {
            if let Some(&n) = remap.get(i) {
                *v = Val::Value(n);
            }
        }
    };
    match op {
        Op::Copy(v) | Op::Unary(_, v) | Op::Ref(v) | Op::Field(v, _) => fix(v),
        Op::Binary(_, l, r) => {
            fix(l);
            fix(r);
        }
        Op::Call(_, args) => args.iter_mut().for_each(fix),
        Op::StructLit(_, fields) => fields.iter_mut().for_each(|(_, v)| fix(v)),
        Op::Const(_) => {}
    }
}

fn remap_val(v: &mut Val, remap: &HashMap<usize, usize>) {
    if let Val::Value(i) = v {
        if let Some(&n) = remap.get(i) {
            *v = Val::Value(n);
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering (for `--emit ssa`)
// ---------------------------------------------------------------------------

impl std::fmt::Display for SsaProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for s in &self.structs {
            writeln!(f, "struct {} {{", s.name)?;
            for (n, t) in &s.fields {
                writeln!(f, "    {}: {}", n, t)?;
            }
            writeln!(f, "}}")?;
        }
        for func in &self.functions {
            let ret = func.ret.clone().unwrap_or_else(|| "()".to_string());
            writeln!(
                f,
                "fn {}({}) -> {} {{",
                func.name,
                func.params.join(", "),
                ret
            )?;
            for (bi, block) in func.blocks.iter().enumerate() {
                writeln!(f, "  b{}:", bi)?;
                for &idx in &block.stmts {
                    let v = &func.values[idx];
                    write!(f, "    t{}: ", idx)?;
                    match &v.op {
                        Op::Const(c) => writeln!(f, "{}", c)?,
                        Op::Copy(o) => writeln!(f, "copy {}", show_val(o))?,
                        Op::Binary(b, l, r) => {
                            writeln!(f, "{:?} {}, {}", b, show_val(l), show_val(r))?
                        }
                        Op::Unary(u, o) => writeln!(f, "{:?} {}", u, show_val(o))?,
                        Op::Call(name, args) => {
                            let a: Vec<String> = args.iter().map(show_val).collect();
                            writeln!(f, "call {} ({})", name, a.join(", "))?
                        }
                        Op::StructLit(name, fields) => {
                            let flds: Vec<String> = fields
                                .iter()
                                .map(|(n, v)| format!("{}: {}", n, show_val(v)))
                                .collect();
                            writeln!(f, "struct {} {{ {} }}", name, flds.join(", "))?
                        }
                        Op::Field(o, fld) => writeln!(f, "{}.{}", show_val(o), fld)?,
                        Op::Ref(o) => writeln!(f, "&{}", show_val(o))?,
                    }
                }
                match &block.term {
                    SsaTerm::Goto(t) => writeln!(f, "    goto b{}", t)?,
                    SsaTerm::If(c, t, e) => {
                        writeln!(f, "    if {} -> b{} else b{}", show_val(c), t, e)?
                    }
                    SsaTerm::Return(Some(v)) => writeln!(f, "    return {}", show_val(v))?,
                    SsaTerm::Return(None) => writeln!(f, "    return")?,
                    SsaTerm::Unreachable => writeln!(f, "    unreachable")?,
                }
            }
            writeln!(f, "}}")?;
        }
        Ok(())
    }
}

fn show_val(v: &Val) -> String {
    match v {
        Val::Value(i) => format!("t{}", i),
        Val::Name(n) => n.clone(),
        Val::Const(c) => c.to_string(),
        Val::Unit => "()".to_string(),
    }
}
