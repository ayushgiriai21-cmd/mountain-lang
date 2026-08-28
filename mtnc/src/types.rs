//! Core Type System (Phase 3), per Document 25 §2.3's scope: primitive
//! types (Document 5 §2), the full 6-rule type-inference engine
//! (Document 5 §4), and the data-shape half of `struct`/`enum`
//! (Document 7) — fields/variants only, not traits/impl/generics
//! (Phase 4/5).
//!
//! Implementation approach note (flagged, not silently assumed):
//! Document 17 §4.2 describes semantic analysis as "a constraint-based
//! (Hindley-Milner-style) type-inference algorithm". This module
//! implements Document 5 §4's 6 rules directly as a **bidirectional,
//! expected-type-propagating checker** (infer when no expectation is
//! given, check against an expectation when one is) rather than a full
//! generalized HM unifier with a separate constraint-solving pass. This
//! is a deliberate scope decision, not an oversight: Document 5's rules
//! are themselves described operationally (explicit-annotation-wins,
//! literal-defaulting, contextual-adoption, no-cross-branch-guessing,
//! ambiguity-is-an-error) rather than mandating a particular algorithm,
//! and full HM-style unification only becomes necessary once generic
//! type *variables* exist to unify over — which is Phase 5's territory
//! (Document 25 §2.3 explicitly scopes generics/monomorphization there,
//! not here). Revisit this decision in Phase 5 if bidirectional checking
//! turns out to be insufficient once real generic functions need
//! inferring. Flagged for sign-off, consistent with the other flagged
//! deviations from prior phases.
//!
//! Also flagged: `ast::Expr`/`ast::Stmt` don't carry per-node `Span`
//! information yet (only `ast::Item` does, from Phase 2). Type errors
//! below therefore report the *item* they occurred in as context, not a
//! precise line/column — full source-span-per-expression is not
//! required by this phase's exit criteria (Document 25 §2.3 asks for
//! correct accept/reject behavior, not diagnostic precision — that's
//! Document 22/Phase 23's job) but is a real precision gap worth noting
//! rather than silently pretending otherwise.

use crate::ast::*;
use std::collections::HashMap;

// ---------- Resolved types ----------

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    I8, I16, I32, I64, I128, Isize,
    U8, U16, U32, U64, U128, Usize,
    F32, F64,
    Bool,
    Char,
    StringTy,
    Str,
    Unit,
    Never,
    Array(Box<Ty>),
    Tuple(Vec<Ty>),
    Ref(bool, Box<Ty>),
    /// A user-declared struct or enum with no generic parameters, by
    /// name. Phase 3 had no generics at all; as of Phase 5, a
    /// zero-generic-parameter struct/enum still resolves to this
    /// variant (simplest, most common case), while one with generic
    /// parameters resolves to `Ty::Generic` once concrete/const
    /// arguments are supplied.
    Named(String),
    /// A generic struct/enum instantiated with concrete type and/or
    /// const arguments (Document 8), e.g. `Matrix<f64, 2, 3>` resolves
    /// to `Generic("Matrix", [Type(F64), Const(2), Const(3)])`.
    Generic(String, Vec<GenericArg>),
    /// An unresolved reference to one of the *enclosing* declaration's
    /// own generic type parameters (e.g. `T` inside `struct Pair<A,
    /// B> { first: A, second: B }`'s field types, before any concrete
    /// instantiation is known). Only appears transiently during
    /// generic-declaration registration/substitution; never the final
    /// type of a checked expression in ordinary (non-generic-body)
    /// code.
    TypeParam(String),
    /// `dyn Trait` (Document 7 §4.5) — statically known only to
    /// implement `Trait`, resolved via vtable at runtime. Method
    /// resolution against this type looks up the *trait's* declared
    /// signature, not any concrete `impl`'s.
    DynTrait(String),
    OptionTy(Box<Ty>),
    ResultTy(Box<Ty>, Box<Ty>),
    Fn(Vec<Ty>, Box<Ty>),
    /// The type of the `null` literal itself (Document 5 §2.6) — only
    /// a valid value inside `unsafe { }` blocks; everywhere else,
    /// wanting "value or absence" must use `OptionTy`.
    Null,
}

/// One generic argument at an instantiation site: either a concrete
/// type (`Matrix<f64, ...>`'s `f64`) or a resolved const value
/// (`Matrix<..., 2, 3>`'s `2`/`3`, Document 8 §8). Phase 5 only
/// supports integer-literal const-generic arguments — Document 8 §8's
/// own examples (`Matrix<f64,2,3>`, array sizes) never show anything
/// else in this position, so nothing broader is invented.
#[derive(Debug, Clone, PartialEq)]
pub enum GenericArg {
    Type(Ty),
    Const(i64),
}

impl std::fmt::Display for GenericArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenericArg::Type(t) => write!(f, "{}", t),
            GenericArg::Const(n) => write!(f, "{}", n),
        }
    }
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::I8 => write!(f, "i8"), Ty::I16 => write!(f, "i16"), Ty::I32 => write!(f, "i32"),
            Ty::I64 => write!(f, "i64"), Ty::I128 => write!(f, "i128"), Ty::Isize => write!(f, "isize"),
            Ty::U8 => write!(f, "u8"), Ty::U16 => write!(f, "u16"), Ty::U32 => write!(f, "u32"),
            Ty::U64 => write!(f, "u64"), Ty::U128 => write!(f, "u128"), Ty::Usize => write!(f, "usize"),
            Ty::F32 => write!(f, "f32"), Ty::F64 => write!(f, "f64"),
            Ty::Bool => write!(f, "bool"), Ty::Char => write!(f, "char"),
            Ty::StringTy => write!(f, "String"), Ty::Str => write!(f, "str"),
            Ty::Unit => write!(f, "()"), Ty::Never => write!(f, "!"),
            Ty::Array(t) => write!(f, "[{}]", t),
            Ty::Tuple(ts) => write!(f, "({})", ts.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ")),
            Ty::Ref(m, t) => write!(f, "&{}{}", if *m { "mut " } else { "" }, t),
            Ty::Named(n) => write!(f, "{}", n),
            Ty::Generic(n, args) => write!(f, "{}<{}>", n, args.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")),
            Ty::TypeParam(n) => write!(f, "{}", n),
            Ty::DynTrait(n) => write!(f, "dyn {}", n),
            Ty::OptionTy(t) => write!(f, "Option<{}>", t),
            Ty::ResultTy(o, e) => write!(f, "Result<{}, {}>", o, e),
            Ty::Fn(ps, r) => write!(f, "fn({}) -> {}", ps.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", "), r),
            Ty::Null => write!(f, "null"),
        }
    }
}

fn ty_is_integer(t: &Ty) -> bool {
    matches!(t, Ty::I8|Ty::I16|Ty::I32|Ty::I64|Ty::I128|Ty::Isize|Ty::U8|Ty::U16|Ty::U32|Ty::U64|Ty::U128|Ty::Usize)
}
fn ty_is_float(t: &Ty) -> bool {
    matches!(t, Ty::F32 | Ty::F64)
}
fn ty_is_numeric(t: &Ty) -> bool {
    ty_is_integer(t) || ty_is_float(t)
}

/// Resolves an `ast::Type` (syntactic, from the parser) to a `Ty`
/// (semantic). Struct/enum names are trusted to exist here and
/// validated separately by `TypeChecker::check_program`'s name-
/// resolution pass, so an unknown name still produces `Ty::Named`
/// rather than failing here — this function is pure syntax-to-shape
/// translation, not validation.
pub fn resolve_type(t: &Type) -> Ty {
    match t {
        Type::Primitive(s) => match s.as_str() {
            "i8" => Ty::I8, "i16" => Ty::I16, "i32" => Ty::I32, "i64" => Ty::I64,
            "i128" => Ty::I128, "isize" => Ty::Isize,
            "u8" => Ty::U8, "u16" => Ty::U16, "u32" => Ty::U32, "u64" => Ty::U64,
            "u128" => Ty::U128, "usize" => Ty::Usize,
            "f32" => Ty::F32, "f64" => Ty::F64,
            "bool" => Ty::Bool, "char" => Ty::Char,
            "String" => Ty::StringTy, "str" => Ty::Str,
            other => Ty::Named(other.to_string()),
        },
        Type::Named(n, _args) => Ty::Named(n.clone()),
        Type::Array(inner, _size) => Ty::Array(Box::new(resolve_type(inner))),
        Type::Tuple(ts) => Ty::Tuple(ts.iter().map(resolve_type).collect()),
        Type::Ref { mutable, inner, .. } => Ty::Ref(*mutable, Box::new(resolve_type(inner))),
        Type::Dyn(n, _) => Ty::DynTrait(n.clone()),
        Type::Fn(ps, r) => Ty::Fn(ps.iter().map(resolve_type).collect(), Box::new(resolve_type(r))),
        Type::Option(inner) => Ty::OptionTy(Box::new(resolve_type(inner))),
        Type::Result(o, e) => Ty::ResultTy(Box::new(resolve_type(o)), Box::new(resolve_type(e))),
        Type::Unit => Ty::Unit,
        Type::Never => Ty::Never,
        Type::ConstArg(_) => Ty::Unit, // not a real type position; see const_int_value below for the Phase 5 handling
    }
}

