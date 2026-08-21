//! Type representation for the Lachesis type engine.
//!
//! For the Phase-1 subset Lachesis uses a direct (non-generic) type language.
//! Inference variables are not needed because every expression's type is
//! determined locally; integer literals default to [`Ty::Int`] (i32). The
//! [`Unknown`](Ty::Unknown) variant is an error sentinel that absorbs further
//! errors so a single mistake does not cascade into a wall of diagnostics.

use std::fmt;

/// A Moira type, restricted to the Phase-1 subset.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    /// Default integer type (the `i32` fallback for unsuffixed integer literals).
    Int,
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    F32,
    F64,
    Bool,
    Char,
    Str,
    /// The unit type `()`.
    Unit,
    /// Shared reference `&T`. (Mutability is not yet tracked — see the borrow
    /// checker module for the rationale.)
    Ref(Box<Ty>),
    /// Mutable reference `&mut T`.
    RefMut(Box<Ty>),
    /// A named (struct) type.
    Struct(String),
    /// Function type `(params) -> ret`.
    Fn(Vec<Ty>, Box<Ty>),
    /// A future that yields `Box<Ty>` when awaited (the async subset models
    /// `Future<Output = T>` as `Future(Box<T>)`).
    Future(Box<Ty>),
    /// A pinned value `Pin<Box<T>>`.
    Pin(Box<Ty>),
    /// A range `a..b`, produced by the `..` operator (used by `for`).
    Range,
    /// A generic type parameter, e.g. `T` in `fn f<T>(x: T) -> T`. Carries the
    /// parameter's name so the inference engine can enforce the trait bounds
    /// declared on it (`T: Add`) and substitute a concrete type at call sites.
    Generic(String),
    /// A type constructor applied to type arguments, e.g. `Vec<i32>` or
    /// `Option<T>`. Carries the base type name and its (already-resolved)
    /// argument types. Used wherever the Phase-3 subset writes a generic
    /// application in a type annotation or struct field.
    Apply(String, Vec<Ty>),
    /// A fixed-length array `[T; N]` (or an unsized slice `[T]` modelled with
    /// `N == 0`). Carries the element type and the (compile-time) length.
    Array(Box<Ty>, u128),
    /// A tuple type `(T1, T2, …, Tn)`, produced by tuple literals and used in
    /// patterns / return types across the compiler's own code.
    Tuple(Vec<Ty>),
    /// Error sentinel.
    Unknown,
}

impl Ty {
    /// True for any numeric type (integer or float).
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            Ty::Int
                | Ty::I8
                | Ty::I16
                | Ty::I32
                | Ty::I64
                | Ty::I128
                | Ty::Isize
                | Ty::U8
                | Ty::U16
                | Ty::U32
                | Ty::U64
                | Ty::U128
                | Ty::Usize
                | Ty::F32
                | Ty::F64
        )
    }

    /// True for signed integer types (used for sign-sensitive operations).
    pub fn is_signed_int(&self) -> bool {
        matches!(
            self,
            Ty::Int | Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::I128 | Ty::Isize
        )
    }

    /// True for integer (non-float) types.
    pub fn is_integer(&self) -> bool {
        matches!(
            self,
            Ty::Int
                | Ty::I8
                | Ty::I16
                | Ty::I32
                | Ty::I64
                | Ty::I128
                | Ty::Isize
                | Ty::U8
                | Ty::U16
                | Ty::U32
                | Ty::U64
                | Ty::U128
                | Ty::Usize
        )
    }

    /// True for floating point types.
    pub fn is_float(&self) -> bool {
        matches!(self, Ty::F32 | Ty::F64)
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Int => write!(f, "i32"),
            Ty::I8 => write!(f, "i8"),
            Ty::I16 => write!(f, "i16"),
            Ty::I32 => write!(f, "i32"),
            Ty::I64 => write!(f, "i64"),
            Ty::I128 => write!(f, "i128"),
            Ty::Isize => write!(f, "isize"),
            Ty::U8 => write!(f, "u8"),
            Ty::U16 => write!(f, "u16"),
            Ty::U32 => write!(f, "u32"),
            Ty::U64 => write!(f, "u64"),
            Ty::U128 => write!(f, "u128"),
            Ty::Usize => write!(f, "usize"),
            Ty::F32 => write!(f, "f32"),
            Ty::F64 => write!(f, "f64"),
            Ty::Bool => write!(f, "bool"),
            Ty::Char => write!(f, "char"),
            Ty::Str => write!(f, "str"),
            Ty::Unit => write!(f, "()"),
            Ty::Ref(t) => write!(f, "&{}", t),
            Ty::RefMut(t) => write!(f, "&mut {}", t),
            Ty::Struct(n) => write!(f, "{}", n),
            Ty::Fn(params, ret) => {
                write!(f, "fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", p)?;
                }
                write!(f, ") -> {}", ret)
            }
            Ty::Future(t) => write!(f, "Future<{}>", t),
            Ty::Pin(t) => write!(f, "Pin<{}>", t),
            Ty::Range => write!(f, "Range"),
            Ty::Generic(n) => write!(f, "{}", n),
            Ty::Apply(n, args) => {
                write!(f, "{}<", n)?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", a)?;
                }
                write!(f, ">")
            }
            Ty::Array(t, n) => write!(f, "[{}; {}]", t, n),
            Ty::Tuple(tys) => {
                write!(f, "(")?;
                for (i, t) in tys.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", t)?;
                }
                write!(f, ")")
            }
            Ty::Unknown => write!(f, "<unknown>"),
        }
    }
}
