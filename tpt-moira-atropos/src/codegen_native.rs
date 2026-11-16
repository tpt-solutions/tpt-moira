//! Stage-1 native backend: emit x86-64 (System V AMD64) assembly from the SSA
//! mid-end, built entirely from scratch (no LLVM, no Cranelift).
//!
//! This is the "native codegen for the restricted subset" deliverable. It targets
//! a *restricted* subset of the SSA graph — integer/boolean arithmetic,
//! comparison, control flow (`if`/`while`/`for`/`loop`/`return`), and function
//! calls (including the `print_i32` helper) — using a simple stack-machine
//! evaluation model. A stack slot is reserved for every named slot (parameters,
//! locals, and cross-block temporaries such as the result of an `if`/`else`)
//! and for every SSA value, so the single-assignment graph maps cleanly onto
//! memory without a register allocator or φ-lowering pass.
//!
//! Composite types (struct literals/fields/references) are handled by the
//! Stage-0 C backend instead and intentionally rejected here with a clear error,
//! keeping this backend small and fast.

use crate::ssa::{Op, SsaFunction, SsaProgram, SsaTerm, Val};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Argument registers under the System V AMD64 calling convention.
const REGS: [&str; 6] = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];

/// Slot allocation for one function.
struct Slots {
    named: HashMap<String, usize>,
    /// The slot that holds the caller's destination block, for a function that returns a struct.
    dest: Option<usize>,
    value: Vec<usize>,
    /// For a value that holds a struct (a literal, or a copy of one): the first slot of its block
    /// and the number of fields. A struct value is a pointer to its block.
    blocks: HashMap<usize, (usize, usize)>,
    /// A struct parameter's own block (name, first slot, fields): the callee copies the argument
    /// into it on entry, so that its changes do not reach the caller's struct.
    param_blocks: Vec<(String, usize, usize)>,
    total: usize,
}

/// Compute slot indices for a function. Parameters occupy slots 0..p, locals
/// p..p+l, cross-block temporaries the next slots, and SSA values get fresh
/// slots (unless their name already maps to a slot, e.g. a cross-block temp or a
/// reassigned local).
fn compute_slots(
    func: &SsaFunction,
    structs: &HashMap<String, Vec<String>>,
    value_ty: &[String],
) -> Slots {
    let mut named: HashMap<String, usize> = HashMap::new();
    // `lower` records parameters in `locals` as well as `params`, so assigning
    // slots from `locals` (in order) covers both without double-counting.
    let l = func.locals.len();
    for (j, (local, _)) in func.locals.iter().enumerate() {
        named.insert(local.clone(), j);
    }
    // Cross-block names (referenced via `Val::Name`) in value operands ...
    let mut next = l;
    for v in &func.values {
        collect_names_in_op(&v.op, &mut |n: &str| {
            if !named.contains_key(n) {
                named.insert(n.to_string(), next);
                next += 1;
            }
        });
    }
    // ... and in terminators (e.g. the result of an `if` referenced by a return).
    for b in &func.blocks {
        if let crate::ssa::SsaTerm::If(crate::ssa::Val::Name(n), _, _) = &b.term {
            if !named.contains_key(n) {
                named.insert(n.clone(), next);
                next += 1;
            }
        }
        if let crate::ssa::SsaTerm::Return(Some(crate::ssa::Val::Name(n))) = &b.term {
            if !named.contains_key(n) {
                named.insert(n.clone(), next);
                next += 1;
            }
        }
    }
    // Temp names defined in more than one block are cross-block merge values
    // (e.g. the result of an `if`/`else`); they must share a single slot so both
    // predecessor branches write it and the merge reads it.
    {
        let mut name_blocks: HashMap<String, std::collections::HashSet<usize>> = HashMap::new();
        for (bi, block) in func.blocks.iter().enumerate() {
            for &idx in &block.stmts {
                if let Some(n) = &func.values[idx].name {
                    name_blocks.entry(n.clone()).or_default().insert(bi);
                }
            }
        }
        // Assign a single shared slot to every cross-block merge name. Iterate
        // in *sorted* order so the slot numbering is deterministic regardless of
        // the `HashMap`'s per-process iteration order (which would otherwise
        // break byte-identical output between stages).
        let mut merge_names: Vec<String> = name_blocks
            .into_iter()
            .filter(|(n, blocks)| blocks.len() >= 2 && !named.contains_key(n))
            .map(|(n, _)| n)
            .collect();
        merge_names.sort();
        for n in merge_names {
            named.insert(n, next);
            next += 1;
        }
    }
    // Value slots.
    let named_base = next;
    let mut value = Vec::with_capacity(func.values.len());
    let mut counter = 0usize;
    for v in &func.values {
        if let Some(n) = &v.name {
            if let Some(&slot) = named.get(n) {
                value.push(slot);
                continue;
            }
        }
        value.push(named_base + counter);
        counter += 1;
    }
    // Struct blocks: a struct value owns one slot per field, contiguous in the frame.
    let mut blocks: HashMap<usize, (usize, usize)> = HashMap::new();
    let mut extra = named_base + counter;
    // A struct-returning function receives its destination block as an extra leading argument,
    // kept in its own slot.
    let dest = match &func.ret {
        Some(r) if structs.contains_key(r) => {
            extra += 1;
            Some(extra - 1)
        }
        _ => None,
    };
    for (vi, v) in func.values.iter().enumerate() {
        // A value that is a struct itself (not a pointer to one) owns a block.
        let own_struct = value_ty
            .get(vi)
            .filter(|ty| !ty.ends_with('*') && structs.contains_key(ty.as_str()))
            .cloned();
        let name = match &v.op {
            Op::StructLit(name, _) => Some(name.clone()),
            Op::Copy(_) | Op::Call(..) => own_struct,
            _ => None,
        };
        if let Some(name) = name {
            let k = structs.get(&name).map(|f| f.len()).unwrap_or(0);
            blocks.insert(vi, (extra, k));
            extra += k;
        }
    }
    let mut param_blocks: Vec<(String, usize, usize)> = Vec::new();
    for p in &func.params {
        let ty = func
            .locals
            .iter()
            .find(|(n, _)| n == p)
            .and_then(|(_, t)| t.clone());
        if let Some(ty) = ty {
            if !ty.ends_with('*') && structs.contains_key(&ty) {
                let k = structs[&ty].len();
                param_blocks.push((p.clone(), extra, k));
                extra += k;
            }
        }
    }
    let total = extra;
    Slots {
        named,
        dest,
        value,
        blocks,
        param_blocks,
        total,
    }
}

