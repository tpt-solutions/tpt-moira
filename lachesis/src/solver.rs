//! Foundation for the Phase-3 Chalk-like trait solver.
//!
//! This is a deliberately small, self-contained Horn-clause resolution engine —
//! the seam the full Prolog-style solver will grow out of:
//!
//! * [`Term`]s unify with inference variables ([`Term::Var`]) under an occurs
//!   check;
//! * a [`Program`] is a set of clauses `forall vars. body => head`;
//! * [`Solver::solve`] performs depth-limited SLD-style resolution over a
//!   conjunction of [`Goal`]s, with a memo cache keyed by goal+depth to give
//!   early-exit / `Ambiguous` behavior (the "memoization + early-exit
//!   heuristics" deliverable).
//!
//! The built-in operator-trait facts are emitted by [`builtin_program`], which
//! mirrors `lachesis::traits::TraitDb` exactly, so the solver can back the
//! existing operator checks while remaining extensible to user `impl`s,
//! where-clauses, and associated types.

use crate::traits::BuiltinTrait;
use crate::ty::Ty;
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};

/// A compiler-known trait, mirroring [`BuiltinTrait`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TraitId {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ord,
    /// A user-defined trait, e.g. the `Eq` in `impl Eq for MyStruct`.
    User(String),
}

impl From<BuiltinTrait> for TraitId {
    fn from(t: BuiltinTrait) -> Self {
        match t {
            BuiltinTrait::Add => TraitId::Add,
            BuiltinTrait::Sub => TraitId::Sub,
            BuiltinTrait::Mul => TraitId::Mul,
            BuiltinTrait::Div => TraitId::Div,
            BuiltinTrait::Rem => TraitId::Rem,
            BuiltinTrait::BitAnd => TraitId::BitAnd,
            BuiltinTrait::BitOr => TraitId::BitOr,
            BuiltinTrait::BitXor => TraitId::BitXor,
            BuiltinTrait::Shl => TraitId::Shl,
            BuiltinTrait::Shr => TraitId::Shr,
            BuiltinTrait::Eq => TraitId::Eq,
            BuiltinTrait::Ord => TraitId::Ord,
        }
    }
}

/// A solver term: a concrete type, an inference variable, a type application, a
/// trait applied to a self-type (the head of a goal/clause), or an associated
/// type projection `Self::Assoc`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Term {
    Var(usize),
    Concr(Ty),
    /// A type application `Wrap<T>`: a type constructor applied to arguments.
    Apply(String, Vec<Term>),
    /// A trait applied to a self-type — the head of a goal/clause.
    App(TraitId, Box<Term>),
    /// An associated-type projection `<Self as Trait>::Assoc`.
    Proj(String, String, Box<Term>),
}

/// A goal is a conjunction of terms that must all hold.
pub type Goal = Vec<Term>;

/// A Horn clause `forall vars. body => head`.
#[derive(Debug, Clone)]
pub struct Clause {
    pub vars: Vec<usize>,
    pub body: Vec<Term>,
    pub head: Term,
}

/// A set of clauses to resolve goals against.
#[derive(Debug, Default, Clone)]
pub struct Program {
    pub clauses: Vec<Clause>,
    /// Associated-type normalization facts: each entry maps a projection term
    /// `<Self as Trait>::Assoc` to its normalized value (per the `type Assoc =`
    /// declared in some `impl`).
    pub projections: Vec<(Term, Term)>,
}

impl Program {
    /// Add a fact (a clause with an empty body).
    pub fn fact(&mut self, head: Term) {
        self.clauses.push(Clause {
            vars: Vec::new(),
            body: Vec::new(),
            head,
        });
    }
}

/// The result of solving a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The goal is provable.
    Yes,
    /// The goal is refutable (no clause applies).
    No,
    /// Resolution hit the depth limit before deciding — neither proven nor
    /// refuted. The richer solver will resolve this with more context.
    Ambiguous,
}

type Subst = HashMap<usize, Term>;

/// Resolve a term through the substitution as far as possible.
fn resolve(t: &Term, sub: &Subst) -> Term {
    let mut cur = t.clone();
    while let Term::Var(v) = &cur {
        match sub.get(v) {
            Some(next) => cur = next.clone(),
            None => break,
        }
    }
    cur
}

