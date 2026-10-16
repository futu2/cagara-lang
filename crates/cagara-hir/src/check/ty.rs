//! Type representation: [`Ty`], the kind / witness / marker enums,
//! the constraint language, schemes, and the small `Ty` constructors.
//!
//! Split out of the single `check.rs` for readability only: whole items and
//! whole `Checker` methods were moved, and no logic was changed.

use super::*;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ty {
    Var(u32),
    /// Type variable from a signature; unifies only with itself.
    Rigid(u32, String),
    /// A quantified variable of a type scheme (index into `Scheme::gens`).
    /// Schemes use these instead of arena variables, so they stand alone.
    Gen(u32),
    Con(&'static str, Vec<Ty>),
    Fun(Box<Ty>, Box<Ty>),
    /// Fields plus a tail (`Empty`, a variable, or a rigid variable).
    Row(Vec<(String, Ty)>, Box<Ty>),
    Empty,
    /// Key mapping over a row: applies a key transformation to every field
    /// label. The mapper is itself a type-level symbol of kind `KeyMap`: a
    /// `Con` naming a constructor (`KeyMapPrefix "s"`, `KeyMapSuffix "s"`,
    /// `KeyMapCompose f g`, `KeyMapId`), or a *variable* when a signature is
    /// polymorphic in the mapping.
    ///
    /// A variable mapper is what lets `prefix p` be written for a lambda
    /// parameter `p`: the term is retained with the variable in place, and
    /// reduces once the variable is bound to a constructor.
    MapKey(Box<Ty>, Box<Ty>),
    /// The affix of a prefix/suffix mapper: a `Con` head plus the literal
    /// string, since `Con` names are static and an affix is not.
    KeyAffix(&'static str, String),
    /// Merge two rows: right-wins on collision, keeps old field positions,
    /// appends labels that occur only in the right row.
    Merge(Box<Ty>, Box<Ty>),
    /// Value mapping over a row: every field type wrapped by the witness the
    /// label names. The witness is a *label* (`Ty::Sym("nullable")`, say) rather
    /// than a closed `ValueMap`, so it can still be a signature's variable —
    /// `mapValue w r` with `valueLabel w` — and travel in the type exactly as a
    /// `keyMap`'s mapper does.
    MapValue(Box<Ty>, Box<Ty>),
    /// Remove one named field from a row: `removeKey k r`. The label is a type
    /// (normally `Sym`), so a signature's `k` links to the result. It reduces
    /// once the label is a `Sym`, `r` is closed, and `r` contains the label;
    /// the missing-column error is reported by the stage that registered it.
    RemoveKey(Box<Ty>, Box<Ty>),
    /// A string literal used as a type: the type of `"k"` in `symbol k`. It
    /// unifies only with the identical literal, so `symbol k` is an ordinary
    /// `Type` variable that the call site binds to `Sym("k")`.
    Sym(String),
}

/// Key mapping witnesses: closed, first-order transformations on field names.
///
/// A mapper is a type-level symbol of kind `KeyMap` (see [`Kind`]), not a value.
/// The set is *closed* and *finite*: `id`, the two value-carrying constructors
/// `prefix`/`suffix`, and composition. Because it holds no syntax and no
/// closures, `normalize` can reduce it to a normal form and `reduce_mapkey` can
/// apply it without any interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyMap {
    /// Identity: leaves all names unchanged.
    Id,
    /// Prefix: adds a string before each name.
    Prefix(String),
    /// Suffix: adds a string after each name.
    Suffix(String),
    /// Composition: apply the first mapper, then the second.
    Compose(Box<KeyMap>, Box<KeyMap>),
}

impl KeyMap {
    /// The type-level symbol for this mapper. The encodings are `Con`s plus
    /// `Ty::KeyAffix` for the string-carrying constructors, so a mapper can sit
    /// in a `Ty` next to a variable of the same kind.
    pub(crate) fn to_ty(&self) -> Ty {
        match self {
            KeyMap::Id => con("KeyMapId"),
            KeyMap::Prefix(s) => Ty::KeyAffix("KeyMapPrefix", s.clone()),
            KeyMap::Suffix(s) => Ty::KeyAffix("KeyMapSuffix", s.clone()),
            KeyMap::Compose(f, g) => Ty::Con("KeyMapCompose", vec![f.to_ty(), g.to_ty()]),
        }
    }

    /// Recover a mapper from a type-level symbol. `None` when the symbol is a
    /// variable, which has no closed form — the term is retained until that
    /// variable is bound.
    pub(crate) fn of_ty(t: &Ty) -> Option<KeyMap> {
        match t {
            Ty::Con("KeyMapId", a) if a.is_empty() => Some(KeyMap::Id),
            Ty::KeyAffix("KeyMapPrefix", s) => Some(KeyMap::Prefix(s.clone())),
            Ty::KeyAffix("KeyMapSuffix", s) => Some(KeyMap::Suffix(s.clone())),
            Ty::Con("KeyMapCompose", a) if a.len() == 2 => Some(
                KeyMap::Compose(
                    Box::new(KeyMap::of_ty(&a[0])?),
                    Box::new(KeyMap::of_ty(&a[1])?),
                )
                .normalize(),
            ),
            _ => None,
        }
    }

    /// Decode a label-driven mapper, `labelPrefix (Sym s)` / `labelSuffix
    /// (Sym s)`, once its label is a literal. This is the form `prependSym k`
    /// / `appendSym k` take while `k` is still a variable: it becomes an
    /// ordinary prefix or suffix mapper when the label is bound.
    ///
    /// `None` while the label is open, so the term stays retained exactly as a
    /// variable mapper does.
    pub(crate) fn of_label_ty(t: &Ty) -> Option<KeyMap> {
        match t {
            Ty::Con("labelPrefix", a) if a.len() == 1 => match &a[0] {
                Ty::Sym(s) => Some(KeyMap::Prefix(s.clone())),
                _ => None,
            },
            Ty::Con("labelSuffix", a) if a.len() == 1 => match &a[0] {
                Ty::Sym(s) => Some(KeyMap::Suffix(s.clone())),
                _ => None,
            },
            _ => None,
        }
    }

    /// Apply this key map to a single field name.
    ///
    /// Total: defined for every name, including names not yet known. That is
    /// what makes `keyMap` reducible on an open row when the row is later
    /// bound, and it is why this layer needs no closed-row restriction.
    pub(crate) fn apply(&self, name: &str) -> String {
        match self {
            KeyMap::Id => name.to_string(),
            KeyMap::Prefix(p) => format!("{}{}", p, name),
            KeyMap::Suffix(s) => format!("{}{}", name, s),
            KeyMap::Compose(f, g) => g.apply(&f.apply(name)),
        }
    }

    /// Reduce to a normal form: eliminate `Id`, reassociate `Compose` to the
    /// right, and fuse adjacent same-constructor chains.
    ///
    /// The fusion laws hold for all strings, so they are sound:
    ///   `Prefix a` then `Prefix b` = `Prefix (a <> b)`
    ///   `Suffix a` then `Suffix b` = `Suffix (b <> a)`   (note the order)
    pub(crate) fn normalize(self) -> KeyMap {
        match self {
            KeyMap::Compose(f, g) => {
                let f = f.normalize();
                let g = g.normalize();
                match (f, g) {
                    (KeyMap::Id, x) | (x, KeyMap::Id) => x,
                    (KeyMap::Prefix(a), KeyMap::Prefix(b)) => KeyMap::Prefix(format!("{a}{b}")),
                    (KeyMap::Suffix(a), KeyMap::Suffix(b)) => KeyMap::Suffix(format!("{b}{a}")),
                    (f, g) => KeyMap::Compose(Box::new(f), Box::new(g)),
                }
            }
            // A nested compose on the left is flattened by the recursive call
            // above, so the only remaining work is normalizing both sides.
            other => other,
        }
    }
}

/// Value-level type wrapper: applies a uniform transformation to field types.
/// First-order only: no arrow kinds, just specific wrappers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ValueMap {
    /// Identity: leaves types unchanged.
    Id,
    /// Wrap each field type in `maybe`.
    AsNullable,
    /// Wrap each field type in `list`.
    AsList,
}