fn collect_names_in_op(op: &Op, visit: &mut impl FnMut(&str)) {
    match op {
        Op::Copy(v) | Op::Unary(_, v) | Op::Ref(v) | Op::Field(v, _) => {
            if let Val::Name(n) = v {
                visit(n);
            }
        }
        Op::Binary(_, l, r) => {
            if let Val::Name(n) = l {
                visit(n);
            }
            if let Val::Name(n) = r {
                visit(n);
            }
        }
        Op::Call(c, args) => {
            if let Val::Name(n) = c {
                visit(n);
            }
            for a in args {
                if let Val::Name(n) = a {
                    visit(n);
                }
            }
        }
        Op::StructLit(_, fields) => {
            for (_, a) in fields {
                if let Val::Name(n) = a {
                    visit(n);
                }
            }
        }
        Op::Store(obj, _, v) => {
            for x in [obj, v] {
                if let Val::Name(n) = x {
                    visit(n);
                }
            }
        }
        Op::Const(_) => {}
    }
}

struct Emitter<'a> {
    slots: &'a Slots,
    label: usize,
    /// Field names of each struct, in declaration order (a field's index is its offset).
    structs: &'a HashMap<String, Vec<String>>,
    /// The functions that return a struct (they take the destination block first).
    struct_fns: &'a std::collections::HashSet<String>,
    /// The type of each SSA value, and of each named local.
    value_ty: &'a [String],
    local_ty: &'a HashMap<String, String>,
}

impl<'a> Emitter<'a> {
    fn off(&self, slot: usize) -> String {
        format!("[rbp - {}]", (slot + 1) * 8)
    }