/// Occurs check: does variable `v` appear inside `t`?
fn occurs(v: usize, t: &Term, sub: &Subst) -> bool {
    match resolve(t, sub) {
        Term::Var(u) => u == v,
        Term::Concr(_) => false,
        Term::Apply(_, args) => args.iter().any(|a| occurs(v, a, sub)),
        Term::App(_, boxed) => occurs(v, &boxed, sub),
        Term::Proj(_, _, boxed) => occurs(v, &boxed, sub),
    }
}

/// Unify two terms under `sub`, extending `sub` on success.
fn unify(a: &Term, b: &Term, sub: &mut Subst) -> bool {
    let a = resolve(a, sub);
    let b = resolve(b, sub);
    match (a, b) {
        (Term::Var(v), other) | (other, Term::Var(v)) => {
            if occurs(v, &other, sub) {
                return false;
            }
            sub.insert(v, other);
            true
        }
        (Term::Concr(x), Term::Concr(y)) => x == y,
        (Term::Apply(n1, a1), Term::Apply(n2, a2)) => {
            n1 == n2 && a1.len() == a2.len() && a1.iter().zip(&a2).all(|(x, y)| unify(x, y, sub))
        }
        (Term::App(t1, a1), Term::App(t2, a2)) => t1 == t2 && unify(&a1, &a2, sub),
        (Term::Proj(t1, a1, s1), Term::Proj(t2, a2, s2)) => {
            t1 == t2 && a1 == a2 && unify(&s1, &s2, sub)
        }
        _ => false,
    }
}

/// Apply `sub` throughout `t`, reconstructing a fully-substituted term.
fn subst(t: &Term, sub: &Subst) -> Term {
    match t {
        Term::Var(_) => resolve(t, sub),
        Term::Concr(x) => Term::Concr(x.clone()),
        Term::Apply(n, args) => {
            Term::Apply(n.clone(), args.iter().map(|a| subst(a, sub)).collect())
        }
        Term::App(tr, s) => Term::App(tr.clone(), Box::new(subst(s, sub))),
        Term::Proj(tr, a, s) => Term::Proj(tr.clone(), a.clone(), Box::new(subst(s, sub))),
    }
}

fn hash_goal(goals: &[Term], sub: &Subst) -> u64 {
    let mut h = DefaultHasher::new();
    goals.len().hash(&mut h);
    for g in goals {
        resolve(g, sub).hash(&mut h);
    }
    h.finish()
}

/// Depth-limited SLD resolution with memoization.
fn solve_goal(
    program: &Program,
    goals: &[Term],
    sub: &Subst,
    depth: usize,
    memo: &mut HashMap<(u64, usize), Answer>,
) -> Answer {
    if goals.is_empty() {
        return Answer::Yes;
    }
    if depth == 0 {
        return Answer::Ambiguous;
    }
    let key = (hash_goal(goals, sub), depth);
    if let Some(a) = memo.get(&key) {
        return *a;
    }
    let resolved_head = resolve(&goals[0], sub);
    let mut result = Answer::No;
    let mut any_ambiguous = false;
    for clause in &program.clauses {
        let mut sub2 = sub.clone();
        if unify(&clause.head, &resolved_head, &mut sub2) {
            let mut new_goals: Vec<Term> = clause.body.clone();
            new_goals.extend_from_slice(&goals[1..]);
            match solve_goal(program, &new_goals, &sub2, depth - 1, memo) {
                Answer::Yes => {
                    result = Answer::Yes;
                    break;
                }
                Answer::Ambiguous => any_ambiguous = true,
                Answer::No => {}
            }
        }
    }
    if result == Answer::No && any_ambiguous {
        result = Answer::Ambiguous;
    }
    memo.insert(key, result);
    result
}

/// A small Horn-clause solver over [`Term`].
pub struct Solver {
    pub program: Program,
    pub max_depth: usize,
}

impl Solver {
    pub fn new(program: Program) -> Self {
        Solver {
            program,
            max_depth: 64,
        }
    }

