//! The Lachesis borrow checker (Phase-2 memory-safety analysis).
//!
//! The checker runs over the Atropos MIR, which is the architecturally correct
//! place for Non-Lexical-Lifetime analysis (as in `rustc`). It implements two
//! layers:
//!
//! * **Fast path** — functions with no references at all are trivially safe and
//!   skip the analysis.
//! * **NLL-style conflict detection** — for functions that do borrow, it builds
//!   the CFG from the terminators, propagates *aliases* (a variable copied from
//!   a borrow temp keeps that borrow alive), and reports a conflict whenever a
//!   place is *mutated* (assigned) at a point where a reference to it is still
//!   live (a use of the borrow or an alias appears after the mutation on a path
//!   reachable from the borrow), or when a reference to a local escapes through
//!   a `return`.
//!
//! **Mutability is now threaded through the AST and MIR** (the parser records
//! `&` vs `&mut`, the MIR `Rvalue::Ref` carries a mutability flag, and the type
//! inference produces `Ty::RefMut` for `&mut`). The borrow checker therefore
//! enforces both:
//!
//! * The strict but *sound* rule "a borrowed place may not be mutated while
//!   borrowed" (correct for `&mut` and for `&`).
//! * That a `&mut` may not coexist with any other live borrow of the same place
//!   — neither a second `&mut` nor a shared `&` — forbidding aliased mutable
//!   borrows that Rust rejects.

use atropos::mir::{Operand, Rvalue, Terminator};
use clotho::error::Diag;
use std::collections::{HashMap, HashSet};