impl ValueMap {
    /// Apply this value map to a single field type.
    pub(crate) fn apply(&self, ty: Ty) -> Ty {
        match self {
            ValueMap::Id => ty,
            ValueMap::AsNullable => match ty {
                // maybe (maybe t) = maybe t (idempotent)
                Ty::Con("maybe", _) => ty,
                _ => Ty::Con("maybe", vec![ty]),
            },
            ValueMap::AsList => Ty::Con("list", vec![ty]),
        }
    }

    /// The same wrapper at the *closed* level, for a reader that has only
    /// [`ScalarType`]s (the scheme views). One rule, two domains: a second
    /// spelling here is how a signature's columns and the row the reducer
    /// produces could come to disagree about a wrapper.
    pub(crate) fn apply_scalar(&self, ty: ScalarType) -> ScalarType {
        match self {
            ValueMap::Id => ty,
            ValueMap::AsNullable => match ty {
                // maybe (maybe t) = maybe t (idempotent)
                ScalarType::Maybe(_) => ty,
                _ => ScalarType::maybe(ty),
            },
            ValueMap::AsList => ScalarType::List(Box::new(ty)),
        }
    }
}

pub(crate) fn con(n: &'static str) -> Ty {
    Ty::Con(n, vec![])
}

pub(crate) fn fun(a: Ty, b: Ty) -> Ty {
    Ty::Fun(Box::new(a), Box::new(b))
}

