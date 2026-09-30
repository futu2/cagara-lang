//! Type checker: Hindley–Milner inference with extensible (Rémy-style) rows,
//! run over the AST before evaluation.
//!
//! - Surface types: `expr r a` (row expression), `agg (expr r a)` and
//!   `win (expr r a)`. Internally one `expr` constructor carries a phase slot
//!   (`row`, `agg`, `win`, or a variable), so `agg (expr r a)` is `expr` at
//!   phase `agg`. A plain `expr` in a signature is phase-polymorphic, shared
//!   across the signature (so `_+_` also works on aggregates and windows),
//!   except when the result is `agg`/`win`: then its `expr` arguments must be
//!   row-phase (the depth-1 rule).
//! - Column references (`.x`) get a phase variable that may become `row` or
//!   `win` but never `agg`, which is how ungrouped columns are rejected.
//! - Scalar constants lift into `expr` at call sites; int literals widen to
//!   float and strings to date / timestamp when the expected value type is
//!   already known.
//! - Query stages (`where`, `select`, `agg`, joins) create deferred
//!   constraints that are solved once their inputs are known. Unsolved ones
//!   travel with the definition's type scheme and are re-instantiated at use.
//! - Top-level definitions are generalized; lambda parameters are monomorphic.
//!   Signatures are checked: their type variables are rigid.
//!
//! - Key mappers are typed by their literal column lists: a list of string
//!   literals has a `Labels` type and a record of string literals a `Renames`
//!   type (both still unify with `list string` / records of strings), so
//!   `pick ["id"]`, `omit [..]` and `rename { a = "b" }` compute their output
//!   row statically, with the same rules as the IR validator.
//!
//! - Overloading: a name defined more than once (each with a signature) is an
//!   overload set. Each use gets the candidates' common shape and an
//!   `Overload` constraint, resolved by trial unification once exactly one
//!   candidate fits. A definition whose overloads stay open (`x => x + x`)
//!   keeps them as holes in its scheme; each use fills them, and the
//!   evaluator follows the recorded choices (dictionary passing, resolved at
//!   compile time). In a query definition, leftover literals default to
//!   their own type before overloads are forced.
//!
//! - Nullability is explicit: `maybe a` is a type, and type variables in
//!   signatures stand for non-null types, so `expr r a` and
//!   `expr r (maybe a)` are disjoint overloads. Operators need non-null
//!   arguments (`coalesce` / `isNotNull` handle nulls); outer joins make the
//!   far side's columns `maybe`; `sum` / `avg` / `min` / `max` return `maybe`.
//!
//! Known limits: `prefix` / `suffix` and key lists that are not literals give
//! an unconstrained row (the schema validator checks them).

use crate::db::ModuleInput;
use crate::ir::{JoinKind, KeyMapper, Phase};
use crate::lower::parse_module;
use crate::resolve::{module_own, module_scope};
use crate::rules::{self, Place};
use crate::value::Prim;
use crate::workspace::{diag_in, Binding, Diag, Workspace};
use cagara_syntax::ast::{self, ExprKind, Side, Span, TypeExpr};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
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
    /// A list of string literals (`["id", "name"]`); a `list string`.
    Labels(Vec<String>),
    /// A record of string literals (`{ id = "key" }`); a closed record of strings.
    Renames(Vec<(String, String)>),
}

fn con(n: &'static str) -> Ty {
    Ty::Con(n, vec![])
}
fn fun(a: Ty, b: Ty) -> Ty {
    Ty::Fun(Box::new(a), Box::new(b))
}
fn query(r: Ty) -> Ty {
    Ty::Con("query", vec![r])
}
fn expr(p: Ty, r: Ty, a: Ty) -> Ty {
    Ty::Con("expr", vec![p, r, a])
}
fn list(a: Ty) -> Ty {
    Ty::Con("list", vec![a])
}
fn sortkey(r: Ty) -> Ty {
    Ty::Con("sortkey", vec![r])
}
fn mapper(payload: Ty) -> Ty {
    Ty::Con("mapper", vec![payload])
}
fn row(fs: Vec<(String, Ty)>, tail: Ty) -> Ty {
    Ty::Row(fs, Box::new(tail))
}

const SCALARS: &[&str] = &["int", "float", "string", "bool", "date", "timestamp"];

/// Type constructors usable in signatures, with their arities.
const CONS: &[(&str, usize)] = &[
    ("int", 0),
    ("float", 0),
    ("string", 0),
    ("bool", 0),
    ("date", 0),
    ("timestamp", 0),
    ("maybe", 1),
    ("list", 1),
    ("query", 1),
    ("expr", 2),
    ("agg", 1),
    ("win", 1),
    ("winspec", 1),
    ("sortkey", 1),
    ("frame", 0),
    ("bound", 0),
];

const NULLABLE: &str = "expected a non-null value, found a `maybe`; use `coalesce default x` \
                        (or `isNull` / `isNotNull` to test it)";
/// An aggregate reaching a phase that must be row or window: next to a
/// plain column, or as a `select` field.
const UNGROUPED: &str = "aggregates cannot mix with ungrouped columns or be `select` fields; \
                         aggregate in `agg`, with columns wrapped in `group`";