/// Extracts an integer value from a const-generic argument expression
/// (Document 8 §8's `Matrix<f64, 2, 3>` — the `2`/`3`). Phase 5 only
/// supports a bare integer literal here (optionally negative, though no
/// spec example shows a negative dimension) — Document 8's own examples
/// never show anything more complex (no const-generic arithmetic
/// expressions like `N + 1`), so nothing broader is invented.
fn const_int_value(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(Literal::Int(s)) => s.replace('_', "").parse::<i64>().ok(),
        Expr::Unary { op: UnaryOp::Neg, expr: inner } => {
            const_int_value(inner).map(|n| -n)
        }
        _ => None,
    }
}

/// Replaces `Ty::TypeParam(n)` with the concrete type supplied for `n`
/// in `subst` (used when checking a generic struct literal's fields
/// against a known concrete instantiation, e.g. `Pair<i32, String>`'s
/// `first` field should be checked as `i32`, not the abstract
/// `TypeParam("A")` stored in the struct's registered shape).
fn substitute_type_params(ty: &Ty, subst: &HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::TypeParam(n) => subst.get(n).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Array(inner) => Ty::Array(Box::new(substitute_type_params(inner, subst))),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| substitute_type_params(t, subst)).collect()),
        Ty::Ref(m, inner) => Ty::Ref(*m, Box::new(substitute_type_params(inner, subst))),
        Ty::OptionTy(inner) => Ty::OptionTy(Box::new(substitute_type_params(inner, subst))),
        Ty::ResultTy(o, e) => Ty::ResultTy(
            Box::new(substitute_type_params(o, subst)),
            Box::new(substitute_type_params(e, subst)),
        ),
        Ty::Fn(ps, r) => Ty::Fn(
            ps.iter().map(|t| substitute_type_params(t, subst)).collect(),
            Box::new(substitute_type_params(r, subst)),
        ),
        other => other.clone(),
    }
}

/// Structural unification for generic-parameter inference (Document 8
/// §2's own example: `fn largest<T>(list: [T]) -> T`, called as
/// `largest(nums)` where `nums: [i32]`, must infer `T = i32`). Walks
/// `param_ty` (which may contain `Ty::TypeParam` placeholders, from a
/// generic function's registered signature) against `arg_ty` (the
/// actual, concrete argument type), binding each `TypeParam` name to
/// whatever concrete type structurally occupies that position. Only
/// handles the structural shapes Document 8's own examples actually
/// use (a bare type parameter, one level of `[T]`/`&T`/tuple nesting) —
/// deeper or more exotic unification isn't attempted; an unresolved
/// type parameter is simply left unbound rather than reported as an
/// error (a known, flagged scope simplification).
fn unify_infer(param_ty: &Ty, arg_ty: &Ty, out: &mut HashMap<String, Ty>) {
    match (param_ty, arg_ty) {
        (Ty::TypeParam(n), t) => {
            out.entry(n.clone()).or_insert_with(|| t.clone());
        }
        (Ty::Array(p), Ty::Array(a)) => unify_infer(p, a, out),
        (Ty::Ref(_, p), Ty::Ref(_, a)) => unify_infer(p, a, out),
        (Ty::Tuple(ps), Ty::Tuple(as_)) if ps.len() == as_.len() => {
            for (p, a) in ps.iter().zip(as_.iter()) {
                unify_infer(p, a, out);
            }
        }
        _ => {}
    }
}

/// Canonical registry-lookup key for a `Ty` — the same key `impls_by_type`
/// is keyed by (an `impl`'s `target.name`, which for a primitive like
/// `impl Comparable for i32` is literally the text `"i32"`, since
/// primitive type names lex as plain identifiers, not keywords — Phase
/// 1's design decision). Used by `satisfies_bound` so a trait bound can
/// be checked against a monomorphized primitive type just as well as a
/// user-declared struct/enum.
fn ty_lookup_name(ty: &Ty) -> Option<String> {
    match ty {
        Ty::Named(n) | Ty::Generic(n, _) => Some(n.clone()),
        Ty::I8 => Some("i8".into()), Ty::I16 => Some("i16".into()), Ty::I32 => Some("i32".into()),
        Ty::I64 => Some("i64".into()), Ty::I128 => Some("i128".into()), Ty::Isize => Some("isize".into()),
        Ty::U8 => Some("u8".into()), Ty::U16 => Some("u16".into()), Ty::U32 => Some("u32".into()),
        Ty::U64 => Some("u64".into()), Ty::U128 => Some("u128".into()), Ty::Usize => Some("usize".into()),
        Ty::F32 => Some("f32".into()), Ty::F64 => Some("f64".into()),
        Ty::Bool => Some("bool".into()), Ty::Char => Some("char".into()),
        Ty::StringTy => Some("String".into()), Ty::Str => Some("str".into()),
        _ => None,
    }
}

// ---------- Errors ----------

#[derive(Debug, Clone, PartialEq)]
pub struct TypeError {
    pub message: String,
    /// Best-available context: which item this occurred in. See the
    /// module doc comment's note on why this isn't a precise span yet.
    pub context: String,
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "type error in `{}`: {}", self.context, self.message)
    }
}

// ---------- Struct/enum data-shape registry ----------

#[derive(Debug, Clone)]
pub struct StructShape {
    pub fields: Vec<(String, Ty)>,
    /// `true` for `struct Foo(A, B);` tuple structs — fields are
    /// positionally named "0", "1", ... internally.
    pub is_tuple: bool,
    /// This struct's own generic parameters, in declaration order
    /// (Document 8 §1/§8) — empty for a non-generic struct. Field types
    /// referencing one of these by name are stored as `Ty::TypeParam`,
    /// not `Ty::Named` (see `mark_type_params`).
    pub generics: Vec<GenericParam>,
}

/// Recursively replaces `Ty::Named(n)` with `Ty::TypeParam(n)` wherever
/// `n` matches one of the enclosing declaration's own generic
/// parameter names — so a generic struct's field types (e.g. `struct
/// Pair<A, B> { first: A, second: B }`) correctly distinguish "this
/// field's type is the generic parameter A" from "this field's type is
/// a concrete struct that happens to be named A". Applied once, right
/// after a generic struct/enum's fields are resolved via the ordinary
/// (context-free) `resolve_type`.
fn mark_type_params(ty: Ty, param_names: &[String]) -> Ty {
    match ty {
        Ty::Named(n) if param_names.contains(&n) => Ty::TypeParam(n),
        Ty::Array(inner) => Ty::Array(Box::new(mark_type_params(*inner, param_names))),
        Ty::Tuple(ts) => Ty::Tuple(ts.into_iter().map(|t| mark_type_params(t, param_names)).collect()),
        Ty::Ref(m, inner) => Ty::Ref(m, Box::new(mark_type_params(*inner, param_names))),
        Ty::OptionTy(inner) => Ty::OptionTy(Box::new(mark_type_params(*inner, param_names))),
        Ty::ResultTy(o, e) => Ty::ResultTy(
            Box::new(mark_type_params(*o, param_names)),
            Box::new(mark_type_params(*e, param_names)),
        ),
        Ty::Fn(ps, r) => Ty::Fn(
            ps.into_iter().map(|t| mark_type_params(t, param_names)).collect(),
            Box::new(mark_type_params(*r, param_names)),
        ),
        other => other,
    }
}

#[derive(Debug, Clone)]
pub struct EnumShape {
    pub variants: Vec<(String, Vec<Ty>)>,
    pub generics: Vec<GenericParam>,
}

/// A method signature (params excluding `self`, return type). Used for
/// both trait-declared methods and concrete `impl`-provided methods.
#[derive(Debug, Clone)]
pub struct FnSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
    /// `true` if this is a trait method with a default body (Document 7
    /// §4.3) — such methods are *not* required to be re-implemented by
    /// an `impl`. Always `true` (irrelevant) for `ImplRecord` methods,
    /// which always have a body by construction.
    pub has_default_body: bool,
}

#[derive(Debug, Clone)]
pub struct TraitShape {
    pub methods: HashMap<String, FnSig>,
}

/// One `impl` block's provided methods for a given target type. `Vec`
/// (not a single record) because a type can have both an inherent impl
/// and multiple trait impls, all contributing callable methods.
#[derive(Debug, Clone)]
pub struct ImplRecord {
    pub trait_name: Option<String>,
    pub methods: HashMap<String, FnSig>,
}

pub struct TypeChecker {
    structs: HashMap<String, StructShape>,
    enums: HashMap<String, EnumShape>,
    /// Registered `fn` signatures (Document 5 §5: params/return type
    /// are always explicit, so these are always fully known) — enables
    /// Document 5 rule 3 (contextual inference) to work through real
    /// function calls, e.g. `fn setAge(age: u8) {...} setAge(25);`
    /// (Document 5 §4's own example) correctly infers `25: u8`.
    /// Extended in Phase 5 to also carry generic parameters and
    /// where-clause bounds (Document 8 §2/§3), so a call to a generic
    /// function can infer its type parameters from argument types and
    /// verify the declared bounds are actually satisfied.
    functions: HashMap<String, FunctionShape>,
    traits: HashMap<String, TraitShape>,
    impls_by_type: HashMap<String, Vec<ImplRecord>>,
    pub errors: Vec<TypeError>,
}