pub(crate) fn query(r: Ty) -> Ty {
    Ty::Con("query", vec![r])
}

pub(crate) fn expr(p: Ty, r: Ty, a: Ty) -> Ty {
    Ty::Con("expr", vec![p, r, a])
}

/// The type of a projection/update record argument.
///
/// Surface signatures spell this as `expr r (row s)` (or
/// `agg (expr r (row s))` for the aggregate stage), but a record literal is
/// not itself a scalar expression. The extra `fields` slot retains the record
/// of column expressions that the stage constraint consumes, while `output` is
/// the row of resulting column values exposed by the signature.
pub(crate) fn record_expr(p: Ty, r: Ty, fields: Ty, output: Ty) -> Ty {
    Ty::Con("record_expr", vec![p, r, fields, output])
}

pub(crate) fn list(a: Ty) -> Ty {
    Ty::Con("list", vec![a])
}

pub(crate) fn sortkey(r: Ty) -> Ty {
    Ty::Con("sortkey", vec![r])
}

pub(crate) fn row(fs: Vec<(String, Ty)>, tail: Ty) -> Ty {
    Ty::Row(fs, Box::new(tail))
}

/// Kinds separate types from rows and key/value mappers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Type,
    Row,
    KeyMap,
    ValueMap,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cons {
    /// `where pred`: pred is a row-phase bool expression over `row`.
    Filter { pred: Ty, row: Ty },
    /// `select` / `agg` record over `input`, producing the `output` row.
    Project {
        fields: Ty,
        input: Ty,
        output: Ty,
        agg: bool,
    },
    /// Join predicate over `join left right`.
    JoinOn { pred: Ty, left: Ty, right: Ty },
    /// Output columns of a join: left, then right columns not on the left.
    /// `nullable`: whether the left / right columns become `maybe`.
    JoinOut {
        left: Ty,
        right: Ty,
        out: Ty,
        nullable: (bool, bool),
        left_only: bool,
    },
    /// Set operations require both inputs to expose the same row.
    Set { left: Ty, right: Ty, out: Ty },
    /// A literal of scalar type `lit` used where `target` is expected, once
    /// `target` is known (int widens to float, string to date / timestamp).
    Lit { lit: &'static str, target: Ty },
    /// `row` (a query's columns) must contain the `req` row, once `row` is
    /// known (so the query's column order is kept).
    Within { req: Ty, row: Ty },
    /// `update fields`: like `Project`, but merged over the input row —
    /// listed names replace the ones they match in place, and names the input
    /// does not have are appended.
    Update {
        fields: Ty,
        input: Ty,
        /// Row of the updated fields (`s` in the public signature).
        output: Ty,
        /// Final row after merging the updates over the input (`merge r s`).
        result: Ty,
    },
    /// `omit "k"`: the row equation `input ~ { k : t | output }`. The unifier
    /// solves this with its own leftover rule — there is no bespoke column
    /// computation here.
    /// `omit "k"`: the stage's own rule, `input ~ { k : t | output }`. The
    /// label is a type (`symbol k`, bound to the literal's `Sym` at the use),
    /// so the equation travels in the definition's scheme and every use
    /// instantiates it with fresh variables. The missing-column error is
    /// reported by this constraint, which is the operation that owns it.
    Omit { label: Ty, input: Ty, output: Ty },
    /// `mapValue "w"`: the stage's own rule, `output ~ mapValue w input`. The
    /// witness is a type (`valueLabel w`, bound to the literal's `Sym` at the
    /// use), so the equation travels in the definition's scheme and every use
    /// instantiates it with fresh variables. An unknown wrapper is reported by
    /// this constraint, which is the operation that owns it.
    MapValue { label: Ty, output: Ty },
    /// `merge left right`: combine two rows; right-wins on collision.
    Merge { left: Ty, right: Ty, out: Ty },
    /// Use of an overload set at type `target`.
    Overload {
        name: String,
        module: usize,
        cands: Vec<usize>,
        target: Ty,
        origin: Origin,
    },
}

/// Where an overload's choice is recorded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Origin {
    /// Use site (see `encode`) in the definition being checked.
    Site(u32),
    /// Hole index of a generalized definition (replaced on instantiation).
    Hole(usize),
}