/// The IR phase a phase type stands for, once it is known.
fn phase_of(t: &Ty) -> Option<Phase> {
    match t {
        Ty::Con("row", _) => Some(Phase::Row),
        Ty::Con("agg", _) => Some(Phase::Agg),
        Ty::Con("win", _) => Some(Phase::Win),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Cons {
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
    },
    /// A literal of scalar type `lit` used where `target` is expected, once
    /// `target` is known (int widens to float, string to date / timestamp).
    Lit { lit: &'static str, target: Ty },
    /// `keyMap mapper`: output row from the input row, once both are known.
    KeyMap { mapper: Ty, input: Ty, output: Ty },
    /// `row` (a query's columns) must contain the `req` row, once `row` is
    /// known (so the query's column order is kept).
    Within { req: Ty, row: Ty },
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
enum Origin {
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
    fn map(&self, f: &mut impl FnMut(&Ty) -> Ty) -> Cons {
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
            } => Cons::JoinOut {
                left: f(left),
                right: f(right),
                out: f(out),
                nullable: *nullable,
            },
            Cons::Lit { lit, target } => Cons::Lit {
                lit,
                target: f(target),
            },
            Cons::KeyMap {
                mapper,
                input,
                output,
            } => Cons::KeyMap {
                mapper: f(mapper),
                input: f(input),
                output: f(output),
            },
            Cons::Within { req, row } => Cons::Within {
                req: f(req),
                row: f(row),
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
struct Scheme {
    ty: Ty,
    cons: Vec<Cons>,
    gens: Vec<GenInfo>,
    /// The definition has a type error: its users fail too, instead of
    /// going on with a made-up type.
    failed: bool,
}

/// Flags of a scheme's quantified variable, copied to each instance.
#[derive(Debug, Clone, PartialEq)]
struct GenInfo {
    row_or_win: bool,
    nonnull: bool,
    /// Signature name, for printing.
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeError {
    pub module: usize,
    pub def: usize,
    pub diag: Diag,
}

/// Result of checking every definition in a workspace.
pub struct TypeCheck {
    pub errors: Vec<TypeError>,
    types: HashMap<(usize, usize), String>,
    holes: HashMap<(usize, usize), usize>,
    /// Per definition: `(use site, hole of the referenced definition)` → choice.
    /// Hole 0 of a direct overload-set use is the overload itself.
    choices: HashMap<(usize, usize), HashMap<(u32, usize), Choice>>,
    /// Per module: the columns known at its `PROBE_FIELD` reference.
    probe_fields: HashMap<usize, Vec<(String, String)>>,
    /// Printed type of each name use, by `(module, span start, span end)`:
    /// the instance at that use, not the definition's scheme.
    use_types: HashMap<(usize, u32, u32), String>,
}

/// A column name no user writes (`__` names are reserved). A reference to
/// it adds no column to its row; the checker records the columns the row
/// is known to have instead, for completion.
pub const PROBE_FIELD: &str = "__cagara_complete";

impl TypeCheck {
    /// Columns (name, printed type) of the row seen by the `PROBE_FIELD`
    /// reference in `module`, if it has one and its definition got that far.
    pub fn probe_fields(&self, module: usize) -> Option<&[(String, String)]> {
        self.probe_fields.get(&module).map(Vec::as_slice)
    }

    /// Printed type scheme of a well-typed definition.
    pub fn type_of(&self, module: usize, def: usize) -> Option<&str> {
        self.types.get(&(module, def)).map(String::as_str)
    }

    /// Number of open overloads a definition leaves to its users.
    pub fn holes(&self, module: usize, def: usize) -> usize {
        self.holes.get(&(module, def)).copied().unwrap_or(0)
    }

    /// Choice for hole `k` at use site `site` inside definition `(module, def)`.
    pub fn choice(&self, module: usize, def: usize, site: u32, k: usize) -> Option<Choice> {
        self.choices.get(&(module, def))?.get(&(site, k)).copied()
    }

    /// Printed type of the name use whose expression spans `span` in `module`.
    pub fn use_type(&self, module: usize, span: Span) -> Option<&str> {
        self.use_types
            .get(&(module, span.start, span.end))
            .map(String::as_str)
    }

    pub fn error_for(&self, module: usize, def: usize) -> Option<&Diag> {
        self.errors
            .iter()
            .find(|e| e.module == module && e.def == def)
            .map(|e| &e.diag)
    }
}

/// Type-check every definition of every loaded module (prelude included).
pub fn check(ws: &Workspace) -> TypeCheck {
    // Modules are loaded after their imports, so index order is a valid
    // dependency order: each module is checked on its own, seeing only the
    // (self-contained) schemes of the modules before it.
    // Each module's check is a salsa query, so an edit re-checks only the
    // edited module and the modules that (transitively) depend on it.
    let mut out = TypeCheck {
        errors: vec![],
        types: HashMap::new(),
        holes: HashMap::new(),
        choices: HashMap::new(),
        probe_fields: HashMap::new(),
        use_types: HashMap::new(),
    };
    for &input in &ws.inputs {
        let mc = module_check(&ws.db, input);
        out.errors.extend(mc.errors.iter().cloned());
        out.types.extend(mc.types.clone());
        out.holes.extend(mc.holes.clone());
        out.choices.extend(mc.choices.clone());
        out.probe_fields.extend(mc.probe_fields.clone());
        out.use_types.extend(mc.use_types.clone());
    }
    out
}

/// Check one module (memoized). Dependencies are checked first; only their
/// self-contained schemes are used.
#[salsa::tracked(returns(ref))]
fn module_check(db: &dyn salsa::Database, input: ModuleInput) -> ModuleCheck {
    #[cfg(test)]
    CHECK_RUNS.with(|c| c.set(c.get() + 1));
    let mut deps = HashMap::new();
    let imported: Vec<ModuleInput> = input
        .prelude(db)
        .iter()
        .copied()
        .chain(input.imports(db).iter().map(|(_, t)| *t))
        .collect();
    for d in &imported {
        deps.extend(module_check(db, *d).schemes.clone());
    }
    let file = *input.file(db);
    let parsed = parse_module(db, file);
    let env = ModuleEnv {
        module: *input.index(db),
        path: input.path(db),
        text: file.text(db),
        defs: &parsed.module.defs,
        scope: module_scope(db, input),
        owns: imported
            .iter()
            .map(|t| (*t.index(db), module_own(db, *t)))
            .collect(),
    };
    check_module(env, &deps)
}

#[cfg(test)]
thread_local! {
    static CHECK_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many module checks ran on this thread (for incrementality tests).
#[cfg(test)]
fn check_runs() -> usize {
    CHECK_RUNS.with(|c| c.get())
}

/// Result of checking one module.
#[derive(Debug, Clone, PartialEq)]
struct ModuleCheck {
    schemes: HashMap<(usize, usize), Scheme>,
    errors: Vec<TypeError>,
    types: HashMap<(usize, usize), String>,
    holes: HashMap<(usize, usize), usize>,
    choices: HashMap<(usize, usize), HashMap<(u32, usize), Choice>>,
    probe_fields: Option<(usize, Vec<(String, String)>)>,
    use_types: HashMap<(usize, u32, u32), String>,
}

/// What checking one module reads: its definitions, scope, and source (for
/// diagnostics), and every module's exports (for `alias.name`). Nothing
/// else of the workspace is needed, so a salsa query can build this view.
pub struct ModuleEnv<'w> {
    pub module: usize,
    pub path: &'w std::path::Path,
    pub text: &'w str,
    pub defs: &'w [ast::Def],
    pub scope: &'w HashMap<String, Binding>,
    /// Exports of the modules it imports, by module index.
    pub owns: HashMap<usize, &'w HashMap<String, Binding>>,
}

impl<'w> ModuleEnv<'w> {
    pub fn of(ws: &'w Workspace, m: usize) -> Self {
        let md = &ws.modules[m];
        ModuleEnv {
            module: m,
            path: &md.path,
            text: &md.text,
            defs: &md.module.defs,
            scope: &md.scope,
            owns: ws
                .modules
                .iter()
                .enumerate()
                .map(|(i, x)| (i, &x.own))
                .collect(),
        }
    }
}

/// Check one module against the schemes of the modules it may use.
fn check_module(env: ModuleEnv<'_>, deps: &HashMap<(usize, usize), Scheme>) -> ModuleCheck {
    let m = env.module;
    let mut c = Checker {
        env,
        module: m,
        vars: Vec::new(),
        schemes: deps.clone(),
        failed: HashSet::new(),
        active: Vec::new(),
        pending: Vec::new(),
        errors: Vec::new(),
        span: Span::default(),
        trail: Vec::new(),
        holes: HashMap::new(),
        choices: HashMap::new(),
        probe: None,
        probe_fields: None,
        uses: Vec::new(),
        use_types: HashMap::new(),
    };
    for i in 0..c.env.defs.len() {
        c.def_scheme(m, i);
    }
    let schemes: HashMap<_, _> = c
        .schemes
        .iter()
        .filter(|(k, _)| k.0 == m)
        .map(|(k, s)| (*k, s.clone()))
        .collect();
    let types = schemes
        .iter()
        .filter(|(k, _)| !c.failed.contains(k))
        .map(|(k, s)| (*k, c.show_scheme(s)))
        .collect();
    let probe_fields = c.probe_fields.map(|fs| (m, fs));
    let use_types = c.use_types;
    ModuleCheck {
        schemes,
        errors: c.errors,
        types,
        holes: c.holes,
        choices: c.choices,
        probe_fields,
        use_types,
    }
}

struct TyErr {
    span: Span,
    msg: String,
}

type R<T> = Result<T, TyErr>;
type U = Result<(), String>;

fn at(span: Span) -> impl FnOnce(String) -> TyErr {
    move |msg| TyErr { span, msg }
}

#[derive(Clone)]
struct VarInfo {
    bound: Option<Ty>,
    /// Phase variable of a column reference: may become `row` or `win`.
    row_or_win: bool,
    /// From a signature's type variable: cannot become `maybe _`.
    nonnull: bool,
}

struct Checker<'w> {
    env: ModuleEnv<'w>,
    /// The module being checked; other modules' schemes are given.
    module: usize,
    vars: Vec<VarInfo>,
    schemes: HashMap<(usize, usize), Scheme>,
    failed: HashSet<(usize, usize)>,
    active: Vec<(usize, usize)>,
    pending: Vec<(Cons, Span)>,
    errors: Vec<TypeError>,
    /// Location of the argument being checked (for deferred literals).
    span: Span,
    /// Previous state of every variable binding, for trial unification.
    trail: Vec<(u32, VarInfo)>,
    holes: HashMap<(usize, usize), usize>,
    choices: HashMap<(usize, usize), HashMap<(u32, usize), Choice>>,
    /// Input row of the `PROBE_FIELD` column being checked.
    probe: Option<Ty>,
    probe_fields: Option<Vec<(String, String)>>,
    /// Names, column references, and literals of the definition being
    /// checked, with their types there. A literal also keeps its own type,
    /// printed if the type it was lifted to stays open.
    uses: Vec<(Span, Ty, Option<&'static str>)>,
    use_types: HashMap<(usize, u32, u32), String>,
}
fn row_or_tail(fs: Vec<(String, Ty)>, tail: Ty) -> Ty {
    if fs.is_empty() {
        tail
    } else {
        row(fs, tail)
    }
}

fn missing(l: &str, have: &[(String, Ty)]) -> String {
    let avail: Vec<&str> = have.iter().map(|(k, _)| k.as_str()).collect();
    if avail.is_empty() {
        format!("no column `{l}`")
    } else {
        format!("no column `{l}`; available: {}", avail.join(", "))
    }
}

impl<'w> Checker<'w> {
    // ── variables and substitution ─────────────────────────────────────────

    fn fresh_id(&mut self, row_or_win: bool) -> u32 {
        self.fresh_with(row_or_win, false)
    }

    fn fresh_with(&mut self, row_or_win: bool, nonnull: bool) -> u32 {
        self.vars.push(VarInfo {
            bound: None,
            row_or_win,
            nonnull,
        });
        self.vars.len() as u32 - 1
    }

    fn fresh(&mut self) -> Ty {
        Ty::Var(self.fresh_id(false))
    }

    fn fresh_col_phase(&mut self) -> Ty {
        Ty::Var(self.fresh_id(true))
    }

    fn resolve(&self, t: &Ty) -> Ty {
        let mut t = t.clone();
        while let Ty::Var(v) = t {
            match &self.vars[v as usize].bound {
                Some(b) => t = b.clone(),
                None => break,
            }
        }
        t
    }

    /// Row fields and the tail after following bound variables.
    fn flatten(&self, t: &Ty) -> (Vec<(String, Ty)>, Ty) {
        let mut fs = Vec::new();
        let mut cur = self.resolve(t);
        loop {
            match cur {
                Ty::Row(more, tail) => {
                    fs.extend(more);
                    cur = self.resolve(&tail);
                }
                Ty::Renames(ps) => {
                    fs.extend(ps.into_iter().map(|(k, _)| (k, con("string"))));
                    return (fs, Ty::Empty);
                }
                other => return (fs, other),
            }
        }
    }

    fn zonk(&self, t: &Ty) -> Ty {
        match self.resolve(t) {
            Ty::Con(n, args) => Ty::Con(n, args.iter().map(|a| self.zonk(a)).collect()),
            Ty::Fun(a, b) => fun(self.zonk(&a), self.zonk(&b)),
            r @ Ty::Row(..) => {
                let (fs, tail) = self.flatten(&r);
                let fs = fs.into_iter().map(|(k, v)| (k, self.zonk(&v))).collect();
                row_or_tail(fs, tail)
            }
            o => o,
        }
    }

    fn occurs(&self, v: u32, t: &Ty) -> bool {
        match self.resolve(t) {
            Ty::Var(u) => u == v,
            Ty::Con(_, args) => args.iter().any(|a| self.occurs(v, a)),
            Ty::Fun(a, b) => self.occurs(v, &a) || self.occurs(v, &b),
            Ty::Row(fs, tail) => fs.iter().any(|(_, t)| self.occurs(v, t)) || self.occurs(v, &tail),
            _ => false,
        }
    }

    // ── unification ────────────────────────────────────────────────────────

    fn bind(&mut self, v: u32, t: Ty) -> U {
        let t = self.resolve(&t);
        if t == Ty::Var(v) {
            return Ok(());
        }
        if self.occurs(v, &t) {
            return Err(format!(
                "infinite type: a type would contain itself ({})",
                self.show(&t)
            ));
        }
        self.trail.push((v, self.vars[v as usize].clone()));
        if self.vars[v as usize].row_or_win {
            match &t {
                Ty::Var(u) => {
                    self.trail.push((*u, self.vars[*u as usize].clone()));
                    self.vars[*u as usize].row_or_win = true
                }
                Ty::Con("row" | "win", _) => {}
                Ty::Con("agg", _) => return Err(UNGROUPED.into()),
                o => {
                    return Err(format!(
                        "a column expression cannot have phase {}",
                        self.show(o)
                    ))
                }
            }
        }
        if self.vars[v as usize].nonnull {
            match &t {
                Ty::Var(u) => {
                    self.trail.push((*u, self.vars[*u as usize].clone()));
                    self.vars[*u as usize].nonnull = true
                }
                Ty::Con("maybe", _) => return Err(NULLABLE.into()),
                _ => {}
            }
        }
        self.vars[v as usize].bound = Some(t);
        Ok(())
    }

    /// Unify `actual` with `expected` (the order only affects messages).
    fn unify(&mut self, actual: &Ty, expected: &Ty) -> U {
        let (a, e) = (self.resolve(actual), self.resolve(expected));
        match (&a, &e) {
            (Ty::Var(v), _) => self.bind(*v, e.clone()),
            (_, Ty::Var(v)) => self.bind(*v, a.clone()),
            (Ty::Rigid(x, _), Ty::Rigid(y, _)) if x == y => Ok(()),
            (Ty::Empty, Ty::Empty) => Ok(()),
            // Label lists and rename records are compared by content: a key
            // mapper built from one must not stand for another.
            (Ty::Labels(x), Ty::Labels(y)) if x == y => Ok(()),
            (Ty::Labels(_), Ty::Labels(_)) => Err(self.mismatch(&a, &e)),
            (Ty::Renames(x), Ty::Renames(y)) => {
                let (mut x, mut y) = (x.clone(), y.clone());
                x.sort();
                y.sort();
                if x == y {
                    Ok(())
                } else {
                    Err(self.mismatch(&a, &e))
                }
            }
            (
                Ty::Row(..) | Ty::Renames(..),
                Ty::Row(..) | Ty::Empty | Ty::Rigid(..) | Ty::Renames(..),
            )
            | (Ty::Empty | Ty::Rigid(..), Ty::Row(..) | Ty::Renames(..)) => self.unify_rows(&a, &e),
            (Ty::Labels(ls), Ty::Con("list", x)) | (Ty::Con("list", x), Ty::Labels(ls)) => {
                if ls.is_empty() {
                    // `[]` fits any element type.
                    return Ok(());
                }
                let x = x[0].clone();
                self.unify(&x, &con("string"))
            }
            (Ty::Con(x, xs), Ty::Con(y, ys)) if x == y && xs.len() == ys.len() => {
                for (p, q) in xs.clone().iter().zip(ys.clone().iter()) {
                    self.unify(p, q)?;
                }
                Ok(())
            }
            (Ty::Fun(a1, r1), Ty::Fun(a2, r2)) => {
                let (a1, r1, a2, r2) = (a1.clone(), r1.clone(), a2.clone(), r2.clone());
                self.unify(&a1, &a2)?;
                self.unify(&r1, &r2)
            }
            _ => Err(self.mismatch(&a, &e)),
        }
    }

    fn unify_rows(&mut self, actual: &Ty, expected: &Ty) -> U {
        let (fa, ta) = self.flatten(actual);
        let (fe, te) = self.flatten(expected);
        for (l, t) in &fa {
            if let Some((_, u)) = fe.iter().find(|(k, _)| k == l) {
                self.unify(t, u).map_err(|m| format!("field `{l}`: {m}"))?;
            }
        }
        let only_a: Vec<(String, Ty)> = fa
            .iter()
            .filter(|(l, _)| !fe.iter().any(|(k, _)| k == l))
            .cloned()
            .collect();
        let only_e: Vec<(String, Ty)> = fe
            .iter()
            .filter(|(l, _)| !fa.iter().any(|(k, _)| k == l))
            .cloned()
            .collect();
        let closed = |t: &Ty| !matches!(t, Ty::Var(_));
        if let Some((l, _)) = only_a.first() {
            if closed(&te) {
                return Err(missing(l, &fe));
            }
        }
        if let Some((l, _)) = only_e.first() {
            if closed(&ta) {
                return Err(missing(l, &fa));
            }
        }
        match (only_a.is_empty(), only_e.is_empty()) {
            (true, true) => self.unify(&ta, &te),
            (false, true) => self.unify(&te, &row(only_a, ta)),
            (true, false) => self.unify(&ta, &row(only_e, te)),
            (false, false) => {
                if ta == te {
                    return Err("incompatible row types".into());
                }
                let tail = self.fresh();
                self.unify(&ta, &row(only_e, tail.clone()))?;
                self.unify(&te, &row(only_a, tail))
            }
        }
    }

    fn mismatch(&self, a: &Ty, e: &Ty) -> String {
        if let (Some(x), Some(y)) = (phase_of(a), phase_of(e)) {
            return rules::clash(x, y);
        }
        let join = |t: &Ty| matches!(t, Ty::Con("join", _));
        let plain = |t: &Ty| matches!(t, Ty::Row(..) | Ty::Empty);
        if (join(a) && plain(e)) || (plain(a) && join(e)) {
            return "cannot mix join-side columns (`.<x`, `.>x`) with plain columns (`.x`)".into();
        }
        let maybe = |t: &Ty| matches!(t, Ty::Con("maybe", _));
        let hint = if maybe(a) != maybe(e) {
            "; only one side is nullable: `coalesce default x` takes a `maybe`, `just x` makes one"
        } else {
            ""
        };
        format!(
            "type mismatch: expected {}, found {}{hint}",
            self.show(e),
            self.show(a)
        )
    }

    // ── definitions and schemes ────────────────────────────────────────────

    fn def_scheme(&mut self, m: usize, i: usize) -> Option<Scheme> {
        if let Some(s) = self.schemes.get(&(m, i)) {
            return Some(s.clone());
        }
        if self.active.contains(&(m, i)) || m != self.module {
            // Recursion (the evaluator reports it), or a module that is not
            // loaded before this one (cannot happen for valid imports).
            return None;
        }
        self.active.push((m, i));
        let saved = std::mem::take(&mut self.pending);
        let saved_uses = std::mem::take(&mut self.uses);
        let r = self.check_def(i);
        // Use types as far as checking got, even if it failed later on.
        for (sp, t, lit) in std::mem::replace(&mut self.uses, saved_uses) {
            let shown = match (lit, self.resolve(&t)) {
                (Some(l), Ty::Var(_)) => l.to_string(),
                _ => self.show(&t),
            };
            self.use_types.insert((m, sp.start, sp.end), shown);
        }
        if let Some(p) = self.probe.take() {
            // Whatever the definition learned about the probe's row, even
            // if it failed later on.
            let (fs, _) = self.flatten(&p);
            self.probe_fields = Some(fs.iter().map(|(k, t)| (k.clone(), self.show(t))).collect());
        }
        let left = std::mem::replace(&mut self.pending, saved);
        self.active.pop();
        let scheme = match r {
            Ok(t) => {
                // Open overloads become holes, numbered in constraint order.
                let mut cons = Vec::new();
                let mut k = 0;
                for (c, _) in &left {
                    let mut c = c.map(&mut |t| self.zonk(t));
                    if let Cons::Overload { origin, .. } = &mut c {
                        if let Origin::Site(site) = *origin {
                            let (use_site, hk) = decode(site);
                            self.record((m, i), use_site, hk, Choice::Hole(k));
                        }
                        *origin = Origin::Hole(k);
                        k += 1;
                    }
                    cons.push(c);
                }
                if k > 0 {
                    self.holes.insert((m, i), k);
                }
                self.generalize(&t, cons)
            }
            Err(e) => {
                let diag = diag_in(self.env.path, self.env.text, e.span, e.msg);
                self.errors.push(TypeError {
                    module: m,
                    def: i,
                    diag,
                });
                self.failed.insert((m, i));
                let v = self.fresh();
                Scheme {
                    failed: true,
                    ..self.generalize(&v, vec![])
                }
            }
        };
        self.schemes.insert((m, i), scheme.clone());
        Some(scheme)
    }

    fn check_def(&mut self, i: usize) -> R<Ty> {
        let defs = self.env.defs;
        let def = &defs[i];
        let ann = match &def.ty {
            Some(t) => Some(self.annotation(t).map_err(at(def.span))?),
            None => None,
        };
        if let ExprKind::Sql(_) = def.body.kind {
            // Templates are trusted: their signature is their type (the
            // evaluator reports a missing one).
            return Ok(ann.unwrap_or_else(|| self.fresh()));
        }
        let t = self.infer(&mut Vec::new(), &def.body)?;
        if let Some(a) = &ann {
            self.span = def.body.span;
            self.coerce(&t, a)
                .map_err(|msg| format!("`{}` does not match its signature: {msg}", def.name))
                .map_err(at(def.body.span))?;
        }
        self.solve()?;
        let t = ann.unwrap_or(t);
        // A member of an overload set is picked by its signature alone, so
        // no use can fill holes of its own: it resolves them like a query.
        let candidate = defs.iter().filter(|d| d.name == def.name).count() > 1;
        if candidate || matches!(self.resolve(&t), Ty::Con("query", _)) {
            // A query is evaluated, so its overloads must be resolved:
            // default leftover literals, then report what is still open.
            self.default_lits();
            self.solve()?;
            for (c, sp) in self.pending.clone() {
                if let Cons::Overload {
                    name,
                    module,
                    cands,
                    target,
                    ..
                } = c
                {
                    let fits = self.fitting(module, &cands, &target);
                    let shown: Vec<String> =
                        fits.iter().map(|&i| self.cand_shown(module, i)).collect();
                    let msg = format!(
                        "ambiguous use of `{}` at type {}; candidates: {}",
                        op_name(&name),
                        self.show(&target),
                        shown.join(", ")
                    );
                    return Err(TyErr { span: sp, msg });
                }
            }
        } else {
            // A non-query definition is not evaluated, so its overloads may
            // stay open for its users. One that no candidate can satisfy is an
            // error already, though: without this it stays hidden until
            // something uses the definition (`.a + "x"`). Leftover literals
            // stay polymorphic (`.age >= 18` keeps its type), so this only
            // reports, and never narrows, what the definition inferred.
            if let Some((
                Cons::Overload {
                    name,
                    module,
                    cands,
                    target,
                    ..
                },
                sp,
            )) = self.first_unsatisfiable()
            {
                let shown: Vec<String> =
                    cands.iter().map(|&i| self.cand_shown(module, i)).collect();
                let msg = format!(
                    "no overload of `{}` matches {}; candidates: {}",
                    op_name(&name),
                    self.show(&target),
                    shown.join(", ")
                );
                return Err(TyErr { span: sp, msg });
            }
        }
        Ok(t)
    }

    /// The first open overload with no candidates left once leftover
    /// literals are defaulted. A single trial covers every overload, so the
    /// cost stays linear in their number. Leaves the checker as it found it.
    fn first_unsatisfiable(&mut self) -> Option<(Cons, Span)> {
        let (trail, nvars) = (self.trail.len(), self.vars.len());
        let saved = self.pending.clone();
        self.default_lits();
        let _ = self.solve();
        let mut found = None;
        for (c, sp) in &saved {
            if let Cons::Overload {
                module,
                cands,
                target,
                ..
            } = c
            {
                if self.fitting(*module, cands, target).is_empty() {
                    found = Some((c.clone(), *sp));
                    break;
                }
            }
        }
        while self.trail.len() > trail {
            let (v, old) = self.trail.pop().expect("trail entry");
            if (v as usize) < nvars {
                self.vars[v as usize] = old;
            }
        }
        self.vars.truncate(nvars);
        self.pending = saved;
        found
    }

    fn default_lits(&mut self) {
        let lits: Vec<(&'static str, Ty)> = self
            .pending
            .iter()
            .filter_map(|(c, _)| match c {
                Cons::Lit { lit, target } => Some((*lit, target.clone())),
                _ => None,
            })
            .collect();
        for (lit, target) in lits {
            if let Ty::Var(_) = self.resolve(&target) {
                let _ = self.unify(&target, &con(lit));
            }
        }
    }

    // ── overloads ──────────────────────────────────────────────────────────

    /// Scheme of an overload candidate: checked if possible, else (inside
    /// its own body) its signature.
    fn cand_scheme(&mut self, m: usize, i: usize) -> Scheme {
        if let Some(s) = self.def_scheme(m, i) {
            return s;
        }
        let defs = self.env.defs;
        let ann = if m == self.module {
            defs[i].ty.as_ref().map(|t| self.annotation(t))
        } else {
            None
        };
        match ann {
            Some(Ok(t)) => self.generalize(&t, vec![]),
            _ => {
                let v = self.fresh();
                self.generalize(&v, vec![])
            }
        }
    }

    fn cand_shown(&mut self, m: usize, i: usize) -> String {
        let s = self.cand_scheme(m, i);
        self.show_scheme(&s)
    }

    /// Would candidate type `t` unify with `target`? Leaves no trace.
    fn fits(&mut self, s: &Scheme, target: &Ty) -> bool {
        let (trail, nvars) = (self.trail.len(), self.vars.len());
        let t = self.inst(&s.ty, &mut HashMap::new(), &s.gens);
        let ok = self.unify(&t, target).is_ok();
        while self.trail.len() > trail {
            let (v, old) = self.trail.pop().expect("trail entry");
            if (v as usize) < nvars {
                self.vars[v as usize] = old;
            }
        }
        self.vars.truncate(nvars);
        ok
    }

    fn fitting(&mut self, m: usize, cands: &[usize], target: &Ty) -> Vec<usize> {
        let mut out = Vec::new();
        for &i in cands {
            let s = self.cand_scheme(m, i);
            if self.fits(&s, target) {
                out.push(i);
            }
        }
        out
    }

    /// Type of a use of an overload set: the candidates' common shape.
    fn overload_type(&mut self, name: &str, m: usize, cands: &[usize], site: u32, sp: Span) -> Ty {
        let tys: Vec<Ty> = cands.iter().map(|&i| self.cand_scheme(m, i).ty).collect();
        let target = self.skeleton(&tys, &mut HashMap::new());
        let c = Cons::Overload {
            name: name.to_string(),
            module: m,
            cands: cands.to_vec(),
            target: target.clone(),
            origin: Origin::Site(site),
        };
        self.pending.push((c, sp));
        target
    }

    /// Anti-unification: shared structure is kept; positions where the
    /// candidates differ become variables (one per distinct combination).
    fn skeleton(&mut self, ts: &[Ty], memo: &mut HashMap<String, Ty>) -> Ty {
        let ts: Vec<Ty> = ts.iter().map(|t| self.resolve(t)).collect();
        match &ts[0] {
            Ty::Con(n, args)
                if ts
                    .iter()
                    .all(|t| matches!(t, Ty::Con(m, a) if m == n && a.len() == args.len())) =>
            {
                let (n, arity) = (*n, args.len());
                let args = (0..arity)
                    .map(|k| {
                        let col: Vec<Ty> = ts
                            .iter()
                            .map(|t| match t {
                                Ty::Con(_, a) => a[k].clone(),
                                _ => unreachable!(),
                            })
                            .collect();
                        self.skeleton(&col, memo)
                    })
                    .collect();
                Ty::Con(n, args)
            }
            Ty::Fun(..) if ts.iter().all(|t| matches!(t, Ty::Fun(..))) => {
                let (mut xs, mut ys) = (Vec::new(), Vec::new());
                for t in &ts {
                    if let Ty::Fun(a, b) = t {
                        xs.push((**a).clone());
                        ys.push((**b).clone());
                    }
                }
                fun(self.skeleton(&xs, memo), self.skeleton(&ys, memo))
            }
            _ => {
                let key = format!("{ts:?}");
                if let Some(t) = memo.get(&key) {
                    return t.clone();
                }
                let v = self.fresh();
                memo.insert(key, v.clone());
                v
            }
        }
    }

    fn record(&mut self, key: (usize, usize), site: u32, k: usize, c: Choice) {
        self.choices.entry(key).or_default().insert((site, k), c);
    }

    /// Convert a signature. Its type variables are rigid; its phase variable
    /// is flexible (a plain `expr` may be used at any phase).
    fn annotation(&mut self, t: &TypeExpr) -> Result<Ty, String> {
        let mut res = t;
        while let TypeExpr::Fun(_, r) = res {
            res = r;
        }
        // A plain `expr` result takes the phase of an `agg` / `win`
        // argument (a scalar template over an aggregate is an aggregate).
        let mut wrapped = Vec::new();
        let mut arg = t;
        while let TypeExpr::Fun(a, r) = arg {
            if let TypeExpr::App { head, .. } = a.as_ref() {
                if (head == "agg" || head == "win") && !wrapped.contains(&head.as_str()) {
                    wrapped.push(head.as_str());
                }
            }
            arg = r;
        }
        let phase = match res {
            TypeExpr::App { head, .. } if head == "agg" || head == "win" => con("row"),
            _ => match wrapped.as_slice() {
                [] => self.fresh(),
                ["agg"] => con("agg"),
                ["win"] => con("win"),
                _ => return Err(rules::clash(Phase::Agg, Phase::Win)),
            },
        };
        let mut names = HashMap::new();
        self.conv(t, &phase, &mut names)
    }

    fn conv(
        &mut self,
        t: &TypeExpr,
        phase: &Ty,
        names: &mut HashMap<String, Ty>,
    ) -> Result<Ty, String> {
        match t {
            TypeExpr::App { head, args, .. } => {
                let Some(&(name, arity)) = CONS.iter().find(|(n, _)| *n == head.as_str()) else {
                    if args.is_empty() {
                        return Ok(self.rigid(head, names));
                    }
                    return Err(format!("unknown type constructor `{head}`"));
                };
                if args.len() != arity {
                    return Err(format!(
                        "`{name}` takes {arity} type argument(s), got {}",
                        args.len()
                    ));
                }
                match name {
                    "expr" => {
                        let r = self.conv(&args[0], phase, names)?;
                        let a = self.conv(&args[1], phase, names)?;
                        Ok(expr(phase.clone(), r, a))
                    }
                    "agg" | "win" => match &args[0] {
                        TypeExpr::App {
                            head, args: inner, ..
                        } if head == "expr" && inner.len() == 2 => {
                            let r = self.conv(&inner[0], phase, names)?;
                            let a = self.conv(&inner[1], phase, names)?;
                            Ok(expr(con(name), r, a))
                        }
                        _ => Err(format!(
                            "`{name}` wraps an expression type, e.g. `{name} (expr r int)`"
                        )),
                    },
                    _ => {
                        let args = args
                            .iter()
                            .map(|a| self.conv(a, phase, names))
                            .collect::<Result<_, _>>()?;
                        Ok(Ty::Con(name, args))
                    }
                }
            }
            TypeExpr::Record { fields, tail, .. } => {
                let mut fs: Vec<(String, Ty)> = Vec::new();
                for (k, v) in fields {
                    if fs.iter().any(|(o, _)| o == k) {
                        return Err(format!("field `{k}` appears twice in a record type"));
                    }
                    let v = self.conv(v, phase, names)?;
                    fs.push((k.clone(), v));
                }
                let tail = match tail {
                    Some(n) => self.rigid(n, names),
                    None => Ty::Empty,
                };
                Ok(row_or_tail(fs, tail))
            }
            TypeExpr::Fun(a, b) => Ok(fun(
                self.conv(a, phase, names)?,
                self.conv(b, phase, names)?,
            )),
            TypeExpr::Error(_) => Ok(self.fresh()),
        }
    }

    fn rigid(&mut self, name: &str, names: &mut HashMap<String, Ty>) -> Ty {
        if let Some(t) = names.get(name) {
            return t.clone();
        }
        let t = Ty::Rigid(self.fresh_with(false, true), name.to_string());
        names.insert(name.to_string(), t.clone());
        t
    }

    /// Fresh copy of a scheme; its deferred constraints join the pending set.
    /// Its holes become overload uses at `site`. Fails when the definition
    /// leaves more open overloads than an encoded origin can hold.
    fn instantiate(&mut self, s: &Scheme, span: Span, site: Option<u32>) -> Result<Ty, String> {
        let mut map = HashMap::new();
        let t = self.inst(&s.ty, &mut map, &s.gens);
        for c in &s.cons {
            let mut c = c.map(&mut |t| self.inst(t, &mut map, &s.gens));
            if let (Cons::Overload { origin, .. }, Some(site)) = (&mut c, site) {
                if let Origin::Hole(k) = *origin {
                    // Resolved as hole `k` of the definition used at `site`.
                    let encoded = encode(site, k).ok_or_else(|| {
                        "too many open overloads: this definition is used where it would need more \
                         than 256 unresolved overloaded names; give it a type signature"
                            .to_string()
                    })?;
                    *origin = Origin::Site(encoded);
                }
            }
            self.pending.push((c, span));
        }
        Ok(t)
    }

    /// Fresh arena variables for a scheme's `Gen` variables.
    fn inst(&mut self, t: &Ty, map: &mut HashMap<u32, Ty>, gens: &[GenInfo]) -> Ty {
        match self.resolve(t) {
            Ty::Gen(k) => {
                if let Some(t) = map.get(&k) {
                    return t.clone();
                }
                let g = &gens[k as usize];
                let n = Ty::Var(self.fresh_with(g.row_or_win, g.nonnull));
                map.insert(k, n.clone());
                n
            }
            Ty::Con(n, args) => Ty::Con(n, args.iter().map(|a| self.inst(a, map, gens)).collect()),
            Ty::Fun(a, b) => fun(self.inst(&a, map, gens), self.inst(&b, map, gens)),
            Ty::Row(fs, tail) => {
                let fs = fs
                    .iter()
                    .map(|(k, v)| (k.clone(), self.inst(v, map, gens)))
                    .collect();
                row(fs, self.inst(&tail, map, gens))
            }
            t @ (Ty::Var(_) | Ty::Rigid(..) | Ty::Empty | Ty::Labels(_) | Ty::Renames(_)) => t,
        }
    }

    /// Close a type and its constraints into a self-contained scheme: every
    /// free variable becomes a `Gen` index carrying its flags, so the scheme
    /// no longer refers to this checker's variable arena.
    fn generalize(&self, ty: &Ty, cons: Vec<Cons>) -> Scheme {
        let (mut map, mut gens) = (HashMap::new(), Vec::new());
        let ty = self.gen(ty, &mut map, &mut gens);
        let mut out = Vec::new();
        for c in &cons {
            out.push(c.map(&mut |t| self.gen(t, &mut map, &mut gens)));
        }
        Scheme {
            ty,
            cons: out,
            gens,
            failed: false,
        }
    }

    fn gen(&self, t: &Ty, map: &mut HashMap<u32, u32>, gens: &mut Vec<GenInfo>) -> Ty {
        match self.resolve(t) {
            r @ (Ty::Var(_) | Ty::Rigid(..)) => {
                let (v, name) = match r {
                    Ty::Var(v) => (v, None),
                    Ty::Rigid(v, n) => (v, Some(n)),
                    _ => unreachable!(),
                };
                let k = *map.entry(v).or_insert_with(|| {
                    let info = &self.vars[v as usize];
                    gens.push(GenInfo {
                        row_or_win: info.row_or_win,
                        nonnull: info.nonnull,
                        name,
                    });
                    gens.len() as u32 - 1
                });
                Ty::Gen(k)
            }
            Ty::Con(n, args) => Ty::Con(n, args.iter().map(|a| self.gen(a, map, gens)).collect()),
            Ty::Fun(a, b) => fun(self.gen(&a, map, gens), self.gen(&b, map, gens)),
            Ty::Row(fs, tail) => {
                let fs = fs
                    .iter()
                    .map(|(k, v)| (k.clone(), self.gen(v, map, gens)))
                    .collect();
                row(fs, self.gen(&tail, map, gens))
            }
            t @ (Ty::Gen(_) | Ty::Empty | Ty::Labels(_) | Ty::Renames(_)) => t,
        }
    }

    // ── inference ──────────────────────────────────────────────────────────

    fn infer(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        match &e.kind {
            ExprKind::Name(n) => {
                let t = match env.iter().rev().find(|(k, _)| k == n) {
                    Some((_, t)) => t.clone(),
                    None => self.lookup(n, e.id, sp).map_err(at(sp))?,
                };
                self.uses.push((sp, t.clone(), None));
                Ok(t)
            }
            ExprKind::Lit(l) => {
                let t = con(lit_name(l));
                self.uses.push((sp, t.clone(), None));
                Ok(t)
            }
            ExprKind::Field(side, n) => {
                let phase = self.fresh_col_phase();
                let (tail, a) = (self.fresh(), self.fresh());
                let r = if n == PROBE_FIELD {
                    // Completion probe: adds no column, remembers the row.
                    self.probe = Some(tail.clone());
                    tail
                } else {
                    row(vec![(n.clone(), a.clone())], tail)
                };
                let input = match side {
                    Side::Single => r,
                    Side::Left => Ty::Con("join", vec![r, self.fresh()]),
                    Side::Right => Ty::Con("join", vec![self.fresh(), r]),
                };
                // Hover shows the column's value type, not the whole row.
                self.uses.push((sp, a.clone(), None));
                Ok(expr(phase, input, a))
            }
            ExprKind::Proj(base, f) => {
                if let ExprKind::Name(n) = &base.kind {
                    let scope = self.env.scope;
                    if !env.iter().any(|(k, _)| k == n) {
                        if let Some(Binding::Module(t)) = scope.get(n) {
                            let own = self.env.owns[t];
                            let t = match own.get(f).cloned() {
                                Some(Binding::Def(dm, i)) => {
                                    self.def_type(dm, i, e.id, sp).map_err(at(sp))?
                                }
                                Some(Binding::Overloads(om, is)) => {
                                    self.overload_type(f, om, &is, e.id, sp)
                                }
                                _ => {
                                    return Err(TyErr {
                                        span: sp,
                                        msg: format!("module `{n}` has no definition `{f}`"),
                                    })
                                }
                            };
                            self.uses.push((sp, t.clone(), None));
                            return Ok(t);
                        }
                    }
                }
                let bt = self.infer(env, base)?;
                let (a, tail) = (self.fresh(), self.fresh());
                let want = row(vec![(f.clone(), a.clone())], tail);
                self.unify(&bt, &want)
                    .map_err(|msg| format!("cannot take `.{f}`: {msg}"))
                    .map_err(at(sp))?;
                Ok(a)
            }
            ExprKind::App(f, args) => {
                let mut ft = self.infer(env, f)?;
                for arg in args {
                    let at_ = self.infer(env, arg)?;
                    self.span = arg.span;
                    ft = match self.resolve(&ft) {
                        Ty::Fun(p, r) => {
                            self.coerce(&at_, &p).map_err(at(arg.span))?;
                            if let ExprKind::Lit(l) = &arg.kind {
                                // A lifted literal (`18` in `.age >= 18`) has
                                // the value type it was lifted to.
                                let v = match self.resolve(&p) {
                                    Ty::Con("expr", a) => a[2].clone(),
                                    o => o,
                                };
                                self.uses.push((arg.span, v, Some(lit_name(l))));
                            }
                            *r
                        }
                        Ty::Var(_) => {
                            let r = self.fresh();
                            self.unify(&ft, &fun(at_, r.clone()))
                                .map_err(at(arg.span))?;
                            r
                        }
                        o => {
                            let msg = format!(
                                "cannot apply a value of type {} to an argument",
                                self.show(&o)
                            );
                            return Err(TyErr {
                                span: arg.span,
                                msg,
                            });
                        }
                    };
                    self.solve()?;
                }
                Ok(ft)
            }
            ExprKind::Lambda(p, body) => {
                let pt = self.fresh();
                env.push((p.clone(), pt.clone()));
                let bt = self.infer(env, body);
                env.pop();
                Ok(fun(pt, bt?))
            }
            ExprKind::Record(fs) => {
                let mut out: Vec<(String, Ty)> = Vec::new();
                for (k, v) in fs {
                    if out.iter().any(|(o, _)| o == k) {
                        return Err(TyErr {
                            span: sp,
                            msg: format!("field `{k}` appears twice"),
                        });
                    }
                    let t = self.infer(env, v)?;
                    out.push((k.clone(), t));
                }
                let lits: Option<Vec<(String, String)>> = fs
                    .iter()
                    .map(|(k, v)| match &v.kind {
                        ExprKind::Lit(ast::Lit::Str(s)) => Some((k.clone(), s.clone())),
                        _ => None,
                    })
                    .collect();
                match lits {
                    Some(ps) if !ps.is_empty() => Ok(Ty::Renames(ps)),
                    _ => Ok(row(out, Ty::Empty)),
                }
            }
            ExprKind::List(xs) => {
                let lits: Option<Vec<String>> = xs
                    .iter()
                    .map(|x| match &x.kind {
                        ExprKind::Lit(ast::Lit::Str(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                if let Some(ls) = lits {
                    return Ok(Ty::Labels(ls));
                }
                let mut ts = Vec::new();
                for x in xs {
                    ts.push((self.infer(env, x)?, x.span));
                }
                let Some((first, _)) = ts.first() else {
                    unreachable!("empty lists are `Labels`")
                };
                // A list of column expressions is a list of sort / partition
                // keys, so `[asc .x, .y]` has one element type.
                let elem = match self.resolve(first) {
                    Ty::Con("expr" | "sortkey", _) => sortkey(self.fresh()),
                    _ => first.clone(),
                };
                for (t, s) in &ts {
                    self.coerce(t, &elem).map_err(at(*s))?;
                }
                Ok(list(elem))
            }
            ExprKind::Sql(_) => Err(TyErr {
                span: sp,
                msg: "`sql \"...\"` must be the whole body of a definition with a type signature"
                    .into(),
            }),
            ExprKind::Error => Ok(self.fresh()),
        }
    }

    fn lookup(&mut self, n: &str, site: u32, sp: Span) -> Result<Ty, String> {
        match self.env.scope.get(n).cloned() {
            Some(Binding::Def(dm, di)) => self.def_type(dm, di, site, sp),
            Some(Binding::Overloads(om, is)) => Ok(self.overload_type(n, om, &is, site, sp)),
            Some(Binding::Prim(p)) => Ok(self.prim_type(p, sp)),
            Some(Binding::Module(_)) => Err(format!(
                "`{n}` is a module; refer to a definition as `{n}.name`"
            )),
            None => Err(format!("unknown name `{n}`")),
        }
    }

    fn def_type(&mut self, m: usize, i: usize, site: u32, sp: Span) -> Result<Ty, String> {
        match self.def_scheme(m, i) {
            Some(s) if s.failed => Err(self.failed_use(m, i)),
            Some(s) => self.instantiate(&s, sp, Some(site)),
            None => Ok(self.fresh()),
        }
    }

    fn failed_use(&self, m: usize, i: usize) -> String {
        let name = if m == self.module {
            Some(self.env.defs[i].name.as_str())
        } else {
            self.env.owns.get(&m).and_then(|own| {
                own.iter().find_map(|(n, b)| match b {
                    Binding::Def(dm, di) if (*dm, *di) == (m, i) => Some(n.as_str()),
                    Binding::Overloads(dm, is) if *dm == m && is.contains(&i) => Some(n.as_str()),
                    _ => None,
                })
            })
        };
        match name {
            Some(n) => format!(
                "`{}` has a type error, so this use cannot be checked",
                op_name(n)
            ),
            None => "this name has a type error, so this use cannot be checked".into(),
        }
    }

    // ── coercions at expectations ──────────────────────────────────────────

    /// Unify `actual` with `expected`, allowing constant lifting into `expr`,
    /// int → float and string → date / timestamp widening, expressions as sort keys, and
    /// records as window specs (also inside lists).
    fn coerce(&mut self, actual: &Ty, expected: &Ty) -> U {
        let (a, e) = (self.resolve(actual), self.resolve(expected));
        match (&a, &e) {
            (Ty::Con(s, sa), Ty::Con("expr", ea)) if sa.is_empty() && SCALARS.contains(s) => {
                let v = ea[2].clone();
                self.lift(s, &v)
            }
            (Ty::Con(s, sa), Ty::Con(_, ta))
                if sa.is_empty() && ta.is_empty() && SCALARS.contains(s) =>
            {
                self.lift(s, &e)
            }
            (Ty::Con("expr", ea), Ty::Con("sortkey", ka)) => {
                let (p, r, want) = (ea[0].clone(), ea[1].clone(), ka[0].clone());
                self.key_phase(&p)?;
                self.unify(&r, &want)
            }
            (Ty::Row(..) | Ty::Empty | Ty::Renames(_), Ty::Con("winspec", w)) => {
                let r = w[0].clone();
                self.winspec(&a, &r)
            }
            (Ty::Con("list", x), Ty::Con("list", y)) => {
                let (x, y) = (x[0].clone(), y[0].clone());
                self.coerce(&x, &y)
            }
            _ => self.unify(&a, &e),
        }
    }

    fn lift(&mut self, s: &'static str, target: &Ty) -> U {
        match self.resolve(target) {
            Ty::Var(_) if matches!(s, "int" | "string") => {
                self.pending.push((
                    Cons::Lit {
                        lit: s,
                        target: target.clone(),
                    },
                    self.span,
                ));
                Ok(())
            }
            Ty::Con(t, args)
                if args.is_empty()
                    && matches!((s, t), ("int", "float") | ("string", "date" | "timestamp")) =>
            {
                Ok(())
            }
            _ => self.unify(&con(s), target),
        }
    }

    fn key_phase(&mut self, p: &Ty) -> U {
        self.place(Place::Key, p)
    }

    fn winspec(&mut self, spec: &Ty, r: &Ty) -> U {
        let (fs, tail) = self.flatten(spec);
        self.unify(&tail, &Ty::Empty)?;
        for (k, t) in fs {
            let want = match k.as_str() {
                "partition" | "order" => list(sortkey(r.clone())),
                "frame" => con("frame"),
                o => {
                    return Err(format!(
                        "unknown window spec field `{o}`; expected partition, order, frame"
                    ))
                }
            };
            self.coerce(&t, &want)
                .map_err(|m| format!("window spec field `{k}`: {m}"))?;
        }
        Ok(())
    }

    // ── primitives ─────────────────────────────────────────────────────────

    fn prim_type(&mut self, p: Prim, sp: Span) -> Ty {
        use Prim::*;
        let (s, i) = (con("string"), con("int"));
        match p {
            Table => fun(s.clone(), fun(s, query(self.fresh()))),
            Where => {
                let (pred, r) = (self.fresh(), self.fresh());
                self.pending.push((
                    Cons::Filter {
                        pred: pred.clone(),
                        row: r.clone(),
                    },
                    sp,
                ));
                fun(pred, fun(query(r.clone()), query(r)))
            }
            Select | AggStage => {
                let (f, a, b) = (self.fresh(), self.fresh(), self.fresh());
                let c = Cons::Project {
                    fields: f.clone(),
                    input: a.clone(),
                    output: b.clone(),
                    agg: p == AggStage,
                };
                self.pending.push((c, sp));
                fun(f, fun(query(a), query(b)))
            }
            Order => {
                let (req, r) = (self.fresh(), self.fresh());
                self.pending.push((
                    Cons::Within {
                        req: req.clone(),
                        row: r.clone(),
                    },
                    sp,
                ));
                fun(list(sortkey(req)), fun(query(r.clone()), query(r)))
            }
            Limit | Offset => {
                let r = self.fresh();
                fun(i, fun(query(r.clone()), query(r)))
            }
            Join(kind) => {
                let nullable = match kind {
                    JoinKind::Inner => (false, false),
                    JoinKind::Left => (false, true),
                    JoinKind::Right => (true, false),
                    JoinKind::Full => (true, true),
                };
                let (l, r, pred, out) = (self.fresh(), self.fresh(), self.fresh(), self.fresh());
                self.pending.push((
                    Cons::JoinOn {
                        pred: pred.clone(),
                        left: l.clone(),
                        right: r.clone(),
                    },
                    sp,
                ));
                let c = Cons::JoinOut {
                    left: l.clone(),
                    right: r.clone(),
                    out: out.clone(),
                    nullable,
                };
                self.pending.push((c, sp));
                fun(query(r), fun(pred, fun(query(l), query(out))))
            }
            Group => {
                let (r, a) = (self.fresh(), self.fresh());
                fun(
                    expr(con("row"), r.clone(), a.clone()),
                    expr(con("agg"), r, a),
                )
            }
            Asc | Desc => {
                let r = self.fresh();
                fun(sortkey(r.clone()), sortkey(r))
            }
            KeyMap => {
                let (mp, a, b) = (self.fresh(), self.fresh(), self.fresh());
                self.pending.push((
                    Cons::KeyMap {
                        mapper: mp.clone(),
                        input: a.clone(),
                        output: b.clone(),
                    },
                    sp,
                ));
                fun(mp, fun(query(a), query(b)))
            }
            KeepOnly | DropKeys | Replace => {
                let tag = match p {
                    KeepOnly => "only",
                    DropKeys => "drop",
                    _ => "replace",
                };
                let arg = self.fresh();
                fun(arg.clone(), mapper(Ty::Con(tag, vec![arg])))
            }
            Prefix | Suffix => fun(s, mapper(con("opaque"))),
            Rows => fun(con("bound"), fun(con("bound"), con("frame"))),
            UnboundedPreceding | UnboundedFollowing | CurrentRow => con("bound"),
            Preceding | Following => fun(i, con("bound")),
        }
    }

    // ── deferred constraints ───────────────────────────────────────────────

    /// Solve pending constraints until no more progress; unsolved ones stay
    /// pending (and become part of the enclosing definition's scheme).
    fn solve(&mut self) -> R<()> {
        loop {
            let mut progress = false;
            let mut keep = Vec::new();
            for (c, sp) in std::mem::take(&mut self.pending) {
                match self.step(&c, sp) {
                    Ok(true) => progress = true,
                    Ok(false) => keep.push((c, sp)),
                    Err(msg) => {
                        self.pending = keep;
                        return Err(TyErr { span: sp, msg });
                    }
                }
            }
            // Resolving an overload may have added the candidate's constraints.
            keep.append(&mut self.pending);
            self.pending = keep;
            if !progress {
                return Ok(());
            }
        }
    }

    /// `Ok(true)` when solved, `Ok(false)` when it must wait.
    fn step(&mut self, c: &Cons, sp: Span) -> Result<bool, String> {
        match c {
            Cons::Overload {
                name,
                module,
                cands,
                target,
                origin,
            } => {
                let fits = self.fitting(*module, cands, target);
                match fits.as_slice() {
                    [] => {
                        let shown: Vec<String> =
                            cands.iter().map(|&i| self.cand_shown(*module, i)).collect();
                        Err(format!(
                            "no overload of `{}` matches {}; candidates: {}",
                            op_name(name),
                            self.show(target),
                            shown.join(", ")
                        ))
                    }
                    [i] => {
                        let s = self.cand_scheme(*module, *i);
                        if s.failed {
                            return Err(self.failed_use(*module, *i));
                        }
                        let t = self.instantiate(&s, sp, None)?;
                        self.unify(&t, target)?;
                        let Origin::Site(site) = *origin else {
                            return Err(format!(
                                "internal error: unresolved hole of `{}`",
                                op_name(name)
                            ));
                        };
                        let (use_site, k) = decode(site);
                        let key = *self.active.last().expect("solving inside a definition");
                        self.record(key, use_site, k, Choice::Def(*module, *i));
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            Cons::Filter { pred, row } => match self.resolve(pred) {
                Ty::Var(_) => Ok(false),
                Ty::Con("bool", _) => Ok(true),
                Ty::Con("expr", a) => {
                    let (p, r, v) = (a[0].clone(), a[1].clone(), a[2].clone());
                    if is_join(&self.resolve(&r)) {
                        return Err(rules::JOIN_ONLY.into());
                    }
                    self.place(Place::Where, &p)?;
                    if matches!(self.resolve(row), Ty::Var(_)) {
                        return Ok(false);
                    }
                    self.unify(row, &r)?;
                    self.unify(&v, &con("bool")).map_err(|_| {
                        format!("`where` needs a bool condition, found {}", self.show(&v))
                    })?;
                    Ok(true)
                }
                o => Err(format!(
                    "`where` needs a bool condition, found {}",
                    self.show(&o)
                )),
            },
            Cons::Project {
                fields,
                input,
                output,
                agg,
            } => {
                let stage = if *agg { "agg" } else { "select" };
                let (fs, tail) = self.flatten(fields);
                match tail {
                    Ty::Empty => {}
                    Ty::Var(_) if fs.is_empty() => return Ok(false),
                    _ => {
                        let msg = format!(
                            "`{stage}` expects a record of column expressions, found {}",
                            self.show(fields)
                        );
                        return Err(msg);
                    }
                }
                if fs.is_empty() {
                    return Err(format!("`{stage}` needs at least one field"));
                }
                if fs
                    .iter()
                    .any(|(_, t)| matches!(self.resolve(t), Ty::Var(_)))
                {
                    return Ok(false);
                }
                let mut out = Vec::new();
                for (l, t) in fs {
                    match self.resolve(&t) {
                        Ty::Con(s, sa) if sa.is_empty() && SCALARS.contains(&s) => out.push((l, t)),
                        Ty::Con("expr", a) => {
                            let (p, r, v) = (a[0].clone(), a[1].clone(), a[2].clone());
                            if is_join(&self.resolve(&r)) {
                                return Err(format!("field `{l}`: {}", rules::JOIN_ONLY));
                            }
                            self.stage_phase(&p, *agg)
                                .map_err(|m| format!("field `{l}` {m}"))?;
                            self.unify(input, &r)
                                .map_err(|m| format!("field `{l}`: {m}"))?;
                            out.push((l, v));
                        }
                        o => {
                            let msg = format!("field `{l}` of `{stage}` must be a column expression or constant, found {}", self.show(&o));
                            return Err(msg);
                        }
                    }
                }
                self.unify(&row(out, Ty::Empty), output)?;
                Ok(true)
            }
            Cons::JoinOn { pred, left, right } => {
                let (p, r, v) = match self.resolve(pred) {
                    Ty::Var(_) => return Ok(false),
                    Ty::Con("bool", _) => return Ok(true),
                    Ty::Con("expr", a) => (a[0].clone(), a[1].clone(), a[2].clone()),
                    o => {
                        return Err(format!(
                            "a join predicate must be a bool expression, found {}",
                            self.show(&o)
                        ))
                    }
                };
                self.place(Place::JoinOn, &p)?;
                match self.resolve(&r) {
                    Ty::Con("join", sides) => {
                        if [left, right]
                            .iter()
                            .any(|t| matches!(self.resolve(t), Ty::Var(_)))
                        {
                            return Ok(false);
                        }
                        let (sl, sr) = (sides[0].clone(), sides[1].clone());
                        self.unify(left, &sl)
                            .map_err(|m| format!("left join input: {m}"))?;
                        self.unify(right, &sr)
                            .map_err(|m| format!("right join input: {m}"))?;
                    }
                    Ty::Var(_) => {
                        self.unify(&r, &Ty::Con("join", vec![left.clone(), right.clone()]))?
                    }
                    o => {
                        let (fs, _) = self.flatten(&o);
                        let n = fs.first().map_or("x", |(k, _)| k.as_str());
                        return Err(rules::needs_side(n));
                    }
                }
                self.unify(&v, &con("bool")).map_err(|_| {
                    format!("a join predicate must be bool, found {}", self.show(&v))
                })?;
                Ok(true)
            }
            Cons::Lit { lit, target } => match self.resolve(target) {
                Ty::Var(_) => Ok(false),
                _ => self.lift(lit, target).map(|_| true),
            },
            Cons::Within { req, row } => match self.resolve(row) {
                Ty::Var(_) => Ok(false),
                _ => self.unify(row, req).map(|_| true),
            },
            Cons::KeyMap {
                mapper,
                input,
                output,
            } => {
                let km = match self.key_mapper(mapper)? {
                    Some(Some(km)) => km,
                    // Waiting for the mapper.
                    None => return Ok(false),
                    // Not static (`prefix`, a computed list): the IR
                    // validator checks it; the output stays unconstrained.
                    Some(None) => return Ok(true),
                };
                let (fs, tail) = self.flatten(input);
                if matches!(tail, Ty::Var(_)) {
                    return Ok(false);
                }
                let names: Vec<String> = fs.iter().map(|(k, _)| k.clone()).collect();
                let cols = km
                    .apply(&names)?
                    .into_iter()
                    .map(|(old, new)| {
                        let t = fs.iter().find(|(k, _)| *k == old).map(|(_, t)| t.clone());
                        (new, t.expect("key mapper output comes from the input"))
                    })
                    .collect();
                let tail = if matches!(km, KeyMapper::Only(_)) {
                    Ty::Empty
                } else {
                    tail
                };
                self.unify(&row_or_tail(cols, tail), output)?;
                Ok(true)
            }
            Cons::JoinOut {
                left,
                right,
                out,
                nullable,
            } => {
                let (lf, lt) = self.flatten(left);
                let (rf, rt) = self.flatten(right);
                if !matches!(lt, Ty::Empty) || !matches!(rt, Ty::Empty) {
                    // Wait for both inputs; with a rigid tail the output
                    // columns are not statically known.
                    return Ok(false);
                }
                // The far side of an outer join may be missing: its columns
                // become `maybe` (once; `maybe (maybe a)` is `maybe a`). That
                // needs to know which already are: wait for a column whose
                // type may still turn out to be a `maybe`.
                let open = |t: &Ty| match self.resolve(t) {
                    Ty::Var(v) => !self.vars[v as usize].nonnull,
                    _ => false,
                };
                if (nullable.0 && lf.iter().any(|(_, t)| open(t)))
                    || (nullable.1 && rf.iter().any(|(_, t)| open(t)))
                {
                    return Ok(false);
                }
                let wrap = |this: &Self, on: bool, (k, t): (String, Ty)| match this.resolve(&t) {
                    Ty::Con("maybe", _) => (k, t),
                    _ if on => (k, Ty::Con("maybe", vec![t])),
                    _ => (k, t),
                };
                let lf: Vec<_> = lf.into_iter().map(|c| wrap(self, nullable.0, c)).collect();
                let rf: Vec<_> = rf.into_iter().map(|c| wrap(self, nullable.1, c)).collect();
                self.unify(&row(rules::join_columns(&lf, &rf), Ty::Empty), out)?;
                Ok(true)
            }
        }
    }

    /// The static key mapper of a `mapper` type: `None` = not known yet,
    /// `Some(None)` = known but not static.
    fn key_mapper(&self, t: &Ty) -> Result<Option<Option<KeyMapper>>, String> {
        let payload = match self.resolve(t) {
            Ty::Var(_) => return Ok(None),
            Ty::Con("mapper", a) => self.resolve(&a[0]),
            o => {
                return Err(format!(
                    "`keyMap` expects a key mapper such as `only [..]`, found {}",
                    self.show(&o)
                ))
            }
        };
        let (tag, arg) = match &payload {
            Ty::Var(_) => return Ok(None),
            Ty::Con("opaque", _) => return Ok(Some(None)),
            Ty::Con(tag, a) => (*tag, self.resolve(&a[0])),
            o => return Err(format!("not a key mapper: {}", self.show(o))),
        };
        Ok(match (tag, arg) {
            (_, Ty::Var(_)) => None,
            ("only", Ty::Labels(ls)) => Some(Some(KeyMapper::Only(ls))),
            ("drop", Ty::Labels(ls)) => Some(Some(KeyMapper::Drop(ls))),
            ("only" | "drop", Ty::Con("list", _)) => Some(None),
            ("replace", Ty::Renames(ps)) => Some(Some(KeyMapper::Replace(ps))),
            ("replace", Ty::Empty) => Some(Some(KeyMapper::Replace(vec![]))),
            ("replace", Ty::Row(..)) => Some(None),
            ("only" | "drop", o) => {
                return Err(format!(
                    "`{tag}` expects a list of column names, found {}",
                    self.show(&o)
                ))
            }
            (_, o) => {
                return Err(format!(
                    "`replace` expects {{ old = \"new\" }}, found {}",
                    self.show(&o)
                ))
            }
        })
    }

    /// Phase rules for one `select` / `agg` field (messages follow the label).
    fn stage_phase(&mut self, p: &Ty, agg: bool) -> U {
        let at = if agg { Place::Agg } else { Place::Select };
        match self.resolve(p) {
            t @ Ty::Con(..) => match phase_of(&t) {
                Some(ph) => rules::place(at, ph),
                None => Err(format!("not a phase: {}", self.show(&t))),
            },
            // Still open, e.g. a helper's parameter (`e => select { x = e }`):
            // a `select` field may become a row or window expression, never
            // an aggregate, so its uses are checked too.
            Ty::Var(v) if !agg => {
                if !self.vars[v as usize].row_or_win {
                    self.trail.push((v, self.vars[v as usize].clone()));
                    self.vars[v as usize].row_or_win = true;
                }
                Ok(())
            }
            Ty::Var(v) if self.vars[v as usize].row_or_win => rules::place(at, Phase::Row),
            _ => self.unify(p, &con("agg")),
        }
    }

    /// Phase rules for a `where` condition, join predicate, or key: row (or
    /// constant). An open phase becomes `row`.
    fn place(&mut self, at: Place, p: &Ty) -> U {
        match phase_of(&self.resolve(p)) {
            Some(ph) => rules::place(at, ph),
            None => self.unify(p, &con("row")),
        }
    }

    // ── printing ───────────────────────────────────────────────────────────

    fn show(&self, t: &Ty) -> String {
        let t = self.zonk(t);
        Printer::default().ty(&t, 0)
    }

    fn show_scheme(&self, s: &Scheme) -> String {
        let mut p = Printer {
            gens: s.gens.iter().map(|g| g.name.clone()).collect(),
            ..Printer::default()
        };
        p.ty(&s.ty, 0)
    }
}

fn lit_name(l: &ast::Lit) -> &'static str {
    match l {
        ast::Lit::Int(_) => "int",
        ast::Lit::Float(_) => "float",
        ast::Lit::Str(_) => "string",
        ast::Lit::Bool(_) => "bool",
    }
}

/// `_+_` → `+` in messages.
fn op_name(n: &str) -> &str {
    match n.strip_prefix('_').and_then(|n| n.strip_suffix('_')) {
        Some(op) if !op.is_empty() => op,
        _ => n,
    }
}

/// An overload's origin names the use site and which hole of the used
/// definition it is (0 for a direct use of an overload set). `site` and the
/// hole index share one `u32`, so both are bounded.
fn encode(site: u32, k: usize) -> Option<u32> {
    if k >= 256 || site >= (1 << 23) {
        return None;
    }
    Some((site << 8) | k as u32 | (1 << 31))
}

fn decode(origin: u32) -> (u32, usize) {
    if origin & (1 << 31) != 0 {
        ((origin & !(1 << 31)) >> 8, (origin & 0xff) as usize)
    } else {
        (origin, 0)
    }
}

fn is_join(t: &Ty) -> bool {
    matches!(t, Ty::Con("join", _))
}

#[derive(Default)]
struct Printer {
    names: HashMap<u32, String>,
    /// Signature names of a scheme's `Gen` variables; others get fresh names.
    gens: Vec<Option<String>>,
    gen_names: HashMap<u32, String>,
}

impl Printer {
    fn var(&mut self, v: u32) -> String {
        let n = self.names.len() + self.gen_names.len();
        self.names.entry(v).or_insert_with(|| var_name(n)).clone()
    }

    /// `prec`: 0 = top, 1 = function argument, 2 = constructor argument.
    fn ty(&mut self, t: &Ty, prec: u8) -> String {
        let paren = |s: String, need: bool| if need { format!("({s})") } else { s };
        match t {
            Ty::Var(v) => self.var(*v),
            Ty::Rigid(_, n) => n.clone(),
            Ty::Gen(k) => match self.gens.get(*k as usize).cloned().flatten() {
                Some(n) => n,
                None => {
                    let n = self.names.len() + self.gen_names.len();
                    self.gen_names
                        .entry(*k)
                        .or_insert_with(|| var_name(n))
                        .clone()
                }
            },
            Ty::Empty => "{}".into(),
            Ty::Labels(ls) => {
                let ls: Vec<String> = ls.iter().map(|l| format!("{l:?}")).collect();
                format!("[{}]", ls.join(", "))
            }
            Ty::Renames(ps) => {
                let ps: Vec<String> = ps.iter().map(|(k, v)| format!("{k} = {v:?}")).collect();
                format!("{{ {} }}", ps.join(", "))
            }
            Ty::Row(fs, tail) => {
                let body: Vec<String> = fs
                    .iter()
                    .map(|(k, v)| format!("{k} = {}", self.ty(v, 0)))
                    .collect();
                match &**tail {
                    Ty::Empty => format!("{{ {} }}", body.join(", ")),
                    t => format!("{{ {} | {} }}", body.join(", "), self.ty(t, 0)),
                }
            }
            Ty::Fun(a, b) => {
                let s = format!("{} -> {}", self.ty(a, 1), self.ty(b, 0));
                paren(s, prec >= 1)
            }
            Ty::Con("expr", a) => {
                let inner = format!("expr {} {}", self.ty(&a[1], 2), self.ty(&a[2], 2));
                match &a[0] {
                    Ty::Con(p @ ("agg" | "win"), _) => paren(format!("{p} ({inner})"), prec >= 2),
                    _ => paren(inner, prec >= 2),
                }
            }
            Ty::Con(n, args) if args.is_empty() => n.to_string(),
            Ty::Con(n, args) => {
                let args: Vec<String> = args.iter().map(|a| self.ty(a, 2)).collect();
                paren(format!("{n} {}", args.join(" ")), prec >= 2)
            }
        }
    }
}

fn var_name(n: usize) -> String {
    if n < 26 {
        ((b'a' + n as u8) as char).to_string()
    } else {
        format!("t{n}")
    }
}

#[cfg(test)]
mod tests;