    /// Solve a goal, returning the strongest answer the depth limit allows.
    pub fn solve(&self, goal: &Goal) -> Answer {
        let mut memo = HashMap::new();
        solve_goal(
            &self.program,
            goal,
            &HashMap::new(),
            self.max_depth,
            &mut memo,
        )
    }

    /// Whether `ty` implements the trait `t` (per the solver's program).
    pub fn implements_trait(&self, t: TraitId, ty: &Ty) -> bool {
        matches!(
            self.solve(&vec![Term::App(t, Box::new(Term::Concr(ty.clone())))]),
            Answer::Yes
        )
    }

    /// Whether `self_term` implements the user-defined trait `trait_name`,
    /// where `self_term` may be a generic application (`Wrap<Var(0)>`),
    /// supporting parameterized impls and where-clauses.
    pub fn impl_for_term(&self, trait_name: &str, self_term: &Term) -> Answer {
        let goal = vec![Term::App(
            TraitId::User(trait_name.to_string()),
            Box::new(self_term.clone()),
        )];
        self.solve(&goal)
    }

    /// Normalize an associated-type projection `<Self as Trait>::Assoc` to its
    /// resolved type term, if a matching `type` projection exists. The
    /// substitution produced by unification is applied to the projected value.
    pub fn normalize(&self, proj: &Term) -> Option<Term> {
        for (p, v) in &self.program.projections {
            let mut sub = Subst::new();
            if unify(p, proj, &mut sub) {
                return Some(subst(v, &sub));
            }
        }
        None
    }
}

/// The same rule `TraitDb::implements` uses, expressed over [`TraitId`], so the
/// built-in program is a drop-in mirror of the existing checks.
pub fn implements_rule(t: TraitId, ty: &Ty) -> bool {
    let numeric = ty.is_numeric();
    match t {
        TraitId::Add | TraitId::Sub | TraitId::Mul | TraitId::Div | TraitId::Rem => numeric,
        TraitId::BitAnd | TraitId::BitOr | TraitId::BitXor => ty.is_integer(),
        TraitId::Shl | TraitId::Shr => ty.is_integer(),
        TraitId::Eq => numeric || matches!(ty, Ty::Bool | Ty::Char | Ty::Str),
        TraitId::Ord => numeric || matches!(ty, Ty::Bool | Ty::Char),
        // User-defined traits are not producible by this rule; the solver uses
        // the user's `impl` clauses instead.
        TraitId::User(_) => false,
    }
}

/// Build the built-in operator-trait program (one fact per type that
/// implements each trait), mirroring `TraitDb`.
pub fn builtin_program() -> Program {
    let types = [
        Ty::Int,
        Ty::I8,
        Ty::I16,
        Ty::I32,
        Ty::I64,
        Ty::I128,
        Ty::Isize,
        Ty::U8,
        Ty::U16,
        Ty::U32,
        Ty::U64,
        Ty::U128,
        Ty::Usize,
        Ty::F32,
        Ty::F64,
        Ty::Bool,
        Ty::Char,
        Ty::Str,
    ];
    let traits = [
        TraitId::Add,
        TraitId::Sub,
        TraitId::Mul,
        TraitId::Div,
        TraitId::Rem,
        TraitId::BitAnd,
        TraitId::BitOr,
        TraitId::BitXor,
        TraitId::Shl,
        TraitId::Shr,
        TraitId::Eq,
        TraitId::Ord,
    ];
    let mut prog = Program::default();
    for t in &traits {
        for ty in &types {
            if implements_rule(t.clone(), ty) {
                prog.fact(Term::App(t.clone(), Box::new(Term::Concr(ty.clone()))));
            }
        }
    }
    prog
}

/// A solver seeded with the built-in operator-trait program.
pub fn builtin_solver() -> Solver {
    Solver::new(builtin_program())
}

/// Build the built-in program augmented with the user `impl` clauses found in a
/// parsed program. This is the "impl search" the full Chalk-like solver will
/// perform: each `impl<T> Trait for Type<T> where T: Bound` becomes a clause
/// `App(User(Trait), SelfTerm) :- App(User(Bound), VarT)`, and each
/// `type Assoc = Val;` inside the impl becomes an associated-type projection
/// `Proj(Trait, Assoc, SelfTerm) -> ValTerm`.
pub fn program_with_impls(prog: &clotho::ast::Program) -> Program {
    let mut prog2 = builtin_program();
    for item in &prog.items {
        if let clotho::ast::ItemKind::Impl(imp) = &item.kind {
            let (clause, projections) = impl_to_clause(imp);
            prog2.clauses.push(clause);
            prog2.projections.extend(projections);
        }
    }
    prog2
}