/// Check every function in `prog` for borrow violations.
pub fn borrow_check(prog: &atropos::mir::Program) -> Result<(), Vec<Diag>> {
    let mut errors = Vec::new();
    for f in &prog.functions {
        check_function(f, &mut errors);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[derive(Clone)]
struct Borrow {
    place: String,
    temp: String,
    block: usize,
    stmt: usize,
    /// `true` for `&mut`, `false` for `&`.
    mutable: bool,
}

/// Precomputed liveness information for a single borrow.
struct BorrowInfo {
    borrow: Borrow,
    /// Whether every reachable statement after creation is live (the borrow is
    /// never read but still occupies the place until the end of scope).
    never_read: bool,
    use_locs: Vec<(usize, usize)>,
    escapes: bool,
    reach: HashSet<usize>,
}

/// Whether `borrow` is live at the statement/terminator position `(bi, si)`
/// (`si == block.stmts.len()` denotes the terminator).
fn is_live_at(info: &BorrowInfo, bi: usize, si: usize) -> bool {
    if !info.reach.contains(&bi) {
        return false;
    }
    if !loc_le((info.borrow.block, info.borrow.stmt), (bi, si)) {
        return false;
    }
    if info.escapes || info.never_read {
        // A borrow that escapes, or is never read, occupies its place for the
        // whole reachable remainder of the function.
        return true;
    }
    info.use_locs.iter().any(|&u| loc_le((bi, si), u))
}

fn check_function(f: &atropos::mir::Function, errors: &mut Vec<Diag>) {
    // Fast path: no references => nothing to check.
    let mut has_borrow = false;
    for b in &f.blocks {
        for stmt in &b.stmts {
            if let Rvalue::Ref(Operand::Var(_), _) = &stmt.rvalue {
                has_borrow = true;
            }
        }
    }
    if !has_borrow {
        return;
    }

    let n = f.blocks.len();
    if n == 0 {
        return;
    }
    let succs = (0..n)
        .map(|bi| successors(&f.blocks[bi].term))
        .collect::<Vec<_>>();

    // Alias map: `p = q` (a plain copy) makes `p` an alias of whatever `q`
    // aliases, transitively. This is what lets a reference survive being bound
    // to a variable (`let r = &x;` then `r` carries the borrow of `x`).
    let mut aliases: HashMap<String, HashSet<String>> = HashMap::new();
    for b in &f.blocks {
        for s in &b.stmts {
            if let Rvalue::Use(Operand::Var(q)) = &s.rvalue {
                if s.place != *q {
                    aliases
                        .entry(s.place.clone())
                        .or_default()
                        .insert(q.clone());
                }
            }
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        let keys: Vec<String> = aliases.keys().cloned().collect();
        for v in keys {
            let srcs: HashSet<String> = aliases.get(&v).cloned().unwrap_or_default();
            for s in srcs {
                if let Some(more) = aliases.get(&s).cloned() {
                    for m in more {
                        if aliases.get_mut(&v).unwrap().insert(m) {
                            changed = true;
                        }
                    }
                }
            }
        }
    }

    // Collect borrows: `temp = &place` (or `temp = &mut place`).
    let mut borrows = Vec::new();
    for (bi, b) in f.blocks.iter().enumerate() {
        for (si, s) in b.stmts.iter().enumerate() {
            if let Rvalue::Ref(Operand::Var(x), m) = &s.rvalue {
                borrows.push(Borrow {
                    place: x.clone(),
                    temp: s.place.clone(),
                    block: bi,
                    stmt: si,
                    mutable: *m,
                });
            }
        }
    }

    // Precompute liveness info per borrow.
    let mut infos = Vec::new();
    for br in &borrows {
        // The set of names that keep this borrow alive: the temp itself plus
        // every variable that aliases it (transitively).
        let mut live_names: HashSet<String> = HashSet::new();
        live_names.insert(br.temp.clone());
        for (v, srcs) in &aliases {
            if srcs.contains(&br.temp) {
                live_names.insert(v.clone());
            }
        }

        // Locations where the borrow (or an alias) is used, plus whether it
        // escapes through a return. A return use is "after everything".
        let mut use_locs: Vec<(usize, usize)> = Vec::new();
        let mut escapes = false;
        for (bi, b) in f.blocks.iter().enumerate() {
            for (si, s) in b.stmts.iter().enumerate() {
                for name in operands_of(&s.rvalue) {
                    if live_names.contains(&name) {
                        use_locs.push((bi, si));
                    }
                }
            }
            match &b.term {
                Terminator::If(Operand::Var(v), _, _) => {
                    if live_names.contains(v) {
                        use_locs.push((bi, b.stmts.len()));
                    }
                }
                Terminator::Return(Some(Operand::Var(v))) if live_names.contains(v) => {
                    escapes = true;
                }
                _ => {}
            }
        }

        let reach = reachable_from(br.block, &succs);
        infos.push(BorrowInfo {
            borrow: br.clone(),
            never_read: use_locs.is_empty(),
            use_locs,
            escapes,
            reach,
        });
    }

    // (1) Mutation-while-borrowed: a borrowed place may not be mutated while the
    // borrow (or an alias) is still live. Sound for both `&` and `&mut`.
    for info in &infos {
        let br = &info.borrow;
        let creation = (br.block, br.stmt);
        for (mb, b) in f.blocks.iter().enumerate() {
            for (si, s) in b.stmts.iter().enumerate() {
                if s.place != br.place {
                    continue;
                }
                let m = (mb, si);
                if !info.reach.contains(&mb) || !loc_le(creation, m) {
                    continue;
                }
                if is_live_at(info, mb, si) {
                    errors.push(Diag::error(
                        format!(
                            "in `{}`: cannot assign to `{}` while it is borrowed",
                            f.name, br.place
                        ),
                        None,
                    ));
                }
            }
        }

        if info.escapes && !f.params.contains(&br.place) {
            errors.push(Diag::error(
                format!(
                    "in `{}`: reference to local `{}` escapes the function",
                    f.name, br.place
                ),
                None,
            ));
        }
    }

    // (2) Aliased `&mut`: a `&mut` may not coexist with any other live borrow of
    // the same place (neither a second `&mut` nor a shared `&`). Two shared `&`
    // borrows may coexist, so skip when both are immutable.
    for i in 0..infos.len() {
        for j in (i + 1)..infos.len() {
            let a = &infos[i];
            let b = &infos[j];
            if a.borrow.place != b.borrow.place {
                continue;
            }
            if !a.borrow.mutable && !b.borrow.mutable {
                continue;
            }
            let mut overlap = false;
            'outer: for (bi, blk) in f.blocks.iter().enumerate() {
                for si in 0..=blk.stmts.len() {
                    if is_live_at(a, bi, si) && is_live_at(b, bi, si) {
                        overlap = true;
                        break 'outer;
                    }
                }
            }
            if overlap {
                let msg = if a.borrow.mutable && b.borrow.mutable {
                    format!(
                        "in `{}`: cannot borrow `{}` as mutable more than once at a time",
                        f.name, a.borrow.place
                    )
                } else if a.borrow.mutable {
                    format!(
                        "in `{}`: cannot borrow `{}` as immutable because it is also borrowed as mutable",
                        f.name, a.borrow.place
                    )
                } else {
                    format!(
                        "in `{}`: cannot borrow `{}` as mutable because it is also borrowed as immutable",
                        f.name, a.borrow.place
                    )
                };
                errors.push(Diag::error(msg, None));
            }
        }
    }
}

fn loc_le(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1 <= b.1)
}

fn successors(term: &Terminator) -> Vec<usize> {
    match term {
        Terminator::Goto(t) => vec![*t],
        Terminator::If(_, t, e) => vec![*t, *e],
        _ => vec![],
    }
}

fn reachable_from(start: usize, succs: &[Vec<usize>]) -> HashSet<usize> {
    let mut seen = HashSet::new();
    let mut stack = vec![start];
    while let Some(b) = stack.pop() {
        if seen.insert(b) {
            for s in &succs[b] {
                if !seen.contains(s) {
                    stack.push(*s);
                }
            }
        }
    }
    seen
}

fn operands_of(rv: &Rvalue) -> Vec<String> {
    match rv {
        Rvalue::Use(o) | Rvalue::Unary(_, o) | Rvalue::Ref(o, _) | Rvalue::Field(o, _, _) => names(o),
        Rvalue::Binary(_, l, r) => {
            let mut v = names(l);
            v.extend(names(r));
            v
        }
        Rvalue::Call(_, args) => args.iter().flat_map(names).collect(),
        Rvalue::StructLit(_, fields) => fields.iter().flat_map(|(_, o)| names(o)).collect(),
        // `StoreField` only appears in the `async fn` state-machine MIR, which is
        // generated for the C backend; the borrow checker still runs over the
        // whole MIR, so it must be exhaustive here (it never reads a stored
        // future field as a borrow).
        Rvalue::StoreField(_, _, v, _) => names(v),
    }
}

fn names(o: &Operand) -> Vec<String> {
    if let Operand::Var(n) = o {
        vec![n.clone()]
    } else {
        vec![]
    }
}
