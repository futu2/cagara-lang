//! Type checker: Hindley–Milner inference with extensible (Rémy-style) rows,
//! run over the source AST.
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
//! - Selecting columns is `select`; `{a = .a, b = .b}` is spelled `{.a, .b}`.
//!   `update` merges a field record over the input row, so a name the input
//!   has keeps its position and a new name is appended.
//!
//! - Overloading: a name defined more than once (each with a signature) is an
//!   overload set. Each use gets the candidates' common shape and an
//!   `Overload` constraint, resolved by trial unification once exactly one
//!   candidate fits. A definition whose overloads stay open (`x => x + x`)
//!   keeps them as holes in its scheme; each use fills them, and the
//!   source elaboration follows the recorded choices (dictionary passing,
//!   resolved at compile time). In a query definition, leftover literals default to
//!   their own type before overloads are forced.
//!
//! - Nullability is explicit: `maybe a` is a type, and type variables in
//!   signatures stand for non-null types, so `expr r a` and
//!   `expr r (maybe a)` are disjoint. Operators and aggregate/window inputs
//!   need non-null arguments (`coalesce` handles nulls); outer
//!   joins make the far side's columns `maybe`; aggregate/window results may
//!   still return `maybe`.
//!
//! Row operations keep column names and wrappers in first-order row terms.
//! `omit` is solved as a row equation; `prefix`, `suffix`, and `mapValue`
//! reduce `keyMap` / `mapValue` terms when their inputs are known. The same
//! schema helpers are used by IR validation (see `docs/ROW-TYPES.md`).
//!
//! The implementation is split by concern; see each module for its part:
//! [`ty`], [`reduce`], [`unify`], [`overload`], [`infer`], [`print`].

mod facts;
mod infer;
mod overload;
mod print;
mod reduce;
mod ty;
mod unify;

use std::sync::Arc;

// The type layer, the row-term reductions, and the `Checker` struct itself
// are shared by every sibling module. Re-exporting them here lets each module
// reach them with a single `use super::*;`, exactly as the pre-split single file
// reached every item with no qualification.
pub(crate) use reduce::*;
pub(crate) use ty::*;

pub(crate) use facts::{definition_facts, DefinitionFacts};
pub use ty::Choice;
/// What shape a definition's scheme has.
///
/// Public because [`TypeCheck::scheme_view`] returns it and a caller outside
/// the crate has to be able to match on the result; a public method returning
/// a crate-private type is unusable from outside. `Ty` itself stays
/// crate-private — inference state is not a public interface, but the
/// *classification* of a checked scheme is data a phase can rely on.
pub use ty::SchemeView;
/// Scalar-type views of a `Ty`, for the checked layer (`crate::checked`).
pub(crate) use ty::{scheme_result_expr, scheme_result_scalar, scheme_view};