/// Translate a Clotho type into a solver [`Term`] using `vars` to map generic
/// parameter names to inference variables. Path types with arguments become
/// [`Term::Apply`] (e.g. `Wrap<T>` -> `Apply("Wrap", [Var(0)])`); bare paths map
/// to concrete `Struct`/`primitive` types; anything without generic structure
/// falls back to the direct [`Ty`] translation.
fn ast_type_to_term(ty: &clotho::ast::Type, vars: &HashMap<String, usize>) -> Term {
    match &ty.kind {
        clotho::ast::TypeKind::Path(p, args) => {
            let name = p
                .segments
                .last()
                .map(|s| s.node.clone())
                .unwrap_or_default();
            if let Some(&v) = vars.get(&name) {
                return Term::Var(v);
            }
            if !args.is_empty() {
                let arg_terms = args.iter().map(|a| ast_type_to_term(a, vars)).collect();
                return Term::Apply(name, arg_terms);
            }
            match crate::infer::TypeChecker::ast_to_ty(ty) {
                Ty::Unknown => Term::Apply(name, Vec::new()),
                concrete => Term::Concr(concrete),
            }
        }
        // Non-path types carry no generic parameters in the subset; lower them
        // directly to their concrete `Ty` representation.
        _ => {
            let t = crate::infer::TypeChecker::ast_to_ty(ty);
            Term::Concr(if matches!(t, Ty::Unknown) {
                Ty::Unit
            } else {
                t
            })
        }
    }
}

/// Whether `t` contains any inference variable (i.e. refers to a generic
/// parameter), used to decide whether a well-formedness check is decidable.
fn term_has_var(t: &Term) -> bool {
    match t {
        Term::Var(_) => true,
        Term::Concr(_) => false,
        Term::Apply(_, args) => args.iter().any(term_has_var),
        Term::App(_, s) => term_has_var(s),
        Term::Proj(_, _, s) => term_has_var(s),
    }
}

/// Lower a single `impl` block into a solver clause (with generic self-type and
/// where-clause body) plus its associated-type projection facts.
fn impl_to_clause(imp: &clotho::ast::ImplDef) -> (Clause, Vec<(Term, Term)>) {
    let mut vars = HashMap::new();
    let mut next = 0usize;
    for p in &imp.generics.params {
        vars.insert(p.name.node.clone(), next);
        next += 1;
    }
    let self_term = ast_type_to_term(&imp.for_ty, &vars);

    // Where-clause body: each parameter bound and each `where` predicate becomes
    // a goal `SelfParam: Bound` that must hold (a where-clause).
    let mut body = Vec::new();
    for p in &imp.generics.params {
        let vi = vars[&p.name.node];
        for bound in &p.bounds {
            let bname = bound_trait_name(bound);
            body.push(Term::App(TraitId::User(bname), Box::new(Term::Var(vi))));
        }
    }
    for pred in &imp.generics.where_clause {
        for bound in &pred.bounds {
            let bname = bound_trait_name(bound);
            body.push(Term::App(
                TraitId::User(bname),
                Box::new(ast_type_to_term(&pred.ty, &vars)),
            ));
        }
    }

    let clause = Clause {
        vars: (0..next).collect(),
        body,
        head: Term::App(
            TraitId::User(imp.trait_name.node.clone()),
            Box::new(self_term.clone()),
        ),
    };

    // Associated-type projections: `type Assoc = Val;` inside the impl.
    let mut projections = Vec::new();
    for item in &imp.items {
        if let clotho::ast::ItemKind::TypeAlias(ta) = &item.kind {
            let proj = Term::Proj(
                imp.trait_name.node.clone(),
                ta.name.node.clone(),
                Box::new(self_term.clone()),
            );
            projections.push((proj, ast_type_to_term(&ta.ty, &vars)));
        }
    }

    (clause, projections)
}

