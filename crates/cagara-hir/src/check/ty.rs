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
    /// Value mapping over a row: applies a uniform type wrapper to every field type.
    MapValue(ValueMap, Box<Ty>),
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
    Omit { key: String, input: Ty, output: Ty },
    /// `prefix "s"` / `suffix "s"`: `output ~ keyMap m input`, where `m` is the
    /// type-level mapper. Usually `m` is the concrete witness the affix names,
    /// but a helper such as `addPrefix = p => q => q & prefix p` leaves it as a
    /// variable, so this equation is what binds that variable once the affix is
    /// known at a use.
    ///
    /// With `m` still a variable the term is retained (R-MapKey-Open); with `m`
    /// concrete and `input` closed the output row is computed here.
    MapKey {
        marker: KeyMarker,
        key: Ty,
        input: Ty,
        output: Ty,
    },
    /// `mapValue "wrapper"`: every column type wrapped by the witness.
    MapValue {
        wrapper: String,
        input: Ty,
        output: Ty,
    },
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
            Cons::Omit { key, input, output } => Cons::Omit {
                key: key.clone(),
                input: f(input),
                output: f(output),
            },
            Cons::MapKey {
                marker,
                key,
                input,
                output,
            } => Cons::MapKey {
                marker: *marker,
                key: f(key),
                input: f(input),
                output: f(output),
            },
            Cons::MapValue {
                wrapper,
                input,
                output,
            } => Cons::MapValue {
                wrapper: wrapper.clone(),
                input: f(input),
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
    pub(crate) uses: Vec<(Span, Ty, Option<&'static str>)>,
    pub(crate) use_types: HashMap<(usize, u32, u32), String>,
    /// Key stages whose affix was not a literal, in the order their key
    /// arguments were read. `key_stage` consumes one per deferred argument to
    /// build a term with a fresh mapper variable instead of a concrete affix,
    /// which is what lets `prefix p` appear in a helper.
    pub(crate) deferred_keys: Vec<KeyMarker>,
    /// Mappers named by an `affix m` parameter, one per key argument read.
    /// `key_stage` consumes one to build the stage's term, so the witness in
    /// the result type and the mapper the affix names are the same variable.
    /// Recursion depth of `unify`, to turn a cyclic reduction into a
    /// diagnostic instead of a stack overflow.
    pub(crate) unify_depth: usize,
    pub(crate) affix_mappers: Vec<(KeyMarker, Ty)>,
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

/// The marker on a key parameter.
///
/// A key names a column (or supplies a label fragment) rather than being an
/// ordinary expression, so it cannot be typed as one. The parameter therefore
/// carries a marker type, and the application recognises that marker in order
/// to read the literal where it is still written.
///
/// This is an enum rather than a set of string literals on purpose: the marker
/// name used to be spelled out at the producer, at the consumer, and in the
/// error text, with nothing tying the three together. A rename in one place
/// silently broke another, which is how this file came to reference AST
/// variants that no longer existed. One constructor, one spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyMarker {
    /// `omit "k"` — drops one column.
    Omit,
    /// `prefix "s"` — prepends to every column name.
    Prefix,
    /// `suffix "s"` — appends to every column name.
    Suffix,
    /// `mapValue "w"` — wraps every field type.
    ValueWrapper,
}

impl KeyMarker {
    /// The type constructor standing for this marker in a signature.
    pub(crate) fn ty(self) -> Ty {
        let name = match self {
            KeyMarker::Omit => "key_omit",
            KeyMarker::Prefix => "key_prefix",
            KeyMarker::Suffix => "key_suffix",
            KeyMarker::ValueWrapper => "value_wrapper",
        };
        Ty::Con(name, Vec::new())
    }

    /// The marker a type denotes, if it denotes one.
    pub(crate) fn of(t: &Ty) -> Option<KeyMarker> {
        match t {
            Ty::Con("key_omit", a) if a.is_empty() => Some(KeyMarker::Omit),
            Ty::Con("key_prefix", a) if a.is_empty() => Some(KeyMarker::Prefix),
            Ty::Con("key_suffix", a) if a.is_empty() => Some(KeyMarker::Suffix),
            Ty::Con("value_wrapper", a) if a.is_empty() => Some(KeyMarker::ValueWrapper),
            _ => None,
        }
    }

    /// The `key_stage` key list tag for this marker. The tag is internal to
    /// this file: it is produced only by [`KeyMarker::tag`] and matched only in
    /// `key_stage`.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            KeyMarker::Omit => "key_omit",
            KeyMarker::Prefix => "key_prefix",
            KeyMarker::Suffix => "key_suffix",
            KeyMarker::ValueWrapper => "value_wrapper",
        }
    }

    /// Why this argument has to be a literal, and what to write instead.
    pub(crate) fn literal_msg(self) -> String {
        match self {
            KeyMarker::Omit => {
                "`omit` needs a literal column name, such as `omit \"password_hash\"`: the \
                 column must be known when the query is checked"
                    .into()
            }
            KeyMarker::Prefix => {
                "`prefix` needs a literal string, such as `prefix \"user_\"`: the mapping is \
                 part of the type, so it must be known when the query is checked"
                    .into()
            }
            KeyMarker::Suffix => {
                "`suffix` needs a literal string, such as `suffix \"_v2\"`: the mapping is part \
                 of the type, so it must be known when the query is checked"
                    .into()
            }
            KeyMarker::ValueWrapper => {
                "`mapValue` needs a literal wrapper, such as `mapValue \"nullable\"` or \
                 `mapValue \"list\"`"
                    .into()
            }
        }
    }
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