    /// Emit code that leaves the value on top of the stack (pushed).
    fn gen_val(&self, v: &Val) -> String {
        match v {
            Val::Const(c) => format!("    mov rax, {}\n    push rax\n", *c as i64),
            // The native backend has no floating-point support yet. A float operand traps
            // (`ud2`) rather than computing with a truncated integer.
            Val::Float(_) => "    ud2\n".to_string(),
            Val::Unit => "    xor rax, rax\n    push rax\n".to_string(),
            Val::Name(n) => {
                let slot = self.slots.named[n];
                format!("    push QWORD PTR {}\n", self.off(slot))
            }
            Val::Value(i) => {
                let slot = self.slots.value[*i];
                format!("    push QWORD PTR {}\n", self.off(slot))
            }
            // A C string constant has no register-level representation in the
            // integer subset; emit 0 (the native backend does not yet run
            // print-macro programs).
            Val::CStr(text) => format!("    lea rax, [rip + {}]\n    push rax\n", str_label(text)),
        }
    }

    /// The struct a value holds (its type without a pointer marker), if it is one.
    fn struct_of(&self, v: &Val) -> Option<String> {
        let ty = match v {
            Val::Value(i) => self.value_ty.get(*i)?.clone(),
            Val::Name(n) => self.local_ty.get(n)?.clone(),
            _ => return None,
        };
        let base = ty.trim_end_matches('*').to_string();
        if self.structs.contains_key(&base) {
            Some(base)
        } else {
            None
        }
    }

    /// The index of `field` in struct `name` (its offset in words).
    fn field_index(&self, name: &str, field: &str) -> Result<usize, String> {
        self.structs
            .get(name)
            .and_then(|fs| fs.iter().position(|f| f == field))
            .ok_or_else(|| format!("no field `{field}` in struct `{name}`"))
    }

    /// The operand of word `i` of the struct block that starts at slot `base` with `k` words. The
    /// block's lowest address is the first word; word `i` is `i` words above it.
    fn block_word(base: usize, k: usize, i: usize) -> String {
        format!("[rbp - {}]", (base + k - i) * 8)
    }

    fn fresh(&mut self) -> usize {
        let l = self.label;
        self.label += 1;
        l
    }