#[derive(Debug, Clone)]
pub struct FunctionShape {
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub generics: Vec<GenericParam>,
    /// Flattened from `ast::WhereClause`: type-param name -> the list of
    /// trait names it must satisfy (Document 8 §3's `T: A + B` becomes
    /// `["A", "B"]`). Trait-bound generic *arguments* (e.g. a bound
    /// written `T: Container<i32>`) aren't tracked — Document 8's own
    /// examples (`T: Comparable`, `T: Serializable + Comparable + Clone`)
    /// never show a parameterized bound, so nothing broader is invented.
    pub bounds: Vec<(String, Vec<String>)>,
}

/// Local variable environment: a stack of scopes (blocks introduce a
/// new scope; `let` bindings add to the innermost one).
struct Env {
    scopes: Vec<HashMap<String, Ty>>,
}
impl Env {
    fn new() -> Self { Env { scopes: vec![HashMap::new()] } }
    fn push(&mut self) { self.scopes.push(HashMap::new()); }
    fn pop(&mut self) { self.scopes.pop(); }
    fn insert(&mut self, name: String, ty: Ty) {
        self.scopes.last_mut().unwrap().insert(name, ty);
    }
    fn get(&self, name: &str) -> Option<&Ty> {
        for scope in self.scopes.iter().rev() {
            if let Some(t) = scope.get(name) {
                return Some(t);
            }
        }
        None
    }
}

impl TypeChecker {
    pub fn new() -> Self {
        TypeChecker {
            structs: HashMap::new(),
            enums: HashMap::new(),
            functions: HashMap::new(),
            traits: HashMap::new(),
            impls_by_type: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// Context-aware type resolution for positions where a user writes
    /// an explicit type annotation on an *expression* (a `let` binding,
    /// a cast target) — as opposed to the plain, context-free
    /// `resolve_type` free function used for struct/fn *declaration*
    /// registration. This is where Document 8 §8's `Matrix<f64, 2, 3>`
    /// actually needs full validation: arg count against the struct's
    /// declared generic parameters, and arg *kind* (a type argument
    /// where a type parameter is declared, a const value where a const
    /// parameter is declared).
    ///
    /// Scoped deliberately narrow rather than replacing `resolve_type`
    /// everywhere: struct/enum FIELD types and function PARAMETER type
    /// *registration* still use the plain function (so e.g. `fn
    /// foo(m: Matrix<f64,2,3>)`'s parameter type is registered as
    /// `Ty::Named("Matrix")`, args discarded, not validated) — flagged
    /// as a known scope boundary in PROGRESS.md rather than silently
    /// pretending full coverage, given the concrete exit-criteria
    /// example (Document 8 §9) is phrased as a direct expression, which
    /// this covers.
    fn resolve_type_full(&mut self, t: &Type, ctx: &str) -> Ty {
        if let Type::Named(name, args) = t {
            if !args.is_empty() {
                let generics = self.structs.get(name).map(|s| s.generics.clone())
                    .or_else(|| self.enums.get(name).map(|e| e.generics.clone()));
                if let Some(generics) = generics {
                    if args.len() != generics.len() {
                        self.errors.push(TypeError {
                            message: format!(
                                "`{}` expects {} generic argument(s), found {}",
                                name, generics.len(), args.len()
                            ),
                            context: ctx.into(),
                        });
                        return Ty::Named(name.clone());
                    }
                    let mut resolved = Vec::new();
                    let mut kind_error = false;
                    for (param, arg) in generics.iter().zip(args.iter()) {
                        match (param, arg) {
                            (GenericParam::Type { .. }, Type::ConstArg(_)) => {
                                self.errors.push(TypeError {
                                    message: format!("`{}`: expected a type argument, found a const value", name),
                                    context: ctx.into(),
                                });
                                kind_error = true;
                            }
                            (GenericParam::Type { .. }, ty) => {
                                resolved.push(GenericArg::Type(self.resolve_type_full(ty, ctx)));
                            }
                            (GenericParam::Const { .. }, Type::ConstArg(expr)) => {
                                match const_int_value(expr) {
                                    Some(n) => resolved.push(GenericArg::Const(n)),
                                    None => {
                                        self.errors.push(TypeError {
                                            message: format!("`{}`: const-generic argument must be an integer literal", name),
                                            context: ctx.into(),
                                        });
                                        kind_error = true;
                                    }
                                }
                            }
                            (GenericParam::Const { .. }, _) => {
                                self.errors.push(TypeError {
                                    message: format!("`{}`: expected a const value argument, found a type", name),
                                    context: ctx.into(),
                                });
                                kind_error = true;
                            }
                        }
                    }
                    if kind_error {
                        return Ty::Named(name.clone());
                    }
                    return Ty::Generic(name.clone(), resolved);
                }
            }
        }
        resolve_type(t)
    }

    pub fn check_program(&mut self, program: &Program) {
        // Pass 1: register all struct/enum/trait data shapes and fn
        // signatures first, so forward references (a function defined
        // before a struct it uses, or mutual struct references) resolve
        // regardless of declaration order.
        for item in &program.items {
            self.register_item(item);
        }
        // Pass 2: register and validate every `impl` block -- run only
        // after pass 1 so a trait declared textually *after* its impl
        // still resolves correctly, and so the orphan-rule / required-
        // method-completeness checks have the full struct/enum/trait
        // picture available regardless of declaration order.
        for item in &program.items {
            self.register_impls(item);
        }
        // Pass 3: type-check every function body (can now resolve
        // method calls via `impls_by_type`/`traits`).
        for item in &program.items {
            self.check_item(item);
        }
    }

    fn register_item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Struct(s) => {
                let generics = s.generics.0.clone();
                let type_param_names: Vec<String> = generics.iter().filter_map(|g| match g {
                    GenericParam::Type { name, .. } => Some(name.clone()),
                    GenericParam::Const { .. } => None,
                }).collect();
                let (fields, is_tuple) = match &s.body {
                    StructBody::Named(fs) => (
                        fs.iter().map(|f| (f.name.clone(), mark_type_params(resolve_type(&f.ty), &type_param_names))).collect(),
                        false,
                    ),
                    StructBody::Tuple(ts) => (
                        ts.iter().enumerate().map(|(i, t)| (i.to_string(), mark_type_params(resolve_type(t), &type_param_names))).collect(),
                        true,
                    ),
                    StructBody::Unit => (Vec::new(), false),
                };
                self.structs.insert(s.name.clone(), StructShape { fields, is_tuple, generics });
            }
            ItemKind::Enum(e) => {
                let generics = e.generics.0.clone();
                let type_param_names: Vec<String> = generics.iter().filter_map(|g| match g {
                    GenericParam::Type { name, .. } => Some(name.clone()),
                    GenericParam::Const { .. } => None,
                }).collect();
                let variants = e.variants.iter()
                    .map(|v| (v.name.clone(), v.data.iter().map(|t| mark_type_params(resolve_type(t), &type_param_names)).collect()))
                    .collect();
                self.enums.insert(e.name.clone(), EnumShape { variants, generics });
            }
            ItemKind::Fn(f) => {
                let generics = f.generics.0.clone();
                let type_param_names: Vec<String> = generics.iter().filter_map(|g| match g {
                    GenericParam::Type { name, .. } => Some(name.clone()),
                    GenericParam::Const { .. } => None,
                }).collect();
                let params = f.params.iter()
                    .filter(|p| p.name != "self")
                    .map(|p| mark_type_params(resolve_type(&p.ty), &type_param_names))
                    .collect();
                let ret = f.return_type.as_ref()
                    .map(|t| mark_type_params(resolve_type(t), &type_param_names))
                    .unwrap_or(Ty::Unit);
                let bounds = f.where_clause.0.iter()
                    .map(|wb| (wb.name.clone(), wb.bounds.iter().map(|tb| tb.name.clone()).collect()))
                    .collect();
                self.functions.insert(f.name.clone(), FunctionShape { params, ret, generics, bounds });
            }
            ItemKind::Trait(t) => {
                let methods = t.items.iter().filter_map(|ti| match ti {
                    TraitItem::Fn(f) => {
                        let params = f.params.iter()
                            .filter(|p| p.name != "self")
                            .map(|p| resolve_type(&p.ty))
                            .collect();
                        let ret = f.return_type.as_ref().map(resolve_type).unwrap_or(Ty::Unit);
                        Some((f.name.clone(), FnSig { params, ret, has_default_body: f.body.is_some() }))
                    }
                    TraitItem::AssocType(_) => None,
                }).collect();
                self.traits.insert(t.name.clone(), TraitShape { methods });
            }
            ItemKind::Mod(m) => {
                for inner in &m.items {
                    self.register_item(inner);
                }
            }
            _ => {}
        }
    }