/// The trait name of a bound, taken from the last path segment.
fn bound_trait_name(b: &clotho::ast::TypeBound) -> String {
    match b {
        clotho::ast::TypeBound::Trait(p) => p
            .segments
            .last()
            .map(|s| s.node.clone())
            .unwrap_or_default(),
    }
}

/// Map a built-in trait's surface name to its [`TraitId`], if it is one.
pub fn builtin_trait_id(name: &str) -> Option<TraitId> {
    match name {
        "Add" => Some(TraitId::Add),
        "Sub" => Some(TraitId::Sub),
        "Mul" => Some(TraitId::Mul),
        "Div" => Some(TraitId::Div),
        "Rem" => Some(TraitId::Rem),
        "BitAnd" => Some(TraitId::BitAnd),
        "BitOr" => Some(TraitId::BitOr),
        "BitXor" => Some(TraitId::BitXor),
        "Shl" => Some(TraitId::Shl),
        "Shr" => Some(TraitId::Shr),
        "PartialEq" | "Eq" => Some(TraitId::Eq),
        "PartialOrd" | "Ord" => Some(TraitId::Ord),
        _ => None,
    }
}

/// Coherence / overlap check across all `impl` blocks in `prog`.
///
/// Two `impl`s of the *same* trait overlap (and are therefore incoherent) when
/// their self-types can be unified — including generic `impl`s whose self-types
/// unify for some instantiation. Returns `(span, message)` pairs for each
/// detected overlap.
pub fn coherence_report(prog: &clotho::ast::Program) -> Vec<(clotho::token::Span, String)> {
    let mut impls: Vec<&clotho::ast::ImplDef> = Vec::new();
    for item in &prog.items {
        if let clotho::ast::ItemKind::Impl(imp) = &item.kind {
            impls.push(imp);
        }
    }
    let mut errors = Vec::new();
    for i in 0..impls.len() {
        for j in (i + 1)..impls.len() {
            if impls[i].trait_name.node != impls[j].trait_name.node {
                continue;
            }
            // Build self-type terms in disjoint variable spaces so the two impls'
            // generic parameters never alias (impl `i` uses vars [0..);
            // impl `j` uses vars [1000..) offset).
            let mut vi = HashMap::new();
            let mut vj = HashMap::new();
            let base_j = 1000usize;
            for (k, p) in impls[i].generics.params.iter().enumerate() {
                vi.insert(p.name.node.clone(), k);
            }
            for (k, p) in impls[j].generics.params.iter().enumerate() {
                vj.insert(p.name.node.clone(), base_j + k);
            }
            let ti = ast_type_to_term(&impls[i].for_ty, &vi);
            let tj = ast_type_to_term(&impls[j].for_ty, &vj);
            let mut sub = Subst::new();
            if unify(&ti, &tj, &mut sub) {
                errors.push((
                    impls[j].trait_name.span,
                    format!(
                        "conflicting implementations of trait `{}` for overlapping type `{}`",
                        impls[j].trait_name.node,
                        impls[i].for_ty.node_name()
                    ),
                ));
            }
        }
    }
    errors
}