    /// Emit code computing `op`, leaving the result pushed on the stack.
    fn emit_op(&mut self, idx: usize, op: &Op) -> Result<String, String> {
        match op {
            Op::Const(c) => Ok(format!("    mov rax, {}\n    push rax\n", *c as i64)),
            Op::Copy(v) => {
                // Copying a struct copies its words into a block of its own.
                let Some(&(base, k)) = self.slots.blocks.get(&idx) else {
                    return Ok(self.gen_val(v));
                };
                let mut s = self.gen_val(v);
                s.push_str("    pop rax\n");
                for i in 0..k {
                    s.push_str(&format!("    mov rcx, QWORD PTR [rax + {}]\n", i * 8));
                    s.push_str(&format!("    mov QWORD PTR {}, rcx\n", Emitter::block_word(base, k, i)));
                }
                s.push_str(&format!("    lea rax, [rbp - {}]\n    push rax\n", (base + k) * 8));
                Ok(s)
            }
            Op::StructLit(name, fields) => {
                let Some(&(base, k)) = self.slots.blocks.get(&idx) else {
                    return Err(format!("struct literal `{name}` has no block"));
                };
                let mut s = String::new();
                for (fname, v) in fields {
                    let i = self.field_index(name, fname)?;
                    s.push_str(&self.gen_val(v));
                    s.push_str("    pop rcx\n");
                    s.push_str(&format!("    mov QWORD PTR {}, rcx\n", Emitter::block_word(base, k, i)));
                }
                s.push_str(&format!("    lea rax, [rbp - {}]\n    push rax\n", (base + k) * 8));
                Ok(s)
            }
            Op::Field(obj, fname) => {
                let name = self
                    .struct_of(obj)
                    .ok_or_else(|| format!("field `{fname}` of a value that is not a struct"))?;
                let i = self.field_index(&name, fname)?;
                let mut s = self.gen_val(obj);
                s.push_str(&format!("    pop rax\n    mov rax, QWORD PTR [rax + {}]\n    push rax\n", i * 8));
                Ok(s)
            }
            Op::Store(obj, fname, v) => {
                let name = self
                    .struct_of(obj)
                    .ok_or_else(|| format!("assignment to `{fname}` of a value that is not a struct"))?;
                let i = self.field_index(&name, fname)?;
                let mut s = self.gen_val(obj);
                s.push_str(&self.gen_val(v));
                s.push_str(&format!(
                    "    pop rcx\n    pop rax\n    mov QWORD PTR [rax + {}], rcx\n    xor eax, eax\n    push rax\n",
                    i * 8
                ));
                Ok(s)
            }
            Op::Binary(b, l, r) => {
                use crate::mir::MirBinOp::*;
                let mut s = String::new();
                s.push_str(&self.gen_val(l));
                s.push_str(&self.gen_val(r));
                s.push_str("    pop rsi\n    pop rax\n");
                match b {
                    Add => s.push_str("    add rax, rsi\n"),
                    Sub => s.push_str("    sub rax, rsi\n"),
                    Mul => s.push_str("    imul rax, rsi\n"),
                    Div => s.push_str("    cqo\n    idiv rsi\n"),
                    Rem => s.push_str("    cqo\n    idiv rsi\n    mov rax, rdx\n"),
                    BitAnd => s.push_str("    and rax, rsi\n"),
                    BitOr => s.push_str("    or rax, rsi\n"),
                    BitXor => s.push_str("    xor rax, rsi\n"),
                    Shl => s.push_str("    mov rcx, rsi\n    shl rax, cl\n"),
                    Shr => s.push_str("    mov rcx, rsi\n    sar rax, cl\n"),
                    Eq => s.push_str("    cmp rax, rsi\n    sete al\n    movzx rax, al\n"),
                    Ne => s.push_str("    cmp rax, rsi\n    setne al\n    movzx rax, al\n"),
                    Lt => s.push_str("    cmp rax, rsi\n    setl al\n    movzx rax, al\n"),
                    Gt => s.push_str("    cmp rax, rsi\n    setg al\n    movzx rax, al\n"),
                    Le => s.push_str("    cmp rax, rsi\n    setle al\n    movzx rax, al\n"),
                    Ge => s.push_str("    cmp rax, rsi\n    setge al\n    movzx rax, al\n"),
                    And => {
                        // Logical and: (l != 0) && (r != 0)
                        let f = self.fresh();
                        let e = self.fresh();
                        s.push_str(&format!(
                            "    test rax, rax\n    jz .Lfalse{}\n    test rsi, rsi\n    jz .Lfalse{}\n    mov rax, 1\n    jmp .Lend{}\n.Lfalse{}:\n    xor rax, rax\n.Lend{}:\n",
                            f, f, e, f, e
                        ));
                    }
                    Or => {
                        let t = self.fresh();
                        let e = self.fresh();
                        s.push_str(&format!(
                            "    test rax, rax\n    jnz .Ltrue{}\n    test rsi, rsi\n    jnz .Ltrue{}\n    xor rax, rax\n    jmp .Lend{}\n.Ltrue{}:\n    mov rax, 1\n.Lend{}:\n",
                            t, t, e, t, e
                        ));
                    }
                    Assign => {
                        // Should not appear in SSA values; treat as copy of r.
                        s.push_str("    mov rax, rsi\n");
                    }
                }
                s.push_str("    push rax\n");
                Ok(s)
            }
            Op::Unary(u, v) => {
                let mut s = self.gen_val(v);
                s.push_str("    pop rax\n");
                match u {
                    crate::mir::MirUnOp::Neg => s.push_str("    neg rax\n"),
                    crate::mir::MirUnOp::Not => {
                        s.push_str("    test rax, rax\n    sete al\n    movzx rax, al\n")
                    }
                    crate::mir::MirUnOp::Deref => s.push_str("    mov rax, [rax]\n"),
                }
                s.push_str("    push rax\n");
                Ok(s)
            }
            Op::Call(callee, args) => {
                let name = match callee {
                    Val::Name(n) => n.clone(),
                    other => {
                        return Err(format!(
                        "native backend only supports calls through a named function, found {:?}",
                        other
                    ))
                    }
                };
                let returns_struct = self.struct_fns.contains(&name);
                let n = args.len() + usize::from(returns_struct);
                if n > 6 {
                    return Err(format!(
                        "native backend supports at most 6 arguments, `{}` needs {}",
                        name, n
                    ));
                }
                let mut s = String::new();
                if returns_struct {
                    let &(base, k) = self
                        .slots
                        .blocks
                        .get(&idx)
                        .ok_or_else(|| format!("call of `{name}` has no result block"))?;
                    s.push_str(&format!("    lea rax, [rbp - {}]\n    push rax\n", (base + k) * 8));
                }
                for a in args {
                    s.push_str(&self.gen_val(a));
                }
                // Pop arguments into registers in parameter order so that the first one lands
                // in rdi, the next in rsi, and so on.
                for i in (0..n).rev() {
                    s.push_str(&format!("    pop {}\n", REGS[i]));
                }
                // Align the stack for the call without depending on how many values are pending
                // (the operands of an enclosing expression sit on the stack): save rsp in the
                // aligned frame, call, then restore rsp exactly. The callee sees rsp = 8 mod 16,
                // as the ABI requires.
                s.push_str("    mov rax, rsp\n    and rsp, -16\n    sub rsp, 8\n    push rax\n");
                s.push_str(&format!("    call {}\n", name));
                s.push_str("    pop rsp\n");
                s.push_str("    push rax\n");
                Ok(s)
            }
            // A reference to a struct is the struct's pointer.
            Op::Ref(v) if self.struct_of(v).is_some() => Ok(self.gen_val(v)),
            Op::Ref(..) => Err(
                "references are handled by the Stage-0 C backend, not the from-scratch native \
                 backend (restricted subset)"
                    .to_string(),
            ),
        }
    }
}