    /// Pass 2: register every `impl` block's methods into
    /// `impls_by_type`, and validate the two things Document 25 §2.3's
    /// exit criteria requires for this phase: the orphan-rule check
    /// (Document 7 §5) and required-trait-method completeness
    /// (Document 7 §4.2/§4.3 — every non-default trait method must be
    /// provided).
    fn register_impls(&mut self, item: &Item) {
        if let ItemKind::Impl(impl_decl) = &item.kind {
            let target_name = impl_decl.target.name.clone();
            let trait_name = impl_decl.trait_ref.as_ref().map(|tr| tr.name.clone());
            // Same `Self`-substitution as `check_fn` (see its doc
            // comment), applied here too so a *registered* method
            // signature (what `resolve_method` hands back to a real
            // call site) has `Self` resolved to the concrete target
            // type, not just the body-checking pass. Consistent with
            // `check_item`'s `ItemKind::Impl` branch, which builds the
            // same `Ty::Named(target.name)` self-type — generic impl
            // targets aren't handled at all yet (Phase 4/5 doesn't
            // consult `impl_decl.generics` anywhere), so this is exactly
            // as simplified as the rest of the impl-checking pipeline
            // already is, not a new gap introduced here.
            let self_ty = Ty::Named(target_name.clone());
            let self_subst: HashMap<String, Ty> = std::iter::once(("Self".to_string(), self_ty)).collect();
            let resolve_with_self = |ty: &Type| -> Ty {
                substitute_type_params(&mark_type_params(resolve_type(ty), &["Self".to_string()]), &self_subst)
            };

            let methods: HashMap<String, FnSig> = impl_decl.items.iter().filter_map(|ii| match ii {
                ImplItem::Fn(f) => {
                    let params = f.params.iter()
                        .filter(|p| p.name != "self")
                        .map(|p| resolve_with_self(&p.ty))
                        .collect();
                    let ret = f.return_type.as_ref().map(|t| resolve_with_self(t)).unwrap_or(Ty::Unit);
                    Some((f.name.clone(), FnSig { params, ret, has_default_body: true }))
                }
                ImplItem::AssocType(..) => None,
            }).collect();

            if let Some(tn) = &trait_name {
                // Document 7 §5's orphan rule, grounded via Document 15
                // §3.2's `use`/`import` distinction: Phase 4 doesn't have
                // the real module/package system yet (Document 15 is
                // Phase 14), so "defined in the current package" is
                // approximated here as "declared locally in this
                // Program" (a `trait`/`struct`/`enum` item actually
                // present) -- anything not locally declared is treated
                // as foreign, consistent with how `import` (cross-
                // package) vs `use` (same-package) already distinguish
                // these in the parsed AST. Flagged for sign-off, same as
                // every other spec-grounded extension in prior phases.
                let trait_is_local = self.traits.contains_key(tn);
                let type_is_local = self.structs.contains_key(&target_name) || self.enums.contains_key(&target_name);
                if !trait_is_local && !type_is_local {
                    self.errors.push(TypeError {
                        message: format!(
                            "orphan rule violation: `impl {} for {}` is not allowed -- neither the trait nor the type is defined in the current package (Document 7 §5)",
                            tn, target_name
                        ),
                        context: format!("impl {} for {}", tn, target_name),
                    });
                }

                // Required-method completeness -- only checkable when
                // the trait itself is locally declared (a foreign
                // trait's full method list isn't known to this checker).
                if let Some(shape) = self.traits.get(tn).cloned() {
                    for (mname, sig) in &shape.methods {
                        if !sig.has_default_body && !methods.contains_key(mname) {
                            self.errors.push(TypeError {
                                message: format!(
                                    "`impl {} for {}` is missing required method `{}` (Document 7 §4.2/§4.3)",
                                    tn, target_name, mname
                                ),
                                context: format!("impl {} for {}", tn, target_name),
                            });
                        }
                    }
                }
            }

            self.impls_by_type.entry(target_name).or_default().push(ImplRecord { trait_name, methods });
        } else if let ItemKind::Mod(m) = &item.kind {
            for inner in &m.items {
                self.register_impls(inner);
            }
        }
    }