/// A resolved overload use: a candidate, or a hole of the enclosing
/// definition (filled differently at each of its uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Def(usize, usize),
    Hole(usize),
}

/// Scalar-type mirror of a `Ty`, for the checked layer.
///
/// `Ty` is the *inference* representation: variables, rigids, schemes, and
/// open rows. Later phases need the *result* of inference instead — a closed,
/// printable scalar type — so `CheckedExpr.ty` is this type, not a `Ty`. The
/// conversion lives here because it is the only module where `Ty` is visible.
///
/// Visibility is `pub(crate)`, not `pub`: `Ty` is an implementation detail of
/// inference and the checked layer lives in this crate. `check/mod.rs`
/// re-exports these for `crate::checked`.
pub(crate) fn ty_to_scalar(t: &Ty) -> ScalarType {
    match t {
        // A variable that reaches here was never solved. "Unknown" is the
        // honest answer: the rest of the definition is already reported.
        Ty::Var(_) | Ty::Rigid(..) | Ty::Gen(_) | Ty::Sym(_) => ScalarType::Unknown,
        Ty::Con(name, args) => match (*name, args.len()) {
            ("int", 0) => ScalarType::Int,
            ("float", 0) => ScalarType::Float,
            ("decimal", 0) => ScalarType::Decimal,
            ("string", 0) => ScalarType::String,
            ("bool", 0) => ScalarType::Bool,
            ("date", 0) => ScalarType::Date,
            ("timestamp", 0) => ScalarType::Timestamp,
            ("maybe", 1) => ScalarType::maybe(ty_to_scalar(&args[0])),
            ("list", 1) => ScalarType::List(Box::new(ty_to_scalar(&args[0]))),
            _ => ScalarType::Unknown,
        },
        Ty::Fun(..) | Ty::Row(..) | Ty::Empty | Ty::MapKey(..) | Ty::KeyAffix(..) => {
            ScalarType::Unknown
        }
        Ty::Merge(..) | Ty::MapValue(..) | Ty::RemoveKey(..) => ScalarType::Unknown,
    }
}