/// Emit one function's x86-64 assembly (System V, Intel syntax).
fn emit_function_asm(
    func: &SsaFunction,
    structs: &HashMap<String, Vec<String>>,
    struct_fns: &std::collections::HashSet<String>,
) -> Result<String, String> {
    let mut out = String::new();
    let local_ty: HashMap<String, String> = func
        .locals
        .iter()
        .filter_map(|(n, t)| t.clone().map(|t| (n.clone(), t)))
        .collect();
    // A value's type is its recorded type; a copy of a value or a local that is a struct (or
    // pointer to one) takes that type when its own recorded type is not one.
    let mut value_ty: Vec<String> = Vec::with_capacity(func.values.len());
    for v in &func.values {
        let from_src = match &v.op {
            Op::Copy(Val::Name(n)) => local_ty.get(n).cloned(),
            Op::Copy(Val::Value(j)) => value_ty.get(*j).cloned(),
            _ => None,
        };
        let is_struct = |t: &str| structs.contains_key(t.trim_end_matches('*'));
        let ty = match from_src {
            Some(src) if is_struct(&src) && !is_struct(&v.ty) => src,
            _ => v.ty.clone(),
        };
        value_ty.push(ty);
    }
    let slots = compute_slots(func, structs, &value_ty);
    let mut em = Emitter {
        slots: &slots,
        label: 0,
        structs,
        struct_fns,
        value_ty: &value_ty,
        local_ty: &local_ty,
    };
    out.push_str(&format!("\n{}:\n", fn_label(&func.name)));
    out.push_str("    push rbp\n    mov rbp, rsp\n");
    out.push_str(&format!("    sub rsp, {}\n", slots.total * 8));

    // Spill incoming parameters into their slots. A struct-returning function's first register
    // is its destination block.
    let shift = usize::from(slots.dest.is_some());
    if let Some(d) = slots.dest {
        out.push_str(&format!("    mov {}, rdi\n", em.off(d)));
    }
    for (i, param) in func.params.iter().enumerate() {
        if i + shift >= REGS.len() {
            break;
        }
        let slot = slots.named[param];
        out.push_str(&format!(
            "    mov {off}, {reg}\n",
            off = em.off(slot),
            reg = REGS[i + shift]
        ));
    }

    // A struct parameter is copied into the callee's own block; the parameter then points there.
    for (p, base, k) in &slots.param_blocks {
        let off = em.off(slots.named[p]);
        out.push_str(&format!("    mov rax, QWORD PTR {off}
"));
        for i in 0..*k {
            out.push_str(&format!("    mov rcx, QWORD PTR [rax + {}]
", i * 8));
            out.push_str(&format!("    mov QWORD PTR {}, rcx
", Emitter::block_word(*base, *k, i)));
        }
        out.push_str(&format!("    lea rax, [rbp - {}]
    mov QWORD PTR {off}, rax
", (base + k) * 8));
    }
    for (bi, block) in func.blocks.iter().enumerate() {
        out.push_str(&format!("{}_b{}:\n", func.name, bi));
        for &idx in &block.stmts {
            let v = &func.values[idx];
            let asm = em.emit_op(idx, &v.op)?;
            out.push_str(&asm);
            let slot = em.slots.value[idx];
            out.push_str(&format!("    pop {}\n", em.off(slot)));
        }
        match &block.term {
            SsaTerm::Goto(t) => out.push_str(&format!("    jmp {}_b{}\n", func.name, t)),
            SsaTerm::If(c, t, e) => {
                out.push_str(&em.gen_val(c));
                out.push_str("    pop rax\n    test rax, rax\n");
                out.push_str(&format!("    jz {}_b{}\n", func.name, e));
                // Fall-through is the next block in layout order, not necessarily the then block.
                out.push_str(&format!("    jmp {}_b{}\n", func.name, t));
            }
            SsaTerm::Return(Some(v)) => {
                out.push_str(&em.gen_val(v));
                out.push_str("    pop rax\n");
                if let (Some(d), Some(r)) = (slots.dest, &func.ret) {
                    // Copy the struct's words into the caller's block; return that pointer.
                    let k = structs.get(r).map(|f| f.len()).unwrap_or(0);
                    out.push_str(&format!("    mov rdx, QWORD PTR {}\n", em.off(d)));
                    for i in 0..k {
                        out.push_str(&format!("    mov rcx, QWORD PTR [rax + {}]\n", i * 8));
                        out.push_str(&format!("    mov QWORD PTR [rdx + {}], rcx\n", i * 8));
                    }
                    out.push_str("    mov rax, rdx\n");
                }
                out.push_str("    mov rsp, rbp\n    pop rbp\n    ret\n");
            }
            SsaTerm::Return(None) => {
                out.push_str("    xor eax, eax\n");
                out.push_str("    mov rsp, rbp\n    pop rbp\n    ret\n");
            }
            SsaTerm::Unreachable => out.push_str("    ; unreachable\n"),
        }
    }
    Ok(out)
}

/// Emit a whole program as x86-64 assembly (System V, Intel syntax).
///
/// Each function is emitted on its own thread and the results are joined in the
/// original function order, so the output (and the first error reported) is
/// deterministic regardless of scheduling.
/// The data label of a string constant: a hash of its bytes, so every use of the same text shares
/// one label, and the labels are the same for every function.
fn str_label(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!(".Lstr{h:016x}")
}

/// The text of a string constant as an assembler string literal.
fn asm_string(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{:03o}", b)),
        }
    }
    out
}

/// Every string constant a function uses, in the operations and the terminators.
fn collect_strings(f: &SsaFunction, out: &mut std::collections::BTreeSet<String>) {
    fn val(v: &Val, out: &mut std::collections::BTreeSet<String>) {
        if let Val::CStr(t) = v {
            out.insert(t.clone());
        }
    }
    for value in &f.values {
        match &value.op {
            Op::Const(_) => {}
            Op::Store(o, _, v) => {
                val(o, out);
                val(v, out);
            }
            Op::Copy(v) | Op::Unary(_, v) | Op::Field(v, _) | Op::Ref(v) => val(v, out),
            Op::Binary(_, l, r) => {
                val(l, out);
                val(r, out);
            }
            Op::Call(c, args) => {
                val(c, out);
                for a in args {
                    val(a, out);
                }
            }
            Op::StructLit(_, fields) => {
                for (_, v) in fields {
                    val(v, out);
                }
            }
        }
    }
    for block in &f.blocks {
        match &block.term {
            SsaTerm::If(c, _, _) => val(c, out),
            SsaTerm::Return(Some(v)) => val(v, out),
            _ => {}
        }
    }
}

pub fn emit_asm(prog: &SsaProgram) -> Result<String, String> {
    let structs: HashMap<String, Vec<String>> = prog
        .structs
        .iter()
        .map(|s| (s.name.clone(), s.fields.iter().map(|(n, _)| n.clone()).collect()))
        .collect();
    let structs = &structs;
    let struct_fns: std::collections::HashSet<String> = prog
        .functions
        .iter()
        .filter(|f| f.ret.as_ref().is_some_and(|r| structs.contains_key(r)))
        .map(|f| f.name.clone())
        .collect();
    let struct_fns = &struct_fns;
    let bodies: Vec<Result<String, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = prog
            .functions
            .iter()
            .map(|f| s.spawn(move || emit_function_asm(f, structs, struct_fns)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut out = String::new();
    out.push_str(
        ".intel_syntax noprefix
",
    );
    out.push_str(
        ".text
",
    );
    out.push_str(
        ".globl main
",
    );
    for body in bodies {
        out.push_str(&body?);
    }

    // The string constants of the program, each once, as NUL-terminated data.
    let mut strings: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for f in &prog.functions {
        collect_strings(f, &mut strings);
    }
    out.push_str("\n.data\n");
    for text in &strings {
        out.push_str(&format!("{}:\n    .asciz \"{}\"\n", str_label(text), asm_string(text)));
    }
    out.push_str(".text\n");

    // The runtime helpers are called with the stack aligned as the ABI requires, and each sets up
    // its own frame so that its `printf` call is aligned too.
    if cfg!(windows) {
        // The C runtime calls `main` and expects rsi and rdi preserved (callee-saved on Windows);
        // the generated code uses them for arguments, so the wrapper saves them around `__moira_main`.
        out.push_str("\nmain:\n    push rbp\n    mov rbp, rsp\n    push rsi\n    push rdi\n    call __moira_main\n    pop rdi\n    pop rsi\n    pop rbp\n    ret\n");
    }
    out.push_str(&print_helper("print_i32", "%lld\\n", ".Lfmt"));
    out.push_str(&print_helper("print_i64", "%lld\\n", ".Lfmt_i64"));
    // `print!` writers: no newline, so the caller ends the line with its own "\n" piece.
    out.push_str(&print_helper("__moira_w_i64", "%lld", ".Lfmt_w_i64"));
    out.push_str(&print_helper("__moira_w_str", "%s", ".Lfmt_w_str"));
    Ok(out)
}

/// The assembly label of a Moira function. On Windows the program's `main` is reached through a
/// wrapper (see the runtime), so the function itself gets a distinct label.
fn fn_label(name: &str) -> String {
    if cfg!(windows) && name == "main" {
        "__moira_main".to_string()
    } else {
        name.to_string()
    }
}

/// A runtime helper that passes its one argument to libc `printf` with format `fmt`.
fn print_helper(name: &str, fmt: &str, label: &str) -> String {
    if cfg!(windows) {
        // Windows x64: printf takes its format in rcx and the value in rdx, and the caller
        // reserves 32 bytes of shadow space. The frame keeps rsp 16-byte aligned for the call.
        return format!(
            "\n{name}:\n    push rbp\n    mov rbp, rsp\n    sub rsp, 32\n    mov rdx, rdi\n    \
             lea rcx, [rip + {label}]\n    xor eax, eax\n    call printf\n    add rsp, 32\n    \
             pop rbp\n    ret\n{label}:\n    .asciz \"{fmt}\"\n"
        );
    }
    format!(
        "\n{name}:\n    push rbp\n    mov rbp, rsp\n    mov rsi, rdi\n    lea rdi, [rip + {label}]\n    \
         xor eax, eax\n    call printf\n    pop rbp\n    ret\n{label}:\n    .asciz \"{fmt}\"\n"
    )
}

/// Locate a C compiler/assembler toolchain.
fn find_cc() -> Option<String> {
    for cc in ["gcc", "clang", "cc"] {
        if Command::new(cc).arg("--version").output().is_ok() {
            return Some(cc.to_string());
        }
    }
    None
}

/// Emit assembly and assemble + link it into a native executable at `out_path`.
/// Returns an error (a) if the program uses constructs outside the restricted
/// native subset, or (b) if no assembler/linker is available on this machine.
pub fn compile_to_native(prog: &SsaProgram, out_path: &Path) -> Result<(), String> {
    let asm = emit_asm(prog)?;
    let cc = find_cc().ok_or_else(|| {
        "no C compiler/assembler (gcc/clang/cc) found; cannot produce a native binary".to_string()
    })?;

    let dir = out_path.parent().unwrap_or_else(|| Path::new("."));
    // The assembly is named after the output, so builds that run at the same time (in one
    // directory) do not overwrite each other's intermediate file.
    let stem = out_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "moira".to_string());
    let asm_path = dir.join(format!("{stem}.moira_native.s"));
    std::fs::write(&asm_path, asm).map_err(|e| format!("write asm: {e}"))?;

    let status = Command::new(&cc)
        .arg(&asm_path)
        .arg("-o")
        .arg(out_path)
        .status()
        .map_err(|e| format!("invoke {}: {}", cc, e))?;
    if !status.success() {
        return Err(format!("{} failed to assemble/link the native output", cc));
    }
    Ok(())
}
