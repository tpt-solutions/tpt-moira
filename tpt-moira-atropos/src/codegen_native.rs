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
    value: Vec<usize>,
    total: usize,
}

/// Compute slot indices for a function. Parameters occupy slots 0..p, locals
/// p..p+l, cross-block temporaries the next slots, and SSA values get fresh
/// slots (unless their name already maps to a slot, e.g. a cross-block temp or a
/// reassigned local).
fn compute_slots(func: &SsaFunction) -> Slots {
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
    let total = named_base + counter;
    Slots {
        named,
        value,
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
        Op::Const(_) => {}
    }
}

struct Emitter<'a> {
    slots: &'a Slots,
    label: usize,
}

impl<'a> Emitter<'a> {
    fn off(&self, slot: usize) -> String {
        format!("[rbp - {}]", (slot + 1) * 8)
    }

    /// Emit code that leaves the value on top of the stack (pushed).
    fn gen_val(&self, v: &Val) -> String {
        match v {
            Val::Const(c) => format!("    mov rax, {}\n    push rax\n", *c as i64),
            Val::Unit => "    xor rax, rax\n    push rax\n".to_string(),
            Val::Name(n) => {
                let slot = self.slots.named[n];
                format!("    push QWORD {}\n", self.off(slot))
            }
            Val::Value(i) => {
                let slot = self.slots.value[*i];
                format!("    push QWORD {}\n", self.off(slot))
            }
            // A C string constant has no register-level representation in the
            // integer subset; emit 0 (the native backend does not yet run
            // print-macro programs).
            Val::CStr(_) => "    xor rax, rax\n    push rax\n".to_string(),
        }
    }

    fn fresh(&mut self) -> usize {
        let l = self.label;
        self.label += 1;
        l
    }

    /// Emit code computing `op`, leaving the result pushed on the stack.
    fn emit_op(&mut self, op: &Op) -> Result<String, String> {
        match op {
            Op::Const(c) => Ok(format!("    mov rax, {}\n    push rax\n", *c as i64)),
            Op::Copy(v) => Ok(self.gen_val(v)),
            Op::Binary(b, l, r) => {
                use crate::mir::BinOp::*;
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
                    crate::mir::UnOp::Neg => s.push_str("    neg rax\n"),
                    crate::mir::UnOp::Not => {
                        s.push_str("    test rax, rax\n    sete al\n    movzx rax, al\n")
                    }
                    crate::mir::UnOp::Deref => s.push_str("    mov rax, [rax]\n"),
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
                if args.len() > 6 {
                    return Err(format!(
                        "native backend supports at most 6 arguments, `{}` needs {}",
                        name,
                        args.len()
                    ));
                }
                let mut s = String::new();
                for a in args {
                    s.push_str(&self.gen_val(a));
                }
                // Pop arguments into registers in reverse parameter order so that
                // arg0 lands in rdi, arg1 in rsi, etc.
                for i in (0..args.len()).rev() {
                    s.push_str(&format!("    pop {}\n", REGS[i]));
                }
                s.push_str("    and rsp, -16\n");
                s.push_str(&format!("    call {}\n", name));
                s.push_str("    push rax\n");
                Ok(s)
            }
            Op::StructLit(..) | Op::Field(..) | Op::Ref(..) => Err(
                "struct literals, field access, and references are handled by the Stage-0 C \
                 backend, not the from-scratch native backend (restricted subset)"
                    .to_string(),
            ),
        }
    }
}

/// Emit a whole program as x86-64 assembly (System V, Intel syntax).
pub fn emit_asm(prog: &SsaProgram) -> Result<String, String> {
    let mut out = String::new();
    out.push_str(".intel_syntax noprefix\n");
    out.push_str(".text\n");
    out.push_str(".globl main\n");

    for func in &prog.functions {
        let slots = compute_slots(func);
        let mut em = Emitter {
            slots: &slots,
            label: 0,
        };
        out.push_str(&format!("\n{}:\n", func.name));
        out.push_str("    push rbp\n    mov rbp, rsp\n");
        out.push_str(&format!("    sub rsp, {}\n", slots.total * 8));

        // Spill incoming parameters into their slots.
        for (i, param) in func.params.iter().enumerate() {
            if i >= REGS.len() {
                break;
            }
            let slot = slots.named[param];
            out.push_str(&format!(
                "    mov {off}, {reg}\n",
                off = em.off(slot),
                reg = REGS[i]
            ));
        }

        for (bi, block) in func.blocks.iter().enumerate() {
            out.push_str(&format!("b{}:\n", bi));
            for &idx in &block.stmts {
                let v = &func.values[idx];
                let asm = em.emit_op(&v.op)?;
                out.push_str(&asm);
                let slot = em.slots.value[idx];
                out.push_str(&format!("    pop {}\n", em.off(slot)));
            }
            match &block.term {
                SsaTerm::Goto(t) => out.push_str(&format!("    jmp b{}\n", t)),
                SsaTerm::If(c, _t, e) => {
                    out.push_str(&em.gen_val(c));
                    out.push_str("    pop rax\n    test rax, rax\n");
                    out.push_str(&format!("    jz b{}\n", e));
                }
                SsaTerm::Return(Some(v)) => {
                    out.push_str(&em.gen_val(v));
                    out.push_str("    pop rax\n");
                    out.push_str("    mov rsp, rbp\n    pop rbp\n    ret\n");
                }
                SsaTerm::Return(None) => {
                    out.push_str("    xor eax, eax\n");
                    out.push_str("    mov rsp, rbp\n    pop rbp\n    ret\n");
                }
                SsaTerm::Unreachable => out.push_str("    ; unreachable\n"),
            }
        }
    }

    // Built-in `print_i32` implemented via libc `printf`, so the native backend
    // is self-contained when linked with the system C compiler.
    out.push_str(
        "\nprint_i32:\n    mov rsi, rdi\n    lea rdi, [rip + .Lfmt]\n    xor eax, eax\n    \
         call printf\n    ret\n.Lfmt:\n    db \"%lld\", 10, 0\n",
    );

    Ok(out)
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
    let asm_path = dir.join("moira_native.s");
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