/// Well-formedness check for `impl` where-clauses / parameter bounds.
///
/// A `where`/`T: Bound` predicate whose type is concrete (no generic parameter)
/// and a primitive or built-in type must actually satisfy `Bound`; otherwise the
/// `impl` is ill-formed. Predicates over user struct types are skipped because
/// they require full generic instantiation, which is out of scope here.
pub fn well_formedness_report(prog: &clotho::ast::Program) -> Vec<(clotho::token::Span, String)> {
    let solver = Solver::new(program_with_impls(prog));
    let mut errors = Vec::new();
    for item in &prog.items {
        let imp = match &item.kind {
            clotho::ast::ItemKind::Impl(imp) => imp,
            _ => continue,
        };
        let mut vars = HashMap::new();
        for (k, p) in imp.generics.params.iter().enumerate() {
            vars.insert(p.name.node.clone(), k);
        }
        let mut predicates: Vec<(clotho::ast::Type, String, clotho::token::Span)> = Vec::new();
        for p in &imp.generics.params {
            for bound in &p.bounds {
                let param_ty = clotho::ast::Type {
                    kind: clotho::ast::TypeKind::Path(
                        clotho::ast::Path {
                            segments: vec![clotho::ast::Spanned::new(
                                p.name.node.clone(),
                                p.name.span,
                            )],
                        },
                        Vec::new(),
                    ),
                    span: p.name.span,
                };
                predicates.push((param_ty, bound_trait_name(bound), p.name.span));
            }
        }
        for pred in &imp.generics.where_clause {
            for bound in &pred.bounds {
                predicates.push((pred.ty.clone(), bound_trait_name(bound), pred.ty.span));
            }
        }
        for (ty, bound, span) in predicates {
            let term = ast_type_to_term(&ty, &vars);
            if term_has_var(&term) {
                continue; // generic — undecidable without instantiation
            }
            let ty = match &term {
                Term::Concr(c) => c.clone(),
                _ => continue,
            };
            if matches!(ty, Ty::Struct(_)) {
                continue; // skip user struct types (needs generic resolution)
            }
            let holds = if let Some(t) = builtin_trait_id(&bound) {
                implements_rule(t, &ty)
            } else {
                solver.implements_trait(TraitId::User(bound.clone()), &ty)
            };
            if !holds {
                errors.push((
                    span,
                    format!("where-clause bound `{ty}: {bound}` is not satisfied for `{ty}`"),
                ));
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solver_backs_builtin_traits() {
        let solver = builtin_solver();
        // numeric types implement the arithmetic traits
        assert!(solver.implements_trait(TraitId::Add, &Ty::I32));
        assert!(solver.implements_trait(TraitId::Mul, &Ty::F64));
        // bool does not
        assert!(!solver.implements_trait(TraitId::Add, &Ty::Bool));
        // bitwise requires integers
        assert!(solver.implements_trait(TraitId::BitAnd, &Ty::U8));
        assert!(!solver.implements_trait(TraitId::BitAnd, &Ty::F64));
        // Eq/Ord rules
        assert!(solver.implements_trait(TraitId::Eq, &Ty::Str));
        assert!(!solver.implements_trait(TraitId::Ord, &Ty::Str));
        assert!(solver.implements_trait(TraitId::Ord, &Ty::Bool));
    }

    #[test]
    fn solver_matches_trait_db() {
        let solver = builtin_solver();
        let types = [
            Ty::Int,
            Ty::I32,
            Ty::U64,
            Ty::F64,
            Ty::Bool,
            Ty::Char,
            Ty::Str,
        ];
        let traits = [
            BuiltinTrait::Add,
            BuiltinTrait::Sub,
            BuiltinTrait::Mul,
            BuiltinTrait::Div,
            BuiltinTrait::Rem,
            BuiltinTrait::BitAnd,
            BuiltinTrait::BitOr,
            BuiltinTrait::BitXor,
            BuiltinTrait::Shl,
            BuiltinTrait::Shr,
            BuiltinTrait::Eq,
            BuiltinTrait::Ord,
        ];
        for t in traits {
            for ty in &types {
                assert_eq!(
                    solver.implements_trait(t.into(), ty),
                    crate::traits::TraitDb::implements(t, ty),
                    "{:?} {:?}",
                    t,
                    ty
                );
            }
        }
    }

    #[test]
    fn unification_with_vars() {
        let mut sub = Subst::new();
        // App(Add, Var(0)) unifies with App(Add, Concr(I32)) => Var(0) := I32
        let a = Term::App(TraitId::Add, Box::new(Term::Var(0)));
        let b = Term::App(TraitId::Add, Box::new(Term::Concr(Ty::I32)));
        assert!(unify(&a, &b, &mut sub));
        assert_eq!(resolve(&Term::Var(0), &sub), Term::Concr(Ty::I32));
    }

    #[test]
    fn occurs_check_blocks_infinite_loop() {
        let mut sub = Subst::new();
        // Var(0) = App(Add, Var(0)) must fail the occurs check.
        let a = Term::Var(0);
        let b = Term::App(TraitId::Add, Box::new(Term::Var(0)));
        assert!(!unify(&a, &b, &mut sub));
    }

    #[test]
    fn simple_rule_resolution() {
        // fact: Even(0).  rule: Even(Var(0)) :- Even(Sub(Var(0), 1))  (illustrative)
        // We test that a single fact resolves a ground goal.
        let mut prog = Program::default();
        prog.fact(Term::App(TraitId::Eq, Box::new(Term::Concr(Ty::Bool))));
        let solver = Solver::new(prog);
        let goal = vec![Term::App(TraitId::Eq, Box::new(Term::Concr(Ty::Bool)))];
        assert_eq!(solver.solve(&goal), Answer::Yes);
        let nope = vec![Term::App(TraitId::Eq, Box::new(Term::Concr(Ty::Str)))];
        assert_eq!(solver.solve(&nope), Answer::No);
    }

    #[test]
    fn user_defined_trait_impl_search() {
        // A user trait `Marker` implemented for `i32` (a fact).
        let mut prog = Program::default();
        prog.fact(Term::App(
            TraitId::User("Marker".into()),
            Box::new(Term::Concr(Ty::I32)),
        ));
        let solver = Solver::new(prog);
        let holds = vec![Term::App(
            TraitId::User("Marker".into()),
            Box::new(Term::Concr(Ty::I32)),
        )];
        assert_eq!(solver.solve(&holds), Answer::Yes);
        // Not implemented for `bool`.
        let missing = vec![Term::App(
            TraitId::User("Marker".into()),
            Box::new(Term::Concr(Ty::Bool)),
        )];
        assert_eq!(solver.solve(&missing), Answer::No);
    }

    #[test]
    fn apply_term_unification() {
        let mut sub = Subst::new();
        assert!(unify(
            &Term::Apply("Wrap".into(), vec![Term::Var(0)]),
            &Term::Apply("Wrap".into(), vec![Term::Concr(Ty::I32)]),
            &mut sub,
        ));
        assert_eq!(resolve(&Term::Var(0), &sub), Term::Concr(Ty::I32));
        // Different type constructors do not unify.
        let mut sub2 = Subst::new();
        assert!(!unify(
            &Term::Apply("Wrap".into(), vec![Term::Var(0)]),
            &Term::Apply("Box".into(), vec![Term::Concr(Ty::I32)]),
            &mut sub2,
        ));
    }

    #[test]
    fn proj_term_unification() {
        let mut sub = Subst::new();
        assert!(unify(
            &Term::Proj("It".into(), "Item".into(), Box::new(Term::Var(0))),
            &Term::Proj("It".into(), "Item".into(), Box::new(Term::Concr(Ty::I32))),
            &mut sub,
        ));
        assert_eq!(resolve(&Term::Var(0), &sub), Term::Concr(Ty::I32));
    }

    #[test]
    fn normalize_generic_associated_type() {
        // impl<T> It for Wrap<T> { type Item = T; }
        let mut prog = Program::default();
        prog.projections.push((
            Term::Proj(
                "It".into(),
                "Item".into(),
                Box::new(Term::Apply("Wrap".into(), vec![Term::Var(0)])),
            ),
            Term::Var(0),
        ));
        let solver = Solver::new(prog);
        // <Wrap<i32> as It>::Item normalizes to i32.
        let query = Term::Proj(
            "It".into(),
            "Item".into(),
            Box::new(Term::Apply("Wrap".into(), vec![Term::Concr(Ty::I32)])),
        );
        assert_eq!(solver.normalize(&query), Some(Term::Concr(Ty::I32)));
        // No matching projection for `Other`.
        let other = Term::Proj(
            "It".into(),
            "Item".into(),
            Box::new(Term::Apply("Other".into(), vec![Term::Concr(Ty::I32)])),
        );
        assert_eq!(solver.normalize(&other), None);
    }

    #[test]
    fn coherence_report_flags_overlap() {
        let src = "struct P {}
                   impl Eq for P {}
                   impl Eq for P {}";
        let prog = clotho::parser::Parser::from_source(src)
            .parse_program()
            .unwrap();
        assert!(!coherence_report(&prog).is_empty());
    }

    #[test]
    fn coherence_report_allows_distinct_impls() {
        let src = "struct A {}
                   struct B {}
                   impl Eq for A {}
                   impl Eq for B {}";
        let prog = clotho::parser::Parser::from_source(src)
            .parse_program()
            .unwrap();
        assert!(coherence_report(&prog).is_empty());
    }
}