    fn check_item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(f) => self.check_fn(f, None),
            ItemKind::Impl(impl_decl) => {
                let self_ty = Ty::Named(impl_decl.target.name.clone());
                for ii in &impl_decl.items {
                    if let ImplItem::Fn(f) = ii {
                        self.check_fn(f, Some(&self_ty));
                    }
                }
            }
            ItemKind::Mod(m) => {
                for inner in &m.items {
                    self.check_item(inner);
                }
            }
            _ => {}
        }
    }

    fn check_fn(&mut self, f: &FnDecl, self_ty: Option<&Ty>) {
        let Some(body) = &f.body else { return };
        let mut env = Env::new();
        // When checking a method body (`self_ty` is `Some`), any
        // parameter/return type written as `Self` (Document 7 §4.1's
        // own canonical trait-method example, `other: borrow Self`)
        // must resolve to the enclosing `impl`'s concrete target type,
        // not stay as an unresolved `Ty::Named("Self")` placeholder —
        // the three Phase 5 test failures this fixes specifically
        // needed `compareTo`'s `Self` parameter to resolve to `i32`
        // inside `impl Comparable for i32`. Reuses the existing
        // mark-then-substitute pipeline already built for ordinary
        // generic type parameters (`mark_type_params` +
        // `substitute_type_params`) rather than writing a separate
        // substitution mechanism — "Self" is treated as if it were a
        // one-element generic parameter list scoped to this single
        // function-checking call, which is structurally exactly what
        // it is.
        let self_subst: HashMap<String, Ty> = match self_ty {
            Some(t) => std::iter::once(("Self".to_string(), t.clone())).collect(),
            None => HashMap::new(),
        };
        let resolve_with_self = |ty: &Type| -> Ty {
            let raw = resolve_type(ty);
            if self_ty.is_some() {
                substitute_type_params(&mark_type_params(raw, &["Self".to_string()]), &self_subst)
            } else {
                raw
            }
        };
        for p in &f.params {
            if p.name == "self" {
                if let Some(t) = self_ty {
                    env.insert("self".to_string(), t.clone());
                }
                continue;
            }
            env.insert(p.name.clone(), resolve_with_self(&p.ty));
        }
        let expected_ret = f.return_type.as_ref().map(|t| resolve_with_self(t));
        self.check_block(body, &mut env, expected_ret.as_ref(), &f.name);
    }

    fn check_block(&mut self, block: &Block, env: &mut Env, expected_tail: Option<&Ty>, ctx: &str) -> Option<Ty> {
        env.push();
        for stmt in &block.stmts {
            self.check_stmt(stmt, env, ctx);
        }
        let result = if let Some(tail) = &block.tail {
            Some(self.check_expr(tail, expected_tail, env, ctx))
        } else {
            None
        };
        env.pop();
        result
    }

    fn check_stmt(&mut self, stmt: &Stmt, env: &mut Env, ctx: &str) {
        match stmt {
            Stmt::Let { pattern, ty, value, .. } => {
                let expected = ty.as_ref().map(|t| self.resolve_type_full(t, ctx));
                let inferred = match value {
                    Some(v) => Some(self.check_expr(v, expected.as_ref(), env, ctx)),
                    None => None,
                };
                // Rule 1: explicit annotation wins -- if both an
                // annotation and a value are present, the binding's
                // type is the annotation (already checked-against
                // above); if only a value is present, use its inferred
                // type; if neither, this is an error (nothing to infer
                // from) -- Document 5 doesn't show a bare `let x;` with
                // no type and no value anywhere, so this is treated as
                // an ambiguity error per rule 6's spirit rather than
                // inventing a default.
                let final_ty = match (expected, inferred) {
                    (Some(t), _) => t,
                    (None, Some(t)) => t,
                    (None, None) => {
                        self.errors.push(TypeError {
                            message: "cannot infer type: no annotation and no initializer".into(),
                            context: ctx.into(),
                        });
                        Ty::Unit
                    }
                };
                if let Pattern::Ident(name) = pattern {
                    env.insert(name.clone(), final_ty);
                } else if let Pattern::Mut(name) = pattern {
                    env.insert(name.clone(), final_ty);
                }
                // Other pattern shapes (tuple/array/tuple-struct
                // destructuring) would need per-field type distribution;
                // Phase 3 scope is the core type system, not full
                // pattern-type distribution -- flagged as a known gap
                // rather than silently mishandled (bindings introduced
                // by a destructuring `let` simply aren't added to `env`
                // yet, so using them later would report "undefined
                // variable" rather than a wrong type -- a safe failure
                // mode, not a silent wrong answer).
            }
            Stmt::Expr(e) => {
                self.check_expr(e, None, env, ctx);
            }
            Stmt::Return(_) | Stmt::Item(_) | Stmt::Break { .. }
            | Stmt::Continue { .. } | Stmt::Yield(_) | Stmt::TargetBlock(..) => {
                // Return-value-vs-fn-return-type checking, nested items,
                // and loop control-flow aren't part of Phase 3's core
                // scope (Document 25 §2.3 scopes this phase to the type
                // system + struct/enum data shapes); not silently
                // "checked and passed" -- simply not yet visited.
            }
        }
    }

    /// Bidirectional check: if `expected` is `Some`, verify the
    /// expression's type is compatible with it (Document 5 rule 1/3);
    /// if `None`, infer a type from the expression alone (defaulting
    /// per rule 2 where relevant). Always returns *some* `Ty` so
    /// callers can keep walking, even after recording an error — errors
    /// are accumulated in `self.errors`, not used to abort the whole
    /// pass, consistent with the error-recovery philosophy carried
    /// through every phase so far.
    /// Bidirectional check, entry point: computes the expression's
    /// actual type via `check_expr_inner`, then performs ONE final,
    /// uniform compatibility check against `expected` here — applying
    /// to every expression kind, not just the ones whose inner logic
    /// happened to check it themselves. This was added after tracing a
    /// real gap: `check_expr_inner`'s `Array`/`Tuple` branches only used
    /// `expected` as a soft hint to guide *element* inference, never
    /// verifying the *whole* result actually matched afterward -- so
    /// `let x: [i32] = [1.5];` would have silently produced
    /// `Array(F64)` (the float literal defaulting per rule 2 since
    /// `i32` isn't a float type) with no error at all, even though `x`
    /// is declared `[i32]`. Rather than add a `check_compatible` call
    /// to every individual branch that needed one, this wraps all of
    /// them once, at the single point every branch already funnels
    /// through.
    fn check_expr(&mut self, expr: &Expr, expected: Option<&Ty>, env: &mut Env, ctx: &str) -> Ty {
        let actual = self.check_expr_inner(expr, expected, env, ctx);
        // Only numeric literals and `null` are excluded from this
        // blanket check -- `check_literal`'s `Int`/`IntHex`/`IntOct`/
        // `IntBin`/`Float` arms already adopt-or-default against
        // `expected` per rules 2/3 (so `actual` already equals
        // `expected` whenever that's even possible), and `Null` reports
        // its own specific, clearer error when incompatible. Running the
        // generic check again for those would double-report the same
        // mismatch a second time with a less specific message.
        //
        // `Str`/`RawStr`/`Char`/`Bool` were WRONGLY included in this
        // exclusion in an earlier version of this fix (blanket-excluding
        // all of `Expr::Literal(_)`) -- `check_literal`'s arms for those
        // never compare against `expected` at all, they just return a
        // fixed type unconditionally. That meant `let x: bool = "hi";`
        // or a `dyn`-dispatched call passing `true` where `f64` was
        // expected would have silently type-checked. Caught by hand-
        // tracing this phase's own dyn-dispatch arg-type test before
        // trusting it, not by a later CI failure.
        let self_checking_literal = matches!(expr, Expr::Literal(
            Literal::Int(_) | Literal::IntHex(_) | Literal::IntOct(_)
            | Literal::IntBin(_) | Literal::Float(_) | Literal::Null
        ));
        if !self_checking_literal {
            self.check_compatible(&actual, expected, ctx);
        }
        actual
    }

    fn check_expr_inner(&mut self, expr: &Expr, expected: Option<&Ty>, env: &mut Env, ctx: &str) -> Ty {
        match expr {
            Expr::Literal(lit) => self.check_literal(lit, expected, ctx),

            Expr::Ident(name) => {
                if let Some(t) = env.get(name) {
                    t.clone()
                } else {
                    self.errors.push(TypeError {
                        message: format!("undefined variable `{}`", name),
                        context: ctx.into(),
                    });
                    expected.cloned().unwrap_or(Ty::Unit)
                }
            }

            Expr::Path(path) => {
                // Two-segment path (`EnumName::Variant`) against a known
                // enum, with no call parens -- must be a unit variant
                // (Document 7 §3.1). This is what actually makes the
                // enum registry (`self.enums`) a real consumer of
                // Document 7's data-shape information, not just a
                // write-only registry -- see the module-level note on
                // why this was added rather than left unused.
                if let [enum_name, variant_name] = path.as_slice() {
                    if let Some(shape) = self.enums.get(enum_name) {
                        match shape.variants.iter().find(|(n, _)| n == variant_name) {
                            Some((_, data)) if data.is_empty() => return Ty::Named(enum_name.clone()),
                            Some((_, _)) => {
                                self.errors.push(TypeError {
                                    message: format!(
                                        "enum variant `{}::{}` carries data and must be called with arguments",
                                        enum_name, variant_name
                                    ),
                                    context: ctx.into(),
                                });
                                return Ty::Named(enum_name.clone());
                            }
                            None => {
                                self.errors.push(TypeError {
                                    message: format!("enum `{}` has no variant `{}`", enum_name, variant_name),
                                    context: ctx.into(),
                                });
                                return Ty::Named(enum_name.clone());
                            }
                        }
                    }
                }
                expected.cloned().unwrap_or(Ty::Unit) // module/struct-assoc paths: Phase 4+ resolves these fully
            }

            Expr::Paren(inner) => self.check_expr(inner, expected, env, ctx),

            Expr::Array(items) => {
                let elem_expected = match expected {
                    Some(Ty::Array(inner)) => Some((**inner).clone()),
                    _ => None,
                };
                if items.is_empty() {
                    // Document 5 §4 rule 6, §7's own explicit example:
                    // `let empty = [];` with no further usage is a
                    // compile error requiring an explicit annotation,
                    // not a silent default to `[i32]`.
                    match elem_expected {
                        Some(t) => Ty::Array(Box::new(t)),
                        None => {
                            self.errors.push(TypeError {
                                message: "cannot infer type of empty array literal `[]` -- add an explicit type annotation".into(),
                                context: ctx.into(),
                            });
                            Ty::Array(Box::new(Ty::Unit))
                        }
                    }
                } else {
                    let first = self.check_expr(&items[0], elem_expected.as_ref(), env, ctx);
                    for item in &items[1..] {
                        let t = self.check_expr(item, Some(&first), env, ctx);
                        if t != first {
                            self.errors.push(TypeError {
                                message: format!("array element type mismatch: expected `{}`, found `{}`", first, t),
                                context: ctx.into(),
                            });
                        }
                    }
                    Ty::Array(Box::new(first))
                }
            }

            Expr::Tuple(items) => {
                let expected_elems: Vec<Option<Ty>> = match expected {
                    Some(Ty::Tuple(ts)) if ts.len() == items.len() => ts.iter().cloned().map(Some).collect(),
                    _ => vec![None; items.len()],
                };
                let tys = items.iter().zip(expected_elems)
                    .map(|(it, exp)| self.check_expr(it, exp.as_ref(), env, ctx))
                    .collect();
                Ty::Tuple(tys)
            }

            Expr::StructLit { name, fields, spread } => self.check_struct_lit(name, fields, spread.is_some(), expected, env, ctx),

            Expr::If(if_expr) => self.check_if(if_expr, expected, env, ctx),

            Expr::Match(match_expr) => self.check_match(match_expr, expected, env, ctx),

            Expr::Block(b) => self.check_block(b, env, expected, ctx).unwrap_or(Ty::Unit),

            Expr::Unsafe(b) => self.check_block(b, env, expected, ctx).unwrap_or(Ty::Unit),

            Expr::Unary { op, expr: inner } => {
                let t = self.check_expr(inner, expected, env, ctx);
                match op {
                    UnaryOp::Not => {
                        if t != Ty::Bool {
                            self.errors.push(TypeError {
                                message: format!("`!` requires `bool`, found `{}`", t),
                                context: ctx.into(),
                            });
                        }
                        Ty::Bool
                    }
                    UnaryOp::Neg => {
                        if !ty_is_numeric(&t) {
                            self.errors.push(TypeError {
                                message: format!("unary `-` requires a numeric type, found `{}`", t),
                                context: ctx.into(),
                            });
                        }
                        t
                    }
                    UnaryOp::BitNot => t,
                }
            }

            Expr::Binary { op, lhs, rhs } => self.check_binary(*op, lhs, rhs, env, ctx),

            Expr::Assign { lhs, rhs, .. } => {
                let lt = self.check_expr(lhs, None, env, ctx);
                self.check_expr(rhs, Some(&lt), env, ctx);
                Ty::Unit
            }

            Expr::Cast { expr: inner, ty } => {
                // Document 4 §6 / Document 5 §6: `as` is the explicit
                // escape hatch from the no-implicit-coercion rule --
                // Phase 3 doesn't validate which primitive-to-primitive
                // casts are semantically legal (that's a Document 4 §6
                // detail beyond this phase's core-type-system scope),
                // only that `as` itself always type-checks to its
                // target type regardless of the source expression's type.
                self.check_expr(inner, None, env, ctx);
                self.resolve_type_full(ty, ctx)
            }

            Expr::Range { lo, hi, .. } => {
                let lt = self.check_expr(lo, None, env, ctx);
                self.check_expr(hi, Some(&lt), env, ctx);
                lt
            }

            Expr::Propagate(inner) => self.check_expr(inner, None, env, ctx),

            Expr::Field { expr: inner, name } => {
                let t = self.check_expr(inner, None, env, ctx);
                self.field_type(&t, name, ctx)
            }

            Expr::Index { expr: inner, index } => {
                let t = self.check_expr(inner, None, env, ctx);
                self.check_expr(index, None, env, ctx);
                match t {
                    Ty::Array(elem) => *elem,
                    other => {
                        self.errors.push(TypeError {
                            message: format!("cannot index into type `{}`", other),
                            context: ctx.into(),
                        });
                        Ty::Unit
                    }
                }
            }

            Expr::Call { callee, args } => {
                if let Expr::Ident(name) = callee.as_ref() {
                    if let Some(shape) = self.functions.get(name).cloned() {
                        if shape.generics.is_empty() {
                            for (i, arg) in args.iter().enumerate() {
                                let exp = shape.params.get(i);
                                self.check_expr(&arg.value, exp, env, ctx);
                            }
                            return shape.ret;
                        }
                        // Generic function call (Document 8 §2): infer
                        // each type parameter from the actual argument
                        // types via structural unification against the
                        // (TypeParam-marked) declared parameter types,
                        // then verify every `where`-clause bound
                        // (§3's `T: A + B`) against the *concrete*
                        // inferred type using Phase 4's impl registry.
                        // Substituting a concrete `Ty` here (never a
                        // `Ty::DynTrait`) is what makes this resolve
                        // through static/monomorphized dispatch, not
                        // the `dyn` vtable machinery Phase 4 built for
                        // the unrelated `dyn Trait` case — this phase's
                        // "zero dyn dispatch for generic-only code"
                        // exit criterion holds by construction, not by
                        // a separate runtime check (there is no runtime
                        // yet); see PROGRESS.md's verification section
                        // for the concrete test that confirms it.
                        let arg_tys: Vec<Ty> = args.iter()
                            .map(|a| self.check_expr(&a.value, None, env, ctx))
                            .collect();
                        let mut subst: HashMap<String, Ty> = HashMap::new();
                        for (p, a) in shape.params.iter().zip(arg_tys.iter()) {
                            unify_infer(p, a, &mut subst);
                        }
                        for (param_name, trait_names) in &shape.bounds {
                            if let Some(concrete) = subst.get(param_name) {
                                for tn in trait_names {
                                    if !self.satisfies_bound(concrete, tn) {
                                        self.errors.push(TypeError {
                                            message: format!(
                                                "type `{}` does not satisfy bound `{}` required for generic parameter `{}` of `{}` (Document 8 §2/§3)",
                                                concrete, tn, param_name, name
                                            ),
                                            context: ctx.into(),
                                        });
                                    }
                                }
                            }
                        }
                        return substitute_type_params(&shape.ret, &subst);
                    }
                }
                if let Expr::Path(path) = callee.as_ref() {
                    if let [enum_name, variant_name] = path.as_slice() {
                        if let Some(shape) = self.enums.get(enum_name).cloned() {
                            match shape.variants.iter().find(|(n, _)| n == variant_name) {
                                Some((_, data_tys)) => {
                                    for (i, arg) in args.iter().enumerate() {
                                        let exp = data_tys.get(i);
                                        self.check_expr(&arg.value, exp, env, ctx);
                                    }
                                    if args.len() != data_tys.len() {
                                        self.errors.push(TypeError {
                                            message: format!(
                                                "`{}::{}` expects {} argument(s), found {}",
                                                enum_name, variant_name, data_tys.len(), args.len()
                                            ),
                                            context: ctx.into(),
                                        });
                                    }
                                    return Ty::Named(enum_name.clone());
                                }
                                None => {
                                    self.errors.push(TypeError {
                                        message: format!("enum `{}` has no variant `{}`", enum_name, variant_name),
                                        context: ctx.into(),
                                    });
                                    return Ty::Named(enum_name.clone());
                                }
                            }
                        }
                    }
                }
                for arg in args {
                    self.check_expr(&arg.value, None, env, ctx);
                }
                expected.cloned().unwrap_or(Ty::Unit)
            }

            Expr::MethodCall { receiver, name, args } => {
                let recv_ty = self.check_expr(receiver, None, env, ctx);
                let sig = self.resolve_method(&recv_ty, name);
                match sig {
                    Some(sig) => {
                        for (i, arg) in args.iter().enumerate() {
                            self.check_expr(&arg.value, sig.params.get(i), env, ctx);
                        }
                        if args.len() != sig.params.len() {
                            self.errors.push(TypeError {
                                message: format!(
                                    "method `{}` on `{}` expects {} argument(s), found {}",
                                    name, recv_ty, sig.params.len(), args.len()
                                ),
                                context: ctx.into(),
                            });
                        }
                        sig.ret
                    }
                    None => {
                        for arg in args {
                            self.check_expr(&arg.value, None, env, ctx);
                        }
                        // Only report "no such method" when the receiver
                        // is a type this checker actually has full
                        // knowledge of (a locally-declared struct/enum,
                        // or a dyn-Trait whose trait is locally
                        // declared) -- for anything else (foreign types,
                        // primitives, stdlib collections with no
                        // registered methods yet) Phase 4 doesn't have
                        // enough information to say the method doesn't
                        // exist, so it stays silent rather than
                        // fabricating a false error, consistent with
                        // Phase 3's existing field-access behavior for
                        // unknown base types.
                        let known_base = match &recv_ty {
                            Ty::Named(n) => self.structs.contains_key(n) || self.enums.contains_key(n),
                            Ty::Generic(n, _) => self.structs.contains_key(n) || self.enums.contains_key(n),
                            Ty::DynTrait(n) => self.traits.contains_key(n),
                            _ => false,
                        };
                        if known_base {
                            self.errors.push(TypeError {
                                message: format!("no method named `{}` found for type `{}`", name, recv_ty),
                                context: ctx.into(),
                            });
                        }
                        expected.cloned().unwrap_or(Ty::Unit)
                    }
                }
            }

            Expr::Borrow { expr: inner, mutable } => {
                let inner_expected = match expected {
                    Some(Ty::Ref(_, t)) => Some((**t).clone()),
                    _ => None,
                };
                let t = self.check_expr(inner, inner_expected.as_ref(), env, ctx);
                Ty::Ref(*mutable, Box::new(t))
            }

            Expr::Await(inner) | Expr::Throw(inner) => {
                self.check_expr(inner, None, env, ctx);
                expected.cloned().unwrap_or(Ty::Unit)
            }

            Expr::Return(inner) => {
                if let Some(e) = inner {
                    self.check_expr(e, None, env, ctx);
                }
                Ty::Never
            }

            Expr::Yield(inner) => {
                self.check_expr(inner, None, env, ctx);
                Ty::Unit
            }

            // Constructs whose full semantic checking depends on later
            // phases (closures need capture/borrow analysis, Phase 6;
            // spawn/select/query/loops need their own domain checkers,
            // Phase 12/18/20/8) -- visited for sub-expression coverage
            // where cheap, but not fully type-checked here. Not silently
            // claimed as "checked".
            Expr::Closure(c) => {
                let mut inner_env = Env::new();
                inner_env.scopes = env.scopes.clone();
                inner_env.push();
                for (pat, ty) in &c.params {
                    if let Pattern::Ident(n) = pat {
                        inner_env.insert(n.clone(), ty.as_ref().map(resolve_type).unwrap_or(Ty::Unit));
                    }
                }
                match &c.body {
                    ClosureBody::Expr(e) => { self.check_expr(e, None, &mut inner_env, ctx); }
                    ClosureBody::Block(b) => { self.check_block(b, &mut inner_env, None, ctx); }
                }
                expected.cloned().unwrap_or(Ty::Unit)
            }
            Expr::Loop(_) | Expr::Spawn { .. } | Expr::Select(_) | Expr::Query(_)
            | Expr::TryCatch { .. } | Expr::Styled { .. } | Expr::Layout { .. }
            | Expr::ComponentChildren { .. } | Expr::EventHandler { .. } => {
                expected.cloned().unwrap_or(Ty::Unit)
            }
        }
    }

    fn check_literal(&mut self, lit: &Literal, expected: Option<&Ty>, ctx: &str) -> Ty {
        match lit {
            Literal::Int(_) | Literal::IntHex(_) | Literal::IntOct(_) | Literal::IntBin(_) => {
                match expected {
                    // Rule 3: contextual/bidirectional inference -- an
                    // integer literal adopts a required numeric type.
                    Some(t) if ty_is_numeric(t) => t.clone(),
                    // Rule 2: literal defaulting -- untyped integer
                    // literal defaults to i32.
                    _ => Ty::I32,
                }
            }
            Literal::Float(_) => {
                match expected {
                    Some(t) if ty_is_float(t) => t.clone(),
                    _ => Ty::F64,
                }
            }
            Literal::Str(_) | Literal::RawStr(_) => Ty::StringTy,
            Literal::Char(_) => Ty::Char,
            Literal::Bool(_) => Ty::Bool,
            Literal::Null => {
                // Document 5 §2.6: `null` is only valid in unsafe/FFI
                // contexts -- Phase 3 doesn't yet track "is this
                // specific literal lexically inside an unsafe block"
                // as a separate flag threaded through `check_expr`
                // (that plumbing is straightforward but not added this
                // phase); instead, the concrete rule actually tested by
                // Document 5 §7 -- "`null` is not usable where an
                // `Option<T>` or plain `T` is expected in safe code" --
                // is enforced directly here: using `null` against any
                // expected type other than `Ty::Null` itself is an
                // error unconditionally, which correctly rejects every
                // safe-code use shown anywhere in Documents 1-24. This
                // is stricter than the full "safe code" carve-out (it
                // would also flag a hypothetical `unsafe { let x: i32 =
                // null; }`, which the language spec does intend to
                // permit) -- flagged as a known simplification, not
                // silently treated as complete unsafe-context support.
                if let Some(t) = expected {
                    if *t != Ty::Null {
                        self.errors.push(TypeError {
                            message: format!("`null` is not usable where `{}` is expected -- use `Option<T>` in safe code (Document 5 §2.6)", t),
                            context: ctx.into(),
                        });
                    }
                }
                Ty::Null
            }
        }
    }

    fn check_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, env: &mut Env, ctx: &str) -> Ty {
        let lt = self.check_expr(lhs, None, env, ctx);
        // For most arithmetic, passing `Some(&lt)` as rhs's expected
        // type lets a literal rhs (`x + 5`) adopt lt's type (rule 3).
        // But for generic-struct operands, a differently-shaped-but-
        // still-compatible rhs (`Matrix<f64,3,5>` against a
        // `Matrix<f64,2,3>` lhs, Document 8 §9's own valid case) is
        // exactly what needs to be *accepted* here -- passing `Some(&lt)`
        // would make the outer `check_expr` wrapper's blanket
        // compatibility check reject it before this function's own
        // dimension-aware logic below ever runs. So: only use `lt` as
        // an rhs hint when it isn't a generic-struct type.
        let rhs_hint = if matches!(lt, Ty::Generic(..)) { None } else { Some(&lt) };
        let rt = self.check_expr(rhs, rhs_hint, env, ctx);
        use BinaryOp::*;
        match op {
            Mul => {
                // Document 8 §8/§9: const-generic matrix multiplication
                // dimension check, when both operands are the same
                // generic struct. See `check_matrix_multiply`'s doc
                // comment for the scope/grounding of this special case.
                if let (Ty::Generic(ln, largs), Ty::Generic(rn, rargs)) = (&lt, &rt) {
                    if ln == rn {
                        let (ln, largs, rargs) = (ln.clone(), largs.clone(), rargs.clone());
                        return self.check_matrix_multiply(&ln, &largs, &rargs, ctx);
                    }
                }
                if lt != rt {
                    self.errors.push(TypeError {
                        message: format!("`Mul`: mismatched types `{}` and `{}` (Document 5 §6: no implicit coercion, use `as`)", lt, rt),
                        context: ctx.into(),
                    });
                }
                lt
            }
            Add | Sub | Div | Mod | Pow | BitAnd | BitOr | BitXor | Shl | Shr => {
                if lt != rt {
                    self.errors.push(TypeError {
                        message: format!("`{:?}`: mismatched types `{}` and `{}` (Document 5 §6: no implicit coercion, use `as`)", op, lt, rt),
                        context: ctx.into(),
                    });
                }
                lt
            }
            EqEq | NotEq | Lt | Gt | LtEq | GtEq => {
                if lt != rt {
                    self.errors.push(TypeError {
                        message: format!("comparison between mismatched types `{}` and `{}` (Document 5 §6)", lt, rt),
                        context: ctx.into(),
                    });
                }
                Ty::Bool
            }
            AndAnd | OrOr => {
                if lt != Ty::Bool {
                    self.errors.push(TypeError { message: format!("`&&`/`||` requires `bool`, found `{}`", lt), context: ctx.into() });
                }
                if rt != Ty::Bool {
                    self.errors.push(TypeError { message: format!("`&&`/`||` requires `bool`, found `{}`", rt), context: ctx.into() });
                }
                Ty::Bool
            }
            Coalesce => {
                match lt {
                    Ty::OptionTy(inner) => *inner,
                    other => other,
                }
            }
        }
    }

    /// Document 8 §8/§9's const-generic matrix-multiplication dimension
    /// check: `Matrix<f64,2,3> * Matrix<f64,4,5>` must be rejected
    /// (3≠4), while `Matrix<f64,2,3> * Matrix<f64,3,5>` must be
    /// accepted and produce `Matrix<f64,2,5>`.
    ///
    /// FLAGGED, SCOPED INTERPRETATION: Mountain's spec doesn't define
    /// any general operator-overloading mechanism anywhere in Documents
    /// 1–24 (no trait-based `Mul` dispatch is described), so this is
    /// implemented as a direct, special-cased rule for `*` between two
    /// instances of the *same* generic struct carrying exactly two
    /// const-generic arguments — matching Document 8 §8's own
    /// `Matrix<T, const ROWS: usize, const COLS: usize>` declaration
    /// shape exactly, with the two const positions read positionally
    /// (first = rows-analog, second = cols-analog) per that
    /// declaration's literal parameter order. This is grounded directly
    /// in Document 8's own concrete struct declaration and §9's
    /// verification trace, not invented from nothing — but it is a
    /// narrow, name-and-shape-triggered rule, not a general arithmetic-
    /// on-generics mechanism. Needs explicit sign-off, same as every
    /// other flagged deviation.
    fn check_matrix_multiply(&mut self, name: &str, largs: &[GenericArg], rargs: &[GenericArg], ctx: &str) -> Ty {
        let const_values = |args: &[GenericArg]| -> Vec<i64> {
            args.iter().filter_map(|a| match a { GenericArg::Const(n) => Some(*n), _ => None }).collect()
        };
        let lconsts = const_values(largs);
        let rconsts = const_values(rargs);
        if lconsts.len() == 2 && rconsts.len() == 2 {
            let (l_rows, l_cols) = (lconsts[0], lconsts[1]);
            let (r_rows, r_cols) = (rconsts[0], rconsts[1]);
            if l_cols != r_rows {
                self.errors.push(TypeError {
                    message: format!(
                        "cannot multiply `{}` with column count {} by `{}` with row count {}: dimension mismatch (Document 8 §9)",
                        name, l_cols, name, r_rows
                    ),
                    context: ctx.into(),
                });
                return Ty::Generic(name.to_string(), largs.to_vec());
            }
            // Result shape is (l_rows, r_cols); rewrite the two const
            // positions in a copy of `largs` (which also carries the
            // element type argument in whichever position it declared),
            // preserving every non-const argument's position untouched.
            let mut result_args = largs.to_vec();
            let mut const_idx = 0;
            for a in result_args.iter_mut() {
                if let GenericArg::Const(n) = a {
                    *n = if const_idx == 0 { l_rows } else { r_cols };
                    const_idx += 1;
                }
            }
            return Ty::Generic(name.to_string(), result_args);
        }
        // Same struct name, but not this 2-const-param shape -- fall
        // back to ordinary strict equality of the full argument list.
        if largs != rargs {
            self.errors.push(TypeError {
                message: format!("`*`: mismatched generic arguments for `{}`", name),
                context: ctx.into(),
            });
        }
        Ty::Generic(name.to_string(), largs.to_vec())
    }

    fn check_compatible(&mut self, actual: &Ty, expected: Option<&Ty>, ctx: &str) {
        if let Some(exp) = expected {
            if actual != exp {
                self.errors.push(TypeError {
                    message: format!("expected `{}`, found `{}` (Document 5 §6: no implicit coercion, use `as`)", exp, actual),
                    context: ctx.into(),
                });
            }
        }
    }

    fn check_if(&mut self, if_expr: &IfExpr, expected: Option<&Ty>, env: &mut Env, ctx: &str) -> Ty {
        self.check_expr(&if_expr.cond, Some(&Ty::Bool), env, ctx);
        let then_ty = self.check_block(&if_expr.then_block, env, expected, ctx).unwrap_or(Ty::Unit);
        match &if_expr.else_branch {
            Some(ElseBranch::Block(b)) => {
                let else_ty = self.check_block(b, env, Some(&then_ty), ctx).unwrap_or(Ty::Unit);
                // Rule 5: no cross-branch guessing -- both arms of an
                // if-expression must agree; the compiler never
                // synthesizes a union/common-supertype.
                if else_ty != then_ty {
                    self.errors.push(TypeError {
                        message: format!(
                            "`if`/`else` branches have incompatible types: `{}` and `{}` (Document 5 rule 5: no cross-branch guessing)",
                            then_ty, else_ty
                        ),
                        context: ctx.into(),
                    });
                }
            }
            Some(ElseBranch::If(inner)) => {
                let else_ty = self.check_if(inner, Some(&then_ty), env, ctx);
                if else_ty != then_ty {
                    self.errors.push(TypeError {
                        message: format!(
                            "`if`/`else if` branches have incompatible types: `{}` and `{}` (Document 5 rule 5)",
                            then_ty, else_ty
                        ),
                        context: ctx.into(),
                    });
                }
            }
            None => {
                // Document 9 §1.2: an `if` without a matching `else`
                // cannot be used as a value-producing expression --
                // Phase 3 doesn't yet distinguish statement-position
                // from expression-position `if` (that requires threading
                // "is this result actually used" through the caller),
                // so this isn't separately enforced yet; flagged rather
                // than silently claimed as checked.
            }
        }
        then_ty
    }

    fn check_match(&mut self, match_expr: &MatchExpr, expected: Option<&Ty>, env: &mut Env, ctx: &str) -> Ty {
        self.check_expr(&match_expr.scrutinee, None, env, ctx);
        let mut common: Option<Ty> = expected.cloned();
        for arm in &match_expr.arms {
            env.push();
            // Pattern-introduced bindings aren't type-distributed against
            // the scrutinee's shape yet (needs enum-variant-field lookup
            // threaded through pattern matching) -- flagged gap, same
            // category as the `let`-destructuring one above.
            if let Some(guard) = &arm.guard {
                self.check_expr(guard, Some(&Ty::Bool), env, ctx);
            }
            let arm_ty = match &arm.body {
                MatchArmBody::Expr(e) => self.check_expr(e, common.as_ref(), env, ctx),
                MatchArmBody::Block(b) => self.check_block(b, env, common.as_ref(), ctx).unwrap_or(Ty::Unit),
            };
            env.pop();
            match &common {
                Some(c) if *c != arm_ty => {
                    // Rule 5 again, for `match`: Document 5 §7's own
                    // explicit verification case (one arm `i32`, another
                    // `String`) -- rejected, not unioned.
                    self.errors.push(TypeError {
                        message: format!(
                            "`match` arms have incompatible types: `{}` and `{}` (Document 5 rule 5: no cross-branch guessing)",
                            c, arm_ty
                        ),
                        context: ctx.into(),
                    });
                }
                None => common = Some(arm_ty),
                _ => {}
            }
        }
        common.unwrap_or(Ty::Unit)
    }

    fn check_struct_lit(&mut self, name: &str, fields: &[(String, Expr)], has_spread: bool, expected: Option<&Ty>, env: &mut Env, ctx: &str) -> Ty {
        let Some(shape) = self.structs.get(name).cloned() else {
            self.errors.push(TypeError {
                message: format!("undefined struct `{}`", name),
                context: ctx.into(),
            });
            return Ty::Named(name.to_string());
        };

        // Document 5 rule 1 (explicit annotation wins), applied to
        // generic struct literals: if `expected` names this same
        // generic struct with concrete args (e.g. `let p: Pair<i32,
        // String> = Pair { first: 1, second: "x" };`), substitute those
        // concrete types for the struct's own `TypeParam` placeholders
        // before checking each field, and use `expected`'s args as the
        // literal's own resolved type. Without a matching annotation,
        // Phase 5 doesn't infer generic args from field values alone
        // (that would need real unification across every field) —
        // falls back to `Ty::Named(name)`, a known, flagged
        // simplification rather than a silent wrong answer.
        let (subst, result_ty) = if !shape.generics.is_empty() {
            match expected {
                Some(Ty::Generic(en, eargs)) if en == name && eargs.len() == shape.generics.len() => {
                    let subst: HashMap<String, Ty> = shape.generics.iter().zip(eargs.iter())
                        .filter_map(|(param, arg)| match (param, arg) {
                            (GenericParam::Type { name, .. }, GenericArg::Type(t)) => Some((name.clone(), t.clone())),
                            _ => None,
                        }).collect();
                    (subst, Ty::Generic(name.to_string(), eargs.clone()))
                }
                _ => (HashMap::new(), Ty::Named(name.to_string())),
            }
        } else {
            (HashMap::new(), Ty::Named(name.to_string()))
        };

        let declared: HashMap<String, Ty> = shape.fields.iter()
            .map(|(n, t)| (n.clone(), substitute_type_params(t, &subst)))
            .collect();
        let mut provided = std::collections::HashSet::new();
        for (fname, fval) in fields {
            provided.insert(fname.as_str());
            match declared.get(fname.as_str()) {
                Some(ft) => { self.check_expr(fval, Some(ft), env, ctx); }
                None => {
                    self.errors.push(TypeError {
                        message: format!("struct `{}` has no field `{}`", name, fname),
                        context: ctx.into(),
                    });
                }
            }
        }
        if !has_spread {
            // Document 7 §2.2: every field must be explicitly
            // initialized unless a `..` spread is present.
            for (fname, _) in &shape.fields {
                if !provided.contains(fname.as_str()) {
                    self.errors.push(TypeError {
                        message: format!("missing field `{}` in struct literal for `{}`", fname, name),
                        context: ctx.into(),
                    });
                }
            }
        }
        result_ty
    }

    /// Resolves a method call against a receiver's static type. Two
    /// dispatch modes, per Document 7 §4.4/§4.5 and this phase's exit
    /// criteria:
    /// - **Static dispatch** (`Ty::Named`): search every `impl` block
    ///   registered for that concrete type (inherent *and* trait impls
    ///   both contribute) for a matching method name. This is what
    ///   "static/monomorphized, zero-cost" dispatch means at the
    ///   type-checking level — the exact concrete implementation is
    ///   known here, at compile time, not deferred to a vtable.
    /// - **Dynamic dispatch** (`Ty::DynTrait`): resolve against the
    ///   *trait's own* declared signature instead of any concrete
    ///   `impl` — correct, because with `dyn Trait` the concrete
    ///   implementing type isn't known until runtime (Document 7 §4.5:
    ///   "resolved via vtable lookup at runtime"); statically, all that
    ///   can be verified is that the trait itself declares a method with
    ///   this name and signature.
    /// Checks whether a concrete type satisfies a named trait bound
    /// (Document 8 §2/§3), by looking up whether any registered `impl`
    /// of that trait exists for the type's canonical name — the same
    /// registry (`impls_by_type`) Phase 4 built, reused here rather
    /// than duplicated. Works for both user-declared structs/enums and
    /// primitives (`impl Comparable for i32` registers under the key
    /// `"i32"`, matching how `parse_type_ref` already lexes primitive
    /// type names as plain identifiers — see Phase 1's design note).
    fn satisfies_bound(&self, ty: &Ty, trait_name: &str) -> bool {
        let Some(key) = ty_lookup_name(ty) else { return false };
        self.impls_by_type.get(&key)
            .map(|impls| impls.iter().any(|r| r.trait_name.as_deref() == Some(trait_name)))
            .unwrap_or(false)
    }

    fn resolve_method(&self, recv_ty: &Ty, name: &str) -> Option<FnSig> {
        match recv_ty {
            Ty::Named(type_name) | Ty::Generic(type_name, _) => {
                let impls = self.impls_by_type.get(type_name)?;
                // Inherent methods (`impl Type { ... }`, no trait) take
                // precedence over trait-provided methods of the same
                // name -- the usual method-resolution order. This is
                // also what makes `ImplRecord::trait_name` an actually-
                // consumed piece of information rather than write-only
                // (caught proactively via `grep -n "\.trait_name\b"`
                // returning nothing before this fix, same discipline as
                // Phase 3's `self.enums` catch).
                impls.iter().find(|r| r.trait_name.is_none())
                    .and_then(|r| r.methods.get(name).cloned())
                    .or_else(|| impls.iter().find_map(|r| r.methods.get(name).cloned()))
            }
            Ty::DynTrait(trait_name) => {
                self.traits.get(trait_name)?.methods.get(name).cloned()
            }
            _ => None,
        }
    }

    fn field_type(&mut self, base: &Ty, field: &str, ctx: &str) -> Ty {
        if let Ty::Named(n) = base {
            if let Some(shape) = self.structs.get(n) {
                for (fname, fty) in &shape.fields {
                    if fname == field {
                        return fty.clone();
                    }
                }
                self.errors.push(TypeError {
                    message: format!("struct `{}` has no field `{}`", n, field),
                    context: ctx.into(),
                });
                return Ty::Unit;
            }
        }
        if let Ty::Generic(n, args) = base {
            if let Some(shape) = self.structs.get(n).cloned() {
                let subst: HashMap<String, Ty> = shape.generics.iter().zip(args.iter())
                    .filter_map(|(param, arg)| match (param, arg) {
                        (GenericParam::Type { name, .. }, GenericArg::Type(t)) => Some((name.clone(), t.clone())),
                        _ => None,
                    }).collect();
                for (fname, fty) in &shape.fields {
                    if fname == field {
                        return substitute_type_params(fty, &subst);
                    }
                }
                self.errors.push(TypeError {
                    message: format!("struct `{}` has no field `{}`", n, field),
                    context: ctx.into(),
                });
                return Ty::Unit;
            }
        }
        // Base type not a known struct (could be a module path result,
        // a foreign/unregistered generic, etc.) -- don't fabricate an
        // error for cases outside this phase's scope.
        Ty::Unit
    }
}

impl Default for TypeChecker {
    fn default() -> Self {
        Self::new()
    }
}