/// Fields of a row type, or of the row inside `query r`.
///
/// The row terms a definition's scheme may still carry (`merge`, `mapValue`)
/// are reduced here rather than in the checked layer, so the checked layer
/// never has to know how the checker represents them.
pub(crate) fn ty_fields(t: &Ty) -> Vec<(String, ScalarType)> {
    match t {
        Ty::Row(fs, _) => fs
            .iter()
            .map(|(n, t)| (n.clone(), ty_to_scalar(t)))
            .collect(),
        Ty::Con("query", args) if args.len() == 1 => ty_fields(&args[0]),
        // The merge rule is the reducer's, at the closed level.
        Ty::Merge(a, b) => overlay(ty_fields(a), ty_fields(b)),
        // A wrapper is a label: a `Sym` names one, and an open label is not
        // known yet, so the columns are the input's until it is. The wrapping
        // itself is the mapper's own rule, at the closed level.
        Ty::MapValue(label, r) => {
            let fields = ty_fields(r);
            match value_map_of_label(label) {
                Some(vm) => fields
                    .into_iter()
                    .map(|(n, t)| (n, vm.apply_scalar(t)))
                    .collect(),
                None => fields,
            }
        }
        _ => vec![],
    }
}

/// What a definition's scheme *is*, structurally.
///
/// [`ty_fields`] returns `vec![]` for every non-row type and
/// [`ty_to_scalar`] returns `Unknown` for every non-scalar one, so neither can
/// say "this is a function" or "this is a query". The checked layer needs that
/// distinction — asking a helper (`f = q => q & where …`) for its columns and
/// getting an empty list claims it is a query with no fields — and reading the
/// printed scheme for the word `query` gets it wrong whenever a *function*
/// mentions a query in its argument or result type.
///
/// This is the structural answer, computed where `Ty` is visible. It is the
/// only thing the checked layer should branch on.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemeView {
    /// `query r`: a query over `r`. `fields` is `None` when `r` is still open,
    /// which is different from a query with no fields.
    Query {
        fields: Option<Vec<(String, ScalarType)>>,
    },
    /// A scalar type (`int`, `maybe string`, `list a`, …).
    Scalar(ScalarType),
    /// A function, however many arguments: `a -> b`, `query r -> query r`.
    Function,
    /// A row that is not behind `query` (a bare `{ a = int }`).
    Row { fields: Vec<(String, ScalarType)> },
    /// Anything the checker did not close: an unsolved variable, a rigid name,
    /// or a type-level term it could not reduce.
    Open,
}

/// Classify a scheme's type. See [`SchemeView`].
pub(crate) fn scheme_view(t: &Ty) -> SchemeView {
    match t {
        Ty::Con("query", args) if args.len() == 1 => SchemeView::Query {
            fields: closed_fields(&args[0]),
        },
        // A bare row is a row whether or not its tail is closed: the fields
        // listed are real, and an open tail only means more may follow.
        Ty::Row(fs, _) => SchemeView::Row {
            fields: fs
                .iter()
                .map(|(n, t)| (n.clone(), ty_to_scalar(t)))
                .collect(),
        },
        Ty::Fun(..) => SchemeView::Function,
        Ty::Sym(_) => SchemeView::Open,
        Ty::Merge(..)
        | Ty::MapValue(..)
        | Ty::MapKey(..)
        | Ty::KeyAffix(..)
        | Ty::RemoveKey(..) => {
            let fields = ty_fields(t);
            if fields.is_empty() {
                SchemeView::Open
            } else {
                SchemeView::Row { fields }
            }
        }
        Ty::Var(_) | Ty::Rigid(..) | Ty::Gen(_) | Ty::Empty => SchemeView::Open,
        // `expr r a` is the *scalar* `a` together with a phase and a row. A
        // consumer asking for the type of an expression wants `a`: the row is
        // what the column is read from and the phase is where it may be used,
        // and both are query-level facts carried elsewhere.
        //
        // Unwrapping here rather than at each call site matters because
        // `use_ty` is one of several readers. Without it, `upper .name` had the
        // recorded type `expr r string` and every reader saw `Unknown`, since
        // `ty_to_scalar` has no `expr` case — the type was present and
        // unreadable, which is worse than absent because it looks recorded.
        Ty::Con("expr", args) if args.len() == 3 => scheme_view(&args[2]),
        Ty::Con(..) => SchemeView::Scalar(ty_to_scalar(t)),
    }
}