pub(crate) use crate::core::ScalarType;
pub(crate) use crate::db::ModuleInput;
pub(crate) use crate::ir::{JoinKind, Phase};
pub(crate) use crate::lower::parse_module;
pub(crate) use crate::primitive::Prim;
pub(crate) use crate::resolve::{module_own, module_scope};
pub(crate) use crate::rules::{self};
pub(crate) use crate::workspace::{Binding, Diag, Workspace};
pub(crate) use cagara_syntax::ast::{self, ExprKind, Side, Span, TypeExpr};
pub(crate) use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub struct TypeError {
    pub module: usize,
    pub def: usize,
    pub diag: Diag,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawTypeError {
    module: usize,
    def: usize,
    span: Span,
    message: String,
}

/// Result of checking every definition in a workspace.
pub struct TypeCheck {
    errors: Vec<TypeError>,
    /// First index into `errors` for each `(module, def)`.
    ///
    /// Elaboration asks `error_for(module, def)` once per definition, so
    /// scanning `errors` there made an error-heavy file quadratic in the
    /// number of diagnostics. `errors()` is read-only, so this cannot fall out
    /// of step with the vector.
    by_def: HashMap<(usize, usize), usize>,
    /// Immutable Salsa results shared by compiler and editor requests.
    /// Indices match the workspace; their per-expression maps stay in place.
    modules: Vec<Arc<ModuleCheck>>,
}

/// A column name no user writes (`__` names are reserved). A reference to
/// it adds no column to its row; the checker records the columns the row
/// is known to have instead, for completion.
pub const PROBE_FIELD: &str = "__cagara_complete";

impl TypeCheck {
    /// Every diagnostic produced by checking, in module then definition order.
    pub fn errors(&self) -> &[TypeError] {
        &self.errors
    }

    /// Columns (name, printed type) of the row seen by the `PROBE_FIELD`
    /// reference in `module`, if it has one and its definition got that far.
    pub fn probe_fields(&self, module: usize) -> Option<&[(String, String)]> {
        self.modules
            .get(module)?
            .probe_fields
            .as_ref()
            .map(|(_, fields)| fields.as_slice())
    }

    /// Printed type scheme of a well-typed definition.
    pub fn type_of(&self, module: usize, def: usize) -> Option<&str> {
        self.modules
            .get(module)?
            .types
            .get(&(module, def))
            .map(String::as_str)
    }

    /// Number of open overloads a definition leaves to its users.
    pub fn holes(&self, module: usize, def: usize) -> usize {
        self.modules
            .get(module)
            .and_then(|m| m.holes.get(&(module, def)))
            .copied()
            .unwrap_or(0)
    }

    /// Choice for hole `k` at use site `site` inside definition `(module, def)`.
    ///
    /// `site` is an [`ExprId`](cagara_syntax::ast::ExprId) — the `id` of the
    /// *use expression* — not a span offset. Both are small integers, so a
    /// caller that passes a byte offset gets `None` at best and, when the two
    /// happen to collide, **another use's choice**. Prefer
    /// [`TypeCheck::choices_of`], which takes the pairs the checker actually
    /// recorded and cannot be called with the wrong key.
    pub fn choice(&self, module: usize, def: usize, site: u32, k: usize) -> Option<Choice> {
        self.modules
            .get(module)?
            .choices
            .get(&(module, def))?
            .get(&(site, k))
            .copied()
    }

    /// Every resolved overload choice of a definition, as
    /// `(use site, hole, choice)` in a deterministic order.
    ///
    /// `use site` is the `ExprId` of the use expression. This is the accessor
    /// for a phase that wants *all* the choices of a definition and has no way
    /// to know the site ids in advance; it reads the same map
    /// [`TypeCheck::choice`] does, so the two cannot disagree.
    pub fn choices_of(&self, module: usize, def: usize) -> Vec<(u32, usize, Choice)> {
        let Some(by_site) = self
            .modules
            .get(module)
            .and_then(|m| m.choices.get(&(module, def)))
        else {
            return Vec::new();
        };
        // HashMap iteration order is not stable, so sort: a phase that records
        // these in a `Vec` needs the same answer on every run, and `ExprId`s
        // are assigned in source order, which makes this a reading order.
        let mut out: Vec<(u32, usize, Choice)> = by_site
            .iter()
            .map(|(&(site, k), &c)| (site, k, c))
            .collect();
        out.sort_by_key(|&(site, k, _)| (site, k));
        out
    }

    /// Printed type of the name use whose expression spans `span` in `module`.
    pub fn use_type(&self, module: usize, span: Span) -> Option<&str> {
        self.modules
            .get(module)?
            .use_types
            .get(&(module, span.start, span.end))
            .map(String::as_str)
    }

    /// The scalar type the checker gave the expression with this `ExprId`, as
    /// closed data.
    ///
    /// This is the fact a **source-level elaboration** needs: walking the AST,
    /// it asks "what type did this node get?" for each expression it turns into
    /// a `CheckedExpr`. The span-keyed [`TypeCheck::use_type`] cannot answer
    /// that — it returns a *printed* string and is keyed by location rather
    /// than by the node.
    ///
    /// `None` when the checker recorded no type for that id, or recorded one
    /// that is not a scalar (a `Query`, a `Function`, an open row).
    /// `Some(Unknown)` when it is a scalar the checker never solved. The
    /// distinction matches [`TypeCheck::def_scalar`], and for the same reason.
    pub fn use_ty(&self, module: usize, id: u32) -> Option<ScalarType> {
        let t = self.modules.get(module)?.use_tys.get(&(module, id))?;
        match scheme_view(t) {
            SchemeView::Scalar(s) => Some(s),
            SchemeView::Open => Some(ScalarType::Unknown),
            _ => None,
        }
    }

    /// The `ExprId`s the checker recorded a type for in `module`, ascending.
    ///
    /// For a caller that wants to know what is available before walking.
    pub fn use_ids(&self, module: usize) -> Vec<u32> {
        let Some(checked) = self.modules.get(module) else {
            return Vec::new();
        };
        let mut out: Vec<u32> = checked
            .use_tys
            .keys()
            .filter(|(m, _)| *m == module)
            .map(|(_, id)| *id)
            .collect();
        out.sort_unstable();
        out
    }

    /// The row of a definition's scheme, as closed data: column names and
    /// [`ScalarType`]s in declaration order.
    ///
    /// This is the typed twin of [`TypeCheck::type_of`]. A phase that needs a
    /// definition's columns reads them here instead of parsing the printed
    /// string.
    ///
    /// The columns of a definition's *closed* query row, or `None` when the
    /// scheme is not a closed query.
    ///
    /// `None` covers every other shape — a function, a scalar, a bare row, a
    /// `query r` with an open row — because a caller asking for columns wants
    /// to know whether there are any, and the shapes differ in more ways than
    /// the answer. Use [`TypeCheck::scheme_view`] to tell *which* shape it is.
    pub fn scheme_fields(&self, module: usize, def: usize) -> Option<Vec<(String, ScalarType)>> {
        let s = self.modules.get(module)?.schemes.get(&(module, def))?;
        match scheme_view(&s.ty) {
            SchemeView::Query { fields } => fields,
            SchemeView::Row { fields } => Some(fields),
            _ => None,
        }
    }

    /// What shape a definition's scheme has, structurally.
    ///
    /// This is the accessor to branch on. [`TypeCheck::scheme_fields`] and
    /// [`TypeCheck::def_scalar`] both collapse several shapes into `None`, so
    /// neither can distinguish "a function taking a query" from "a query";
    /// this can, because it is computed from the type rather than from the
    /// shape of a printed string.
    /// The phase and scalar type a definition *returns*, for a call site. See
    /// `check::ty::scheme_result_expr`.
    pub fn result_expr(&self, module: usize, def: usize) -> Option<(Phase, ScalarType)> {
        let s = self.modules.get(module)?.schemes.get(&(module, def))?;
        scheme_result_expr(&s.ty)
    }

    /// The scalar type a definition *returns*, with arguments stripped and any
    /// `expr r a` unwrapped to `a`.
    ///
    /// This is what a call site needs. `TypeCheck::use_ty` answers for a leaf
    /// expression; an application has no recorded entry, so its type comes from
    /// the callee signature.
    pub fn result_scalar(&self, module: usize, def: usize) -> Option<ScalarType> {
        let s = self.modules.get(module)?.schemes.get(&(module, def))?;
        scheme_result_scalar(&s.ty)
    }

    pub fn scheme_view(&self, module: usize, def: usize) -> Option<SchemeView> {
        let s = self.modules.get(module)?.schemes.get(&(module, def))?;
        Some(scheme_view(&s.ty))
    }

    /// A definition's scalar type, as closed data.
    ///
    /// `None` for a scheme that is not a scalar — a function, a row, or a
    /// query — and `Some(Unknown)` for a scalar the checker never solved. The
    /// two used to be one case, because `ty_to_scalar` maps every non-scalar
    /// to `Unknown`; this reads the same structural view
    /// [`TypeCheck::scheme_view`] exposes, so a function is now `None` rather
    /// than a misleading `Some(Unknown)`.
    pub fn def_scalar(&self, module: usize, def: usize) -> Option<ScalarType> {
        let s = self.modules.get(module)?.schemes.get(&(module, def))?;
        match scheme_view(&s.ty) {
            SchemeView::Scalar(t) => Some(t),
            SchemeView::Open => Some(ScalarType::Unknown),
            SchemeView::Query { .. } | SchemeView::Row { .. } | SchemeView::Function => None,
        }
    }

    pub fn error_for(&self, module: usize, def: usize) -> Option<&Diag> {
        let index = *self.by_def.get(&(module, def))?;
        self.errors.get(index).map(|e| &e.diag)
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
        by_def: HashMap::new(),
        modules: Vec::with_capacity(ws.inputs.len()),
    };
    for &input in &ws.inputs {
        let mc = module_check(&ws.db, input);
        for e in mc.errors.iter() {
            // One entry per definition: the checker reports at most one error
            // for a definition, and the first is the one elaboration should see.
            out.by_def
                .entry((e.module, e.def))
                .or_insert(out.errors.len());
            out.errors.push(e.clone());
        }
        out.modules.push(Arc::clone(mc));
    }
    out
}

/// Check one module (memoized). Dependencies are checked first; only their
/// self-contained schemes are used.
#[salsa::tracked(returns(ref))]
fn module_check(db: &dyn salsa::Database, input: ModuleInput) -> Arc<ModuleCheck> {
    #[cfg(test)]
    CHECK_RUNS.with(|c| c.set(c.get() + 1));
    let imported: Vec<ModuleInput> = input
        .prelude(db)
        .iter()
        .copied()
        .chain(input.imports(db).iter().map(|(_, t)| *t))
        .collect();
    let file = *input.file(db);
    let parsed = parse_module(db, file);
    let env = ModuleEnv {
        module: *input.index(db),
        defs: &parsed.module.defs,
        scope: module_scope(db, input),
        owns: imported
            .iter()
            .map(|t| (*t.index(db), module_own(db, *t).as_ref()))
            .collect(),
        schemes: imported
            .iter()
            .map(|t| (*t.index(db), &module_check(db, *t).schemes))
            .collect(),
    };
    let out = check_module(env);
    // Render the diagnostics here, once per module text, rather than in every
    // `check()`: an editor calls `check()` on each request, and re-rendering
    // every message against the text each time was the cost of a request on an
    // error-heavy file. The salsa query already depends on the text, so a
    // rendered diagnostic is invalidated exactly when its span would be.
    let path = input.path(db);
    let text = file.text(db);
    let starts = crate::workspace::line_starts(text);
    let errors = out
        .errors
        .iter()
        .map(|e| TypeError {
            module: e.module,
            def: e.def,
            diag: crate::workspace::make_diag_indexed(
                path,
                text,
                &starts,
                e.span.start as usize,
                e.span.end as usize,
                e.message.clone(),
            ),
        })
        .collect();
    Arc::new(ModuleCheck {
        schemes: out.schemes,
        errors,
        types: out.types,
        holes: out.holes,
        choices: out.choices,
        probe_fields: out.probe_fields,
        use_tys: out.use_tys,
        use_types: out.use_types,
    })
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

/// What one module's check produced, before its diagnostics are rendered.
///
/// Rendering needs the module's path and text, which the pure checker does not
/// have; [`module_check`] converts this into a [`ModuleCheck`].
#[derive(Debug, Clone, PartialEq)]
struct ModuleOutput {
    schemes: HashMap<(usize, usize), Scheme>,
    errors: Vec<RawTypeError>,
    types: HashMap<(usize, usize), String>,
    holes: HashMap<(usize, usize), usize>,
    choices: HashMap<(usize, usize), HashMap<(u32, usize), Choice>>,
    probe_fields: Option<(usize, Vec<(String, String)>)>,
    use_tys: HashMap<(usize, u32), Ty>,
    use_types: HashMap<(usize, u32, u32), String>,
}

/// A checked module with its diagnostics already rendered.
///
/// Rendered once per module text, inside the salsa query, so a request that
/// only needs the diagnostics does not re-render them.
#[derive(Debug, Clone, PartialEq)]
struct ModuleCheck {
    schemes: HashMap<(usize, usize), Scheme>,
    errors: Vec<TypeError>,
    types: HashMap<(usize, usize), String>,
    holes: HashMap<(usize, usize), usize>,
    choices: HashMap<(usize, usize), HashMap<(u32, usize), Choice>>,
    probe_fields: Option<(usize, Vec<(String, String)>)>,
    use_tys: HashMap<(usize, u32), Ty>,
    use_types: HashMap<(usize, u32, u32), String>,
}

/// What checking one module reads: its definitions and scope, and every
/// module's exports (for `alias.name`). Nothing else of the workspace is
/// needed, so a salsa query can build this view.
pub(crate) struct ModuleEnv<'w> {
    pub(crate) module: usize,
    pub(crate) defs: &'w [ast::Def],
    pub(crate) scope: &'w HashMap<String, Binding>,
    /// Exports of the modules it imports, by module index.
    pub(crate) owns: HashMap<usize, &'w HashMap<String, Binding>>,
    /// Borrowed checked exports; looking up one name never copies a catalog.
    pub(crate) schemes: HashMap<usize, &'w HashMap<(usize, usize), Scheme>>,
}

/// Check one module against the schemes of the modules it may use.
fn check_module(env: ModuleEnv<'_>) -> ModuleOutput {
    let m = env.module;
    let mut c = Checker {
        env,
        module: m,
        vars: Vec::new(),
        schemes: HashMap::new(),
        failed: HashSet::new(),
        active: Vec::new(),
        depth: 0,
        depth_reported: false,
        pending: Vec::new(),
        errors: Vec::new(),
        span: Span::default(),
        trail: Vec::new(),
        fit_cache: HashMap::new(),
        holes: HashMap::new(),
        choices: HashMap::new(),
        probe: None,
        probe_fields: None,
        uses: Vec::new(),
        use_types: HashMap::new(),
        use_tys: HashMap::new(),
        deferred_keys: Vec::new(),
        affix_mappers: Vec::new(),
        unify_depth: 0,
    };
    for i in 0..c.env.defs.len() {
        c.def_scheme(m, i);
    }
    let types = c
        .schemes
        .iter()
        .filter(|(k, _)| !c.failed.contains(k))
        .map(|(k, s)| (*k, c.show_scheme(s)))
        .collect();
    let probe_fields = c.probe_fields.map(|fs| (m, fs));
    let use_types = c.use_types;
    let use_tys = c.use_tys;
    ModuleOutput {
        schemes: c.schemes,
        errors: c.errors,
        types,
        holes: c.holes,
        choices: c.choices,
        probe_fields,
        use_types,
        use_tys,
    }
}

#[cfg(test)]
mod tests;