/// The phase a scheme's result sits in, and the scalar inside it.
///
/// `sum : expr r int -> agg (expr r (maybe int))` returns an *aggregate*
/// expression, so a call to it is `CheckedExpr::agg_template`, not a scalar
/// template.
///
/// The phase is the **first argument of the `expr` constructor**, not an outer
/// head: the checker writes `expr(con("agg"), r, a)` (`infer.rs:1658`), which
/// prints as `agg (expr r a)`. Reading it as `Ty::Con("agg", ..)` finds nothing
/// and silently reports every aggregate as row-phase, which is what an earlier
/// version of this function did.
///
/// Returns `(phase, value)`. `None` when the result is not an expression at all
/// (a query, a function returning a function).
pub(crate) fn scheme_result_expr(t: &Ty) -> Option<(Phase, ScalarType)> {
    match t {
        Ty::Fun(_, r) => scheme_result_expr(r),
        Ty::Con("expr", args) if args.len() == 3 => {
            let phase = match &args[0] {
                Ty::Con("agg", _) => Phase::Agg,
                Ty::Con("win", _) => Phase::Win,
                // A variable phase in a polymorphic signature: the call site's
                // own phase is what matters and it is not knowable here, so
                // report row phase, which is the one every context accepts.
                _ => Phase::Row,
            };
            match scheme_view(&args[2]) {
                SchemeView::Scalar(s) => Some((phase, s)),
                SchemeView::Open => Some((phase, ScalarType::Unknown)),
                _ => None,
            }
        }
        // A bare scalar constant, e.g. `one : int = 1`.
        other => match scheme_view(other) {
            SchemeView::Scalar(s) => Some((Phase::Const, s)),
            SchemeView::Open => Some((Phase::Const, ScalarType::Unknown)),
            _ => None,
        },
    }
}

/// A definition's result type, with the arguments stripped and any `expr r a`
/// wrapper unwrapped to `a`.
///
/// A call site needs the type the callee *returns*, and a definition's scheme
/// is where that is declared: `_>_` is
/// `expr r a -> expr r a -> expr r bool`, and the call's type is the `bool`
/// inside the final `expr`.
///
/// `None` when the scheme's result is not a scalar (a query, a function
/// returning a function, an open row).
pub(crate) fn scheme_result_scalar(t: &Ty) -> Option<ScalarType> {
    match t {
        Ty::Fun(_, r) => scheme_result_scalar(r),
        Ty::Con("expr", args) if args.len() == 3 => match scheme_view(&args[2]) {
            SchemeView::Scalar(s) => Some(s),
            SchemeView::Open => Some(ScalarType::Unknown),
            _ => None,
        },
        other => match scheme_view(other) {
            SchemeView::Scalar(s) => Some(s),
            SchemeView::Open => Some(ScalarType::Unknown),
            _ => None,
        },
    }
}

/// The fields of a row type when it is closed, or `None` when its tail is
/// still open. `None` means "not known", not "empty".
fn closed_fields(t: &Ty) -> Option<Vec<(String, ScalarType)>> {
    match t {
        Ty::Row(fs, tail) if matches!(**tail, Ty::Empty) => Some(
            fs.iter()
                .map(|(n, t)| (n.clone(), ty_to_scalar(t)))
                .collect(),
        ),
        // A closed merge or value-map reduces to concrete fields.
        Ty::Merge(..) | Ty::MapValue(..) => {
            let fields = ty_fields(t);
            if fields.is_empty() {
                None
            } else {
                Some(fields)
            }
        }
        // `query r` in a signature, or a row term still waiting on a variable:
        // the columns are not statically known.
        _ => None,
    }
}

impl Cons {
    pub(crate) fn map(&self, f: &mut impl FnMut(&Ty) -> Ty) -> Cons {
        match self {
            Cons::Filter { pred, row } => Cons::Filter {
                pred: f(pred),
                row: f(row),
            },
            Cons::Project {
                fields,
                input,
                output,
                agg,
            } => Cons::Project {
                fields: f(fields),
                input: f(input),
                output: f(output),
                agg: *agg,
            },
            Cons::JoinOn { pred, left, right } => Cons::JoinOn {
                pred: f(pred),
                left: f(left),
                right: f(right),
            },
            Cons::JoinOut {
                left,
                right,
                out,
                nullable,
                left_only,
            } => Cons::JoinOut {
                left: f(left),
                right: f(right),
                out: f(out),
                nullable: *nullable,
                left_only: *left_only,
            },
            Cons::Set { left, right, out } => Cons::Set {
                left: f(left),
                right: f(right),
                out: f(out),
            },
            Cons::Lit { lit, target } => Cons::Lit {
                lit,
                target: f(target),
            },
            Cons::Within { req, row } => Cons::Within {
                req: f(req),
                row: f(row),
            },
            Cons::Update {
                fields,
                input,
                output,
                result,
            } => Cons::Update {
                fields: f(fields),
                input: f(input),
                output: f(output),
                result: f(result),
            },
            Cons::Omit {
                label,
                input,
                output,
            } => Cons::Omit {
                label: f(label),
                input: f(input),
                output: f(output),
            },
            Cons::MapValue { label, output } => Cons::MapValue {
                label: f(label),
                output: f(output),
            },
            Cons::Merge { left, right, out } => Cons::Merge {
                left: f(left),
                right: f(right),
                out: f(out),
            },
            Cons::Overload {
                name,
                module,
                cands,
                target,
                origin,
            } => Cons::Overload {
                name: name.clone(),
                module: *module,
                cands: cands.clone(),
                target: f(target),
                origin: *origin,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Scheme {
    pub(crate) ty: Ty,
    pub(crate) cons: Vec<Cons>,
    pub(crate) gens: Vec<GenInfo>,
    /// The stage this definition *is*, when its body is a `primitive`.
    ///
    /// The operation travels with the scheme so a use site can record which
    /// stage it applied without reading it back out of a composed result type;
    /// a helper carries `None`, and the stages it applies are the nodes
    /// recorded in its own body.
    pub(crate) stage: Option<RowStage>,
    /// The definition has a type error: its users fail too, instead of
    /// going on with a made-up type.
    pub(crate) failed: bool,
}

/// Flags of a scheme's quantified variable, copied to each instance.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GenInfo {
    pub(crate) kind: Kind,
    pub(crate) row_or_win: bool,
    pub(crate) nonnull: bool,
    /// Signature name, for printing.
    pub(crate) name: Option<String>,
}

#[derive(Clone)]
pub(crate) struct VarInfo {
    pub(crate) bound: Option<Ty>,
    /// The kind of this type variable.
    pub(crate) kind: Kind,
    /// Phase variable of a column reference: may become `row` or `win`.
    pub(crate) row_or_win: bool,
    /// From a signature's type variable: cannot become `maybe _`.
    pub(crate) nonnull: bool,
    pub(crate) version: u64,
}

pub(crate) struct Checker<'w> {
    pub(crate) env: ModuleEnv<'w>,
    /// The module being checked; other modules' schemes are given.
    pub(crate) module: usize,
    pub(crate) vars: Vec<VarInfo>,
    pub(crate) schemes: HashMap<(usize, usize), Scheme>,
    pub(crate) failed: HashSet<(usize, usize)>,
    pub(crate) active: Vec<(usize, usize)>,
    /// How many `def_scheme` calls are on the stack, for `MAX_DEF_DEPTH`.
    pub(crate) depth: usize,
    /// Whether the `MAX_DEF_DEPTH` diagnostic was already reported.
    pub(crate) depth_reported: bool,
    pub(crate) pending: Vec<(Cons, Span)>,
    pub(crate) errors: Vec<RawTypeError>,
    /// Location of the argument being checked (for deferred literals).
    pub(crate) span: Span,
    /// Previous state of every variable binding, for trial unification.
    pub(crate) trail: Vec<(u32, VarInfo)>,
    pub(crate) fit_cache: HashMap<(usize, Vec<usize>, String), Vec<usize>>,
    pub(crate) holes: HashMap<(usize, usize), usize>,
    pub(crate) choices: HashMap<(usize, usize), HashMap<(u32, usize), Choice>>,
    /// Input row of the `PROBE_FIELD` column being checked, and the definition
    /// that referenced it. The probe is text the LSP spliced in, so it can
    /// only appear in one definition; recording it once at the end would let
    /// a later definition (one whose own probe row is unknown, such as a
    /// helper over an open parameter) overwrite the answer.
    pub(crate) probe: Option<(usize, Ty)>,
    pub(crate) probe_fields: Option<Vec<(String, String)>>,
    /// Names, column references, and literals of the definition being
    /// checked, with their types there. A literal also keeps its own type,
    /// printed if the type it was lifted to stays open.
    ///
    /// The `ExprId` is carried because it is the key the rest of the checker's
    /// per-use bookkeeping uses — `choices` is keyed by `Origin::Site`, which
    /// is an `ExprId` — and because a phase that wants to build a typed tree
    /// from source must pair a syntactic node with the type it received. A span
    /// cannot do that job: two expressions can share a span, and a span is not
    /// what a choice was recorded against.
    pub(crate) uses: Vec<(ast::ExprId, Span, Ty, Option<&'static str>)>,
    /// What each module-scope name occurrence resolved to, by `ExprId`.
    ///
    /// The identity counterpart of the type recorded for a use: the type says
    /// what an expression *is*, this says what a name *referred to*. A consumer
    /// expanding a call needs the second and cannot recover it from the first,
    /// and re-deriving it by looking the name up again is a second answer
    /// waiting to disagree with this one. Read while checking — a stage
    /// application looks up its callee through it — and copied onto the typed
    /// tree, which is where readers outside the checker get it.
    pub(crate) resolutions: HashMap<(usize, u32), Resolved>,
    /// The stage applications of the definition being checked, in order.
    pub(crate) row_stages: Vec<PendingRowStage>,
    /// Each definition's body as a typed tree, by `(module, def)`.
    pub(crate) typed_bodies: HashMap<(usize, usize), TypedBody>,
    /// Recursion depth of `unify`, to turn a cyclic reduction into a
    /// diagnostic instead of a stack overflow.
    pub(crate) unify_depth: usize,
}

pub(crate) fn missing(l: &str, have: &[(String, Ty)]) -> String {
    let avail: Vec<&str> = have.iter().map(|(k, _)| k.as_str()).collect();
    if avail.is_empty() {
        format!("no column `{l}`")
    } else {
        format!("no column `{l}`; available: {}", avail.join(", "))
    }
}

/// A row's column names, for an error that lists what a query produced.
pub(crate) fn names(fields: &[(String, Ty)]) -> String {
    fields
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Error type and helpers shared by every `Checker` module. Moved here
/// verbatim from the single `check.rs`; the split changed no logic.
pub(crate) struct TyErr {
    pub(crate) span: Span,
    pub(crate) msg: String,
}
pub(crate) type R<T> = Result<T, TyErr>;
pub(crate) type U = Result<(), String>;
pub(crate) fn at(span: Span) -> impl FnOnce(String) -> TyErr {
    move |msg| TyErr { span, msg }
}
/// An overload's origin names the use site and which hole of the used
/// definition it is (0 for a direct use of an overload set). `site` and the
/// hole index share one `u32`, so both are bounded.
pub(crate) fn encode(site: u32, k: usize) -> Option<u32> {
    if k >= 256 || site >= (1 << 23) {
        return None;
    }
    Some((site << 8) | k as u32 | (1 << 31))
}
pub(crate) fn decode(origin: u32) -> (u32, usize) {
    if origin & (1 << 31) != 0 {
        ((origin & !(1 << 31)) >> 8, (origin & 0xff) as usize)
    } else {
        (origin, 0)
    }
}
